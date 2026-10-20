//! Capture pipeline: one thread owns the USB device, everyone else subscribes.
//!
//! A single tuner means the device can only be on one frequency at a time, so
//! `Hub` doubles as the "remote control": anyone can call [`Hub::tune_to`] and
//! the capture thread will retune, wipe the PSI state and bump the epoch so
//! streaming clients on the old mux disconnect instead of consuming garbage.

use anyhow::{bail, Result};
use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

use crate::proto::Status;
use crate::ts::{Aligner, Analyzer, PKT};
use crate::usb::Dtmb;

/// Chunk capacity for slow clients: 256 × ~64 KB ≈ 16 MB.
pub const HUB_CAPACITY: usize = 256;

pub struct Inner {
    pub freq_hz: u32,
    pub status: Status,
    pub analyzer: Analyzer,
    pub resyncs: u64,
    pub bytes_total: u64,
    pub bitrate_mbps: f32,
    pub recording: Option<std::path::PathBuf>,
}

pub struct Hub {
    pub tx: broadcast::Sender<Bytes>,
    pub inner: Mutex<Inner>,
    pub subscribers: AtomicU64,
    /// bumped on every retune; streaming clients compare and bail out
    epoch: AtomicU64,
    tune_tx: mpsc::Sender<u32>,
}

