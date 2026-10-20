// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 Jiajun Chen <tychenjiajun@live.cn>

//! L3 — single-tuner service.
//!
//! One stick can only receive one multiplex at a time, so this layer owns the
//! device exclusively and turns "play this service" into "be on that frequency".
//! Everything downstream (HTTP) only reads from the broadcast channel and the
//! status snapshot.
//!
//! Responsibilities:
//!   * lazy tuning — nothing is tuned until a client asks for a service
//!   * zapping — a request for another mux retunes, and clients on the old mux
//!     are dropped via `epoch` so they reconnect instead of consuming garbage
//!   * recovery — empty reads / USB errors trigger a power cycle, a retune, or a
//!     full device reopen (cable replug) with backoff

use bytes::Bytes;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

use crate::ts::{Aligner, Analyzer, Program, PKT};
use crate::usb::proto::Status;
use crate::usb::Device;

/// ~64 KB chunks, 128 slots: slow clients drop old data instead of blocking the tuner.
const CAPACITY: usize = 128;

pub struct Snapshot {
    pub freq_hz: u32,
    pub status: Status,
    pub programs: Vec<Program>,
    pub bitrate_mbps: f32,
    pub bytes_total: u64,
    pub resyncs: u64,
    pub packets: u64,
    pub tuning: bool,
}

struct Inner {
    freq_hz: u32,
    status: Status,
    analyzer: Analyzer,
    bytes_total: u64,
    bitrate_mbps: f32,
    resyncs: u64,
}

enum Cmd {
    Tune {
        hz: u32,
        reply: mpsc::Sender<std::result::Result<(), String>>,
    },
    /// Antenna meter: stop streaming, tune to `hz` (or keep the current probe
    /// frequency when `None`), and report one signal reading.
    Meter {
        hz: Option<u32>,
        reply: mpsc::Sender<std::result::Result<SignalReading, String>>,
    },
    /// Leave meter mode and resume whatever the viewers were watching.
    MeterStop {
        reply: mpsc::Sender<std::result::Result<(), String>>,
    },
}

/// One frontend measurement — what you read while aiming an antenna.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SignalReading {
    pub freq_hz: u32,
    pub locked: bool,
    pub strength_pct: u8,
    pub snr_db: f32,
    pub modulation: &'static str,
    pub guard_interval: &'static str,
    pub fec: &'static str,
}

pub struct Tuner {
    inner: Mutex<Inner>,
    tx: broadcast::Sender<Bytes>,
    epoch: AtomicU64,
    cmd_tx: mpsc::Sender<Cmd>,
    tuning: AtomicBool,
}

impl Tuner {
    pub fn spawn(log: fn(&str)) -> Arc<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (tx, _) = broadcast::channel(CAPACITY);
        let tuner = Arc::new(Self {
            inner: Mutex::new(Inner {
                freq_hz: 0,
                status: Status::default(),
                analyzer: Analyzer::default(),
                bytes_total: 0,
                bitrate_mbps: 0.0,
                resyncs: 0,
            }),
            tx,
            epoch: AtomicU64::new(0),
            cmd_tx,
            tuning: AtomicBool::new(false),
        });

        let worker = tuner.clone();
        std::thread::Builder::new()
            .name("tuner".into())
            .spawn(move || worker.run(cmd_rx, log))
            .expect("spawn tuner thread");

