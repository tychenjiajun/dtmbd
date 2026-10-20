// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) 2026 Jiajun Chen <tychenjiajun@live.cn>

//! L1 — USB transport (rusb / libusb-1.0).
//!
//! The stick is a vendor-class device: one interface, vendor control requests on
//! EP0 and MPEG-TS on bulk-IN 0x82. No kernel driver, no firmware upload.
//!
//! Two quirks drive the design of this layer and are handled here, not above:
//!   * changing frequency while streaming wedges the FX2 FIFO, so `set_freq`
//!     always stops the pipe first;
//!   * a wedged frontend only recovers through a power cycle (`power_cycle`).

use anyhow::{bail, Context as AnyhowContext, Result};
use rusb::{Context, DeviceHandle, Direction, Recipient, RequestType, UsbContext};
use std::time::{Duration, Instant};

pub mod proto;

use proto::{req, SignalInfo, Status, Variant, Version};

/// Control-transfer timeout: long enough for a NAKing frontend, short enough
/// that a dead device does not hang the tuner thread.
const CTRL_TIMEOUT: Duration = proto::REQ_TIMEOUT;

pub struct Device {
    handle: DeviceHandle<Context>,
    pub version: Version,
    pub variant: Variant,
    pub bus: u8,
    pub address: u8,
}

impl Device {
    pub fn open() -> Result<Self> {
        let ctx = Context::new().context("libusb init")?;
        let devices = ctx.devices().context("enumerating USB devices")?;

        for dev in devices.iter() {
            let desc = match dev.device_descriptor() {
                Ok(d) => d,
                Err(_) => continue,
            };
            if desc.vendor_id() != proto::VID || desc.product_id() != proto::PID {
                continue;
            }

            let handle = dev.open().with_context(|| {
                format!("opening {:04x}:{:04x}", proto::VID, proto::PID)
            })?;

            // Linux: a dvb-usb kernel module may already own the interface.
            let _ = handle.set_auto_detach_kernel_driver(true);
            #[cfg(target_os = "linux")]
            if handle.kernel_driver_active(proto::IFACE).unwrap_or(false) {
                let _ = handle.detach_kernel_driver(proto::IFACE);
            }

            handle
                .claim_interface(proto::IFACE)
                .context("claiming interface 0 (is the dvb-usb kernel module loaded?)")?;

            let mut this = Self {
                handle,
                version: Version::from([0u8; 4]),
                variant: Variant::Unknown,
                bus: dev.bus_number(),
                address: dev.address(),
            };

            // a previous process may have died mid-stream, leaving the FIFO full
            this.power_cycle();
            // the tuner PLL needs a moment after power-on or the first set_freq
            // is silently dropped (costs ~2 s of retries later)
            std::thread::sleep(Duration::from_millis(300));
            let _ = this.ctrl_out(req::STREAM_OFF, &[]);

            let mut v = [0u8; 4];
            if this.ctrl_in(req::GET_VERSION, &mut v)? != 4 {
                bail!("GET_VERSION returned a short reply");
            }
            this.version = Version::from(v);
            this.variant = this.version.variant();

            if this.variant == Variant::Cvb {
                // CVB sticks refuse to tune until the activation key is exchanged.
                let _ = this.ctrl_out(req::CVB_KEY, req::CVB_KEY_PAYLOAD);
                let mut hash = [0u8; 22];
                let _ = this.ctrl_in(req::CVB_HASH, &mut hash);
            }

            return Ok(this);
        }

        bail!(
            "no DTMB dongle found ({:04x}:{:04x}). Is it plugged in?",
            proto::VID,
            proto::PID
        )
    }

    // ---------------------------------------------------------------- plumbing