impl Hub {
    pub fn new(tune_tx: mpsc::Sender<u32>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(HUB_CAPACITY);
        Arc::new(Self {
            tx,
            inner: Mutex::new(Inner {
                freq_hz: 0,
                status: Status::default(),
                analyzer: Analyzer::default(),
                resyncs: 0,
                bytes_total: 0,
                bitrate_mbps: 0.0,
                recording: None,
            }),
            subscribers: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            tune_tx,
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Bytes> {
        self.subscribers.fetch_add(1, Ordering::Relaxed);
        self.tx.subscribe()
    }

    /// Ask the capture thread to retune and wait until the new mux is locked.
    pub fn tune_to(&self, freq_hz: u32, timeout: Duration) -> Result<()> {
        if self.inner.lock().unwrap().freq_hz == freq_hz {
            return Ok(());
        }
        self.tune_tx
            .send(freq_hz)
            .map_err(|_| anyhow::anyhow!("capture thread is gone"))?;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            {
                let g = self.inner.lock().unwrap();
                if g.freq_hz == freq_hz && g.status.locked {
                    return Ok(());
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        bail!("timed out waiting for lock on {:.3} MHz", freq_hz as f64 / 1e6)
    }

    pub fn snapshot(&self) -> Snapshot {
        let g = self.inner.lock().unwrap();
        Snapshot {
            freq_hz: g.freq_hz,
            status: g.status,
            programs: g.analyzer.programs.values().cloned().collect(),
            bitrate_mbps: g.bitrate_mbps,
            bytes_total: g.bytes_total,
            resyncs: g.resyncs,
            packets: g.analyzer.packet_count,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub freq_hz: u32,
    pub status: Status,
    pub programs: Vec<crate::ts::Program>,
    pub bitrate_mbps: f32,
    pub bytes_total: u64,
    pub resyncs: u64,
    pub packets: u64,
}

pub struct CaptureConfig {
    pub freq_hz: Option<u32>,
    pub record: Option<std::path::PathBuf>,
    /// Called with human-readable progress.
    pub log: fn(&str),
}

/// Spawn the capture thread. `cmd_rx` receives retune requests.
pub fn spawn(hub: Arc<Hub>, cmd_rx: mpsc::Receiver<u32>, cfg: CaptureConfig) -> Arc<AtomicU64> {
    let running = Arc::new(AtomicU64::new(1));
    let flag = running.clone();
    let log = cfg.log;
    std::thread::Builder::new()
        .name("dtmb-capture".into())
        .spawn(move || {
            if let Err(e) = capture_loop(hub, cmd_rx, cfg, flag) {
                (log)(&format!("capture stopped: {e:#}"));
            }
        })
        .expect("spawn capture thread");
    running
}

fn capture_loop(
    hub: Arc<Hub>,
    cmd_rx: mpsc::Receiver<u32>,
    cfg: CaptureConfig,
    running: Arc<AtomicU64>,
) -> Result<()> {
    let mut dev = Dtmb::open()?;
    (cfg.log)(&format!(
        "device: firmware V{} ({:?}) bus {:03} addr {:03}",
        dev.version.to_string(),
        dev.variant,
        dev.bus,
        dev.address
    ));
    dev.power(true)?;

    let mut freq = match cfg.freq_hz {
        Some(f) => f,
        None => autotune(&mut dev, cfg.log)?.unwrap_or(698_000_000),
    };
    let status = dev.tune(freq, Duration::from_millis(900))?;
    (cfg.log)(&format!(
        "tuned {:.3} MHz lock={} strength={}% snr={:.1} dB",
        freq as f64 / 1e6,
        status.locked as u8,
        status.strength_pct,
        status.snr_db
    ));
    {
        let mut g = hub.inner.lock().unwrap();
        g.freq_hz = freq;
        g.status = status;
        g.recording = cfg.record.clone();
    }
    dev.stream(true)?;

    let mut aligner = Aligner::default();
    let mut file = match &cfg.record {
        Some(p) => Some(std::fs::File::create(p)?),
        None => None,
    };

    let mut buf = vec![0u8; 64 * 1024];
    let mut last_report = Instant::now();
    let mut window_bytes: u64 = 0;

    while running.load(Ordering::Relaxed) == 1 {
        // remote control: retune on request
        while let Ok(target) = cmd_rx.try_recv() {
            if target == freq {
                continue;
            }
            (cfg.log)(&format!("retune {:.3} → {:.3} MHz", freq as f64 / 1e6, target as f64 / 1e6));
            dev.power_cycle();
            let st = dev.tune(target, Duration::from_millis(800))?;
            dev.stream(true)?;
            freq = target;
            // new mux = new PIDs: wipe alignment and PSI state, then kick old clients
            aligner = Aligner::default();
            {
                let mut g = hub.inner.lock().unwrap();
                g.analyzer = Analyzer::default();
                g.freq_hz = freq;
                g.status = st;
                g.resyncs = 0;
            }
            hub.epoch.fetch_add(1, Ordering::Relaxed);
            (cfg.log)(&format!(
                "tuned {:.3} MHz lock={} strength={}% snr={:.1} dB",
                freq as f64 / 1e6,
                st.locked as u8,
                st.strength_pct,
                st.snr_db
            ));
        }

        let n = match dev.read_ts(&mut buf, Duration::from_millis(300)) {
            Ok(n) => n,
            Err(e) => {
                (cfg.log)(&format!("usb read error: {e:#}"));
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        if n == 0 {
            if let Ok(st) = dev.tune(freq, Duration::from_millis(0)) {
                hub.inner.lock().unwrap().status = st;
            }
            continue;
        }

        let mut aligned = Vec::with_capacity(n + PKT);
        aligner.push(&buf[..n], &mut aligned);
        if aligned.is_empty() {
            continue;
        }

        if let Some(f) = file.as_mut() {
            use std::io::Write;
            let _ = f.write_all(&aligned);
        }

        {
            let mut g = hub.inner.lock().unwrap();
            g.analyzer.feed(&aligned);
            g.bytes_total += aligned.len() as u64;
            g.resyncs = aligner.resyncs;
            window_bytes += aligned.len() as u64;
        }
        let _ = hub.tx.send(Bytes::from(aligned));

        if last_report.elapsed() >= Duration::from_secs(2) {
            let secs = last_report.elapsed().as_secs_f32();
            let mbps = window_bytes as f32 * 8.0 / secs / 1e6;
            hub.inner.lock().unwrap().bitrate_mbps = mbps;
            window_bytes = 0;
            last_report = Instant::now();
        }
    }

    dev.close();
    (cfg.log)("capture thread exiting");
    Ok(())
}

fn autotune(dev: &mut Dtmb, log: fn(&str)) -> Result<Option<u32>> {
    use crate::proto::{FREQ_MAX_HZ, FREQ_MIN_HZ, FREQ_STEP_HZ};
    (log)("no --freq given, scanning for the first locked mux");
    let mut f = FREQ_MIN_HZ;
    while f <= FREQ_MAX_HZ {
        let st = dev.tune(f, Duration::from_millis(350))?;
        if st.locked {
            (log)(&format!(
                "autotune: locked at {:.3} MHz ({}% / {:.1} dB)",
                f as f64 / 1e6,
                st.strength_pct,
                st.snr_db
            ));
            return Ok(Some(f));
        }
        f += FREQ_STEP_HZ;
    }
    (log)("autotune: nothing locked — check the antenna");
    Ok(None)
}