        tuner
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Bytes> {
        self.tx.subscribe()
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
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
            tuning: self.tuning.load(Ordering::Relaxed),
        }
    }

    /// One measurement at `hz` (or the current probe frequency when `None`).
    /// Enters meter mode, which pauses streaming until [`Tuner::meter_stop`].
    pub fn meter(&self, hz: Option<u32>) -> anyhow::Result<SignalReading> {
        let (tx, rx) = mpsc::channel();
        self.cmd_tx
            .send(Cmd::Meter { hz, reply: tx })
            .map_err(|_| anyhow::anyhow!("tuner thread is gone"))?;
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => Err(anyhow::anyhow!(e)),
            Err(_) => Err(anyhow::anyhow!("meter timed out")),
        }
    }

    /// Leave meter mode; viewers reconnect on their next request.
    pub fn meter_stop(&self) {
        let (tx, rx) = mpsc::channel();
        if self.cmd_tx.send(Cmd::MeterStop { reply: tx }).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(4));
        }
    }

    pub fn psi_snapshot(&self) -> Vec<(u16, Vec<u8>)> {
        self.inner.lock().unwrap().analyzer.psi_snapshot()
    }

    /// Blocking: ask the worker to go to `hz` and wait until it is streaming.
    /// Call from `spawn_blocking`, never from an async task directly.
    pub fn tune_to(&self, hz: u32, timeout: Duration) -> anyhow::Result<()> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.cmd_tx
            .send(Cmd::Tune { hz, reply: reply_tx })
            .map_err(|_| anyhow::anyhow!("tuner thread is gone"))?;
        match reply_rx.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow::anyhow!(e)),
            Err(_) => Err(anyhow::anyhow!("timed out waiting for lock")),
        }
    }

    // ------------------------------------------------------------- worker side

    fn run(&self, cmd_rx: mpsc::Receiver<Cmd>, log: fn(&str)) {
        let mut dev: Option<Device> = None;
        let mut want: Option<u32> = None;
        let mut tuned: Option<u32> = None;
        let mut meter: Option<u32> = None;
        let mut aligner = Aligner::default();
        let mut buf = vec![0u8; 64 * 1024];
        let mut empty_streak = 0u32;
        let mut window_bytes = 0u64;
        let mut window_start = Instant::now();
        let mut backoff = Duration::from_millis(200);

        loop {
            // 1. commands. While streaming this must NOT block: every millisecond
            // spent here is time the FX2 FIFO is not drained (9 Mbit/s ≈ 1.1 KB/ms),
            // which used to show up as ~3-10% continuity-counter errors in the TS.
            let cmd = if tuned.is_some() {
                match cmd_rx.try_recv() {
                    Ok(c) => Some(c),
                    Err(mpsc::TryRecvError::Disconnected) => break,
                    Err(mpsc::TryRecvError::Empty) => None,
                }
            } else {
                match cmd_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(c) => Some(c),
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                }
            };
            match cmd {
                Some(Cmd::Tune { hz, reply }) => {
                    want = Some(hz);
                    if tuned == Some(hz) && dev.is_some() {
                        let _ = reply.send(Ok(()));
                        continue;
                    }
                    self.tuning.store(true, Ordering::Relaxed);
                    let res = self.retune(&mut dev, hz, &mut aligner, log);
                    self.tuning.store(false, Ordering::Relaxed);
                    match &res {
                        Ok(()) => {
                            tuned = Some(hz);
                            empty_streak = 0;
                            backoff = Duration::from_millis(200);
                        }
                        Err(e) => {
                            (log)(&format!("tune {:.3} MHz failed: {e:#}", hz as f64 / 1e6));
                            tuned = None;
                            dev = None;
                        }
                    }
                    let _ = reply.send(res.map_err(|e| format!("{e:#}")));
                }
                Some(Cmd::Meter { hz, reply }) => {
                    let res = self.meter_step(&mut dev, &mut meter, &mut tuned, hz);
                    let _ = reply.send(res.map_err(|e| format!("{e:#}")));
                }
                Some(Cmd::MeterStop { reply }) => {
                    meter = None;
                    // force the normal path to re-tune and restart streaming
                    tuned = None;
                    let _ = reply.send(Ok(()));
                }
                None => {}
            }

            // meter mode owns the frontend: no streaming, no auto-recovery noise
            if meter.is_some() {
                continue;
            }

            let Some(target) = want else { continue };

            // 2. keep the device healthy, reopen/re-tune when it drops out
            if dev.is_none() || tuned != Some(target) {
                self.tuning.store(true, Ordering::Relaxed);
                match self.retune(&mut dev, target, &mut aligner, log) {
                    Ok(()) => {
                        tuned = Some(target);
                        empty_streak = 0;
                        backoff = Duration::from_millis(200);
                    }
                    Err(e) => {
                        (log)(&format!("recover {:.3} MHz failed: {e:#}", target as f64 / 1e6));
                        dev = None;
                        tuned = None;
                        std::thread::sleep(backoff);
                        backoff = (backoff * 2).min(Duration::from_secs(10));
                    }
                }
                self.tuning.store(false, Ordering::Relaxed);
                continue;
            }

            // 3. pump bytes to subscribers
            let n = match dev.as_mut().unwrap().read_ts(&mut buf, Duration::from_millis(200)) {
                Ok(n) => n,
                Err(e) => {
                    (log)(&format!("usb error: {e:#}; reopening"));
                    dev = None;
                    tuned = None;
                    continue;
                }
            };

            if n == 0 {
                empty_streak += 1;

                // ~6 s of silence on a tuned mux means the frontend wedged
                if empty_streak > 30 {
                    (log)("no data for 6s, power-cycling the frontend");
                    if let Some(d) = dev.as_mut() {
                        d.power_cycle();
                    }
                    tuned = None;
                    empty_streak = 0;
                }
                continue;
            }
            empty_streak = 0;

            let mut aligned = Vec::with_capacity(n + PKT);
            aligner.push(&buf[..n], &mut aligned);
            if aligned.is_empty() {
                continue;
            }

            {
                let mut g = self.inner.lock().unwrap();
                g.analyzer.feed(&aligned);
                g.bytes_total += aligned.len() as u64;
                g.resyncs = aligner.resyncs;
            }
            window_bytes += aligned.len() as u64;
            let _ = self.tx.send(Bytes::from(aligned));

            if window_start.elapsed() >= Duration::from_secs(2) {
                let secs = window_start.elapsed().as_secs_f32();
                let mbps = window_bytes as f32 * 8.0 / secs / 1e6;
                self.inner.lock().unwrap().bitrate_mbps = mbps;
                window_bytes = 0;
                window_start = Instant::now();
            }
        }

        if let Some(d) = dev.take() {
            d.close();
        }
    }

    /// Tune for measurement (streaming stopped) and take one reading.
    fn meter_step(
        &self,
        dev: &mut Option<Device>,
        meter: &mut Option<u32>,
        tuned: &mut Option<u32>,
        hz: Option<u32>,
    ) -> anyhow::Result<SignalReading> {
        if dev.is_none() {
            *dev = Some(Device::open()?);
        }
        let d = dev.as_mut().unwrap();

        if let Some(hz) = hz {
            if *meter != Some(hz) {
                // pause the pipe: control reads are unreliable while it streams
                let _ = d.stream(false);
                d.set_freq(hz)?;
                if !d.wait_lock(Duration::from_millis(1500)) {
                    // cold frontend: same recipe as the scanner
                    d.power_cycle();
                    d.set_freq(hz)?;
                    d.wait_lock(Duration::from_millis(2000));
                }
                *meter = Some(hz);
                *tuned = None;
            }
        }
        let freq = (*meter).ok_or_else(|| anyhow::anyhow!("not in meter mode"))?;
        let st = d.meter_status();
        Ok(SignalReading {
            freq_hz: freq,
            locked: st.locked,
            strength_pct: st.strength_pct,
            snr_db: st.snr_db,
            modulation: st.info.modulation(),
            guard_interval: st.info.guard_interval(),
            fec: st.info.fec(),
        })
    }

    fn retune(
        &self,
        dev: &mut Option<Device>,
        hz: u32,
        aligner: &mut Aligner,
        log: fn(&str),
    ) -> anyhow::Result<()> {
        if dev.is_none() {
            let d = Device::open()?;
            (log)(&format!(
                "device: firmware V{} ({:?}) bus {:03} addr {:03}",
                d.version.to_string(),
                d.variant,
                d.bus,
                d.address
            ));
            *dev = Some(d);
        }
        let d = dev.as_mut().unwrap();
        // the frontend wedges easily: always start a retune from a clean power state
        d.power_cycle();
        d.set_freq(hz)?;

        let mut locked = d.wait_lock(Duration::from_millis(2500));
        if !locked {
            // common on the first tune after a wedge: one power cycle fixes it
            d.power_cycle();
            d.set_freq(hz)?;
            locked = d.wait_lock(Duration::from_millis(2500));
        }
        if !locked {
            anyhow::bail!("no demod lock on {:.3} MHz (antenna?)", hz as f64 / 1e6);
        }

        // read signal info *before* opening the pipe: control transfers while the
        // FX2 is streaming stall the bulk pipe on this hardware
        // signal info must be read before the pipe opens: control transfers while
        // the FX2 is streaming stall the bulk endpoint on this hardware
        let st = d.status();
        d.stream(true)?;
        // new mux: wipe PSI/alignment state and kick clients off the old one
        *aligner = Aligner::default();
        {
            let mut g = self.inner.lock().unwrap();
            g.analyzer = Analyzer::default();
            g.freq_hz = hz;
            g.status = Status {
                tuned_hz: hz,
                ..st
            };
            g.resyncs = 0;
        }
        self.epoch.fetch_add(1, Ordering::Relaxed);
        (log)(&format!(
            "tuned {:.3} MHz lock={} strength={}% snr={:.1} dB",
            hz as f64 / 1e6,
            st.locked as u8,
            st.strength_pct,
            st.snr_db
        ));
        Ok(())
    }
}