    fn ctrl(
        &mut self,
        dir: Direction,
        request: u8,
        mut data: Option<&mut [u8]>,
        timeout: Duration,
        retry: bool,
    ) -> Result<usize> {
        let req_type = rusb::request_type(dir, RequestType::Vendor, Recipient::Device);
        let len = data.as_ref().map(|d| d.len()).unwrap_or(0);

        // a stalled control pipe poisons every later request: clear once and retry
        let attempts = if retry { 2 } else { 1 };
        for attempt in 0..attempts {
            let res: rusb::Result<usize> = match (&mut data, dir) {
                (Some(buf), Direction::In) => self
                    .handle
                    .read_control(req_type, request, proto::CTRL_VALUE, 0, buf, timeout),
                (Some(buf), Direction::Out) => self
                    .handle
                    .write_control(req_type, request, proto::CTRL_VALUE, 0, buf, timeout),
                (None, _) => {
                    let mut empty: [u8; 0] = [];
                    self.handle
                        .write_control(req_type, request, proto::CTRL_VALUE, 0, &mut empty, timeout)
                }
            };

            match res {
                Ok(n) => return Ok(n),
                Err(rusb::Error::Pipe) if attempt + 1 < attempts => {
                    let _ = self.handle.clear_halt(0);
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(rusb::Error::Timeout) if attempt + 1 < attempts && len > 0 => {
                    std::thread::sleep(Duration::from_millis(30));
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!(
                            "control {} 0x{request:02x} ({len} bytes)",
                            if dir == Direction::In { "IN" } else { "OUT" }
                        )
                    })
                }
            }
        }
        bail!("control request 0x{request:02x} failed")
    }

    fn ctrl_in(&mut self, request: u8, buf: &mut [u8]) -> Result<usize> {
        self.ctrl(Direction::In, request, Some(buf), CTRL_TIMEOUT, true)
    }

    fn ctrl_out(&mut self, request: u8, payload: &[u8]) -> Result<usize> {
        if payload.is_empty() {
            self.ctrl(Direction::Out, request, None, CTRL_TIMEOUT, true)
        } else {
            let mut buf = payload.to_vec();
            self.ctrl(Direction::Out, request, Some(&mut buf), CTRL_TIMEOUT, true)
        }
    }

    /// Fire-and-forget OUT: short timeout, no retry. Used for the tuning kick,
    /// which legitimately NAKs on empty channels and must not slow a scan down.
    fn ctrl_out_quiet(&mut self, request: u8, payload: &mut [u8], timeout: Duration) -> Result<usize> {
        self.ctrl(Direction::Out, request, Some(payload), timeout, false)
    }

    /// Status reads are short and never retried: empty channels NAK them and a
    /// 6 s scan must not turn into a 60 s scan.
    fn status_in(&mut self, request: u8, buf: &mut [u8]) -> Result<usize> {
        self.ctrl(Direction::In, request, Some(buf), Duration::from_millis(250), false)
    }

    // --------------------------------------------------------------- commands

    /// Power-cycle the frontend. ~500 ms, but the only way to recover from the
    /// FX2 wedge that shows up after long tuning/streaming runs.
    pub fn power_cycle(&mut self) {
        let _ = self.ctrl_out(req::POWER_OFF, &[]);
        std::thread::sleep(Duration::from_millis(150));
        let _ = self.ctrl_out(req::POWER_ON, &[]);
        std::thread::sleep(Duration::from_millis(350));
    }

    pub fn power_on(&mut self) -> Result<()> {
        self.ctrl_out(req::POWER_ON, &[])?;
        Ok(())
    }

    pub fn stream(&mut self, on: bool) -> Result<()> {
        let r = if on { req::STREAM_ON } else { req::STREAM_OFF };
        self.ctrl_out(r, &[])?;
        Ok(())
    }

    /// Retune. Always stops the pipe first, see the module docs.
    pub fn set_freq(&mut self, hz: u32) -> Result<()> {
        let mut off: [u8; 0] = [];
        let _ = self.ctrl(
            Direction::Out,
            req::STREAM_OFF,
            Some(&mut off),
            Duration::from_millis(120),
            false,
        );
        let mut payload = proto::encode_freq(hz);
        self.ctrl(Direction::Out, req::SET_FREQ, Some(&mut payload), CTRL_TIMEOUT, true)?;
        // 1-byte kick is what the Letv firmware wants and locks ~200 ms sooner
        // than the 4-byte variant; NERC firmware ignores it either way.
        let mut kick = proto::encode_freq(hz)[..1].to_vec();
        let _ = self.ctrl_out_quiet(req::LOCK, &mut kick, Duration::from_millis(200));
        Ok(())
    }

    pub fn locked(&mut self) -> bool {
        let mut b = [0u8; 1];
        matches!(
            self.ctrl(Direction::In, req::LOCK, Some(&mut b), Duration::from_millis(300), false),
            Ok(1) if b[0] == 1
        )
    }

    /// Wait for demod lock. Returns true if locked within `budget`.
    pub fn wait_lock(&mut self, budget: Duration) -> bool {
        let t0 = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(100));
            if self.locked() {
                return true;
            }
            if t0.elapsed() >= budget {
                return false;
            }
        }
    }

    pub fn status(&mut self) -> Status {
        let mut st = Status::default();
        let mut b = [0u8; 4];

        st.locked = self.locked();
        let mut one = [0u8; 1];
        st.has_signal = matches!(self.status_in(req::HAS_SIGNAL, &mut one), Ok(1) if one[0] != 0);
        if matches!(self.status_in(req::STRENGTH, &mut b), Ok(4)) {
            st.strength_pct = b[3].min(100);
        }
        let mut snr = [0u8; 2];
        if matches!(self.status_in(req::SNR, &mut snr), Ok(2)) {
            st.snr_db = snr[0] as f32 + snr[1] as f32 / 100.0;
        }
        let mut info = [0u8; 6];
        if matches!(self.status_in(req::SIGNAL_INFO, &mut info), Ok(6)) {
            st.info = SignalInfo { raw: info };
        }
        st
    }

    /// Fast triplet for the antenna meter: lock, strength, SNR with short
    /// timeouts (an unlocked channel NAKs everything, so keep it snappy).
    pub fn meter_status(&mut self) -> Status {
        let d = Duration::from_millis(150);
        let mut st = Status::default();

        let mut b = [0u8; 1];
        st.locked = matches!(
            self.ctrl(Direction::In, req::LOCK, Some(&mut b), d, false),
            Ok(1) if b[0] == 1
        );
        let mut s = [0u8; 4];
        if matches!(self.ctrl(Direction::In, req::STRENGTH, Some(&mut s), d, false), Ok(4)) {
            st.strength_pct = s[3].min(100);
        }
        let mut n = [0u8; 2];
        if matches!(self.ctrl(Direction::In, req::SNR, Some(&mut n), d, false), Ok(2)) {
            st.snr_db = n[0] as f32 + n[1] as f32 / 100.0;
        }
        if st.locked {
            let mut info = [0u8; 6];
            if matches!(self.ctrl(Direction::In, req::SIGNAL_INFO, Some(&mut info), d, false), Ok(6))
            {
                st.info = SignalInfo { raw: info };
            }
        }
        st
    }

    // ------------------------------------------------------------------- data

    /// Blocking bulk read of MPEG-TS from endpoint 0x82.
    pub fn read_ts(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        match self.handle.read_bulk(proto::EP_TS, buf, timeout) {
            Ok(n) => Ok(n),
            Err(rusb::Error::Pipe) => {
                let _ = self.handle.clear_halt(proto::EP_TS);
                Ok(0)
            }
            Err(rusb::Error::Timeout) => Ok(0),
            Err(e) => Err(e).context("bulk read on endpoint 0x82"),
        }
    }

    pub fn close(mut self) {
        let _ = self.stream(false);
        let _ = self.handle.release_interface(proto::IFACE);
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = self.stream(false);
        let _ = self.handle.release_interface(proto::IFACE);
    }
}
