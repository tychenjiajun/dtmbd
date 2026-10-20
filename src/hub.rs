//! Capture pipeline: one thread owns the USB device, everyone else subscribes.

use anyhow::Result;
use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

use crate::proto::{Status, FREQ_STEP_HZ, FREQ_MAX_HZ, FREQ_MIN_HZ};
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
}

impl Hub {
    pub fn new() -> Arc<Self> {
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
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Bytes> {
        self.subscribers.fetch_add(1, Ordering::Relaxed);
        self.tx.subscribe()
    }

    pub fn snapshot(&self) -> (u32, Status, Vec<crate::ts::Program>, f32, u64, u64, u64) {
        let g = self.inner.lock().unwrap();
        (
            g.freq_hz,
            g.status,
            g.analyzer.programs.values().cloned().collect(),
            g.bitrate_mbps,
            g.bytes_total,
            g.resyncs,
            g.analyzer.packet_count,
        )
    }
}

pub struct CaptureConfig {
    pub freq_hz: Option<u32>,
    pub record: Option<std::path::PathBuf>,
    /// Called with human-readable progress.
    pub log: fn(&str),
}

/// Spawn the capture thread. Returns immediately; the hub fills up in the background.
pub fn spawn(hub: Arc<Hub>, cfg: CaptureConfig) -> Arc<AtomicU64> {
    let running = Arc::new(AtomicU64::new(1));
    let flag = running.clone();
    let log = cfg.log;
    std::thread::Builder::new()
        .name("dtmb-capture".into())
        .spawn(move || {
            if let Err(e) = capture_loop(hub, cfg, flag) {
                (log)(&format!("capture stopped: {e:#}"));
            }
        })
        .expect("spawn capture thread");
    running
}

fn capture_loop(hub: Arc<Hub>, cfg: CaptureConfig, running: Arc<AtomicU64>) -> Result<()> {
    let mut dev = Dtmb::open()?;
    (cfg.log)(&format!(
        "device: firmware V{} ({:?}) bus {:03} addr {:03}",
        dev.version.to_string(),
        dev.variant,
        dev.bus,
        dev.address
    ));
    dev.power(true)?;

    let freq = match cfg.freq_hz {
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
        let n = match dev.read_ts(&mut buf, Duration::from_millis(500)) {
            Ok(n) => n,
            Err(e) => {
                (cfg.log)(&format!("usb read error: {e:#}"));
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        if n == 0 {
            // idle: refresh frontend status so the UI shows signal changes
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
            {
                let mut g = hub.inner.lock().unwrap();
                g.bitrate_mbps = mbps;
            }
            window_bytes = 0;
            last_report = Instant::now();
        }
    }

    dev.close();
    (cfg.log)("capture thread exiting");
    Ok(())
}

fn autotune(dev: &mut Dtmb, log: fn(&str)) -> Result<Option<u32>> {
    (log)("no --freq given, scanning the 474–866 MHz raster");
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
