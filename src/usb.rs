//! USB transport for the DTMB stick (rusb / libusb-1.0).
//!
//! The stick is a vendor-class device: one interface, two bulk-IN endpoints
//! (0x82 = MPEG-TS, 0x86 = unused) and vendor control requests on endpoint 0.
//! No kernel driver, no firmware upload. On Windows it needs WinUSB bound
//! (Zadig) or the vendor CyUSB driver; on macOS/Linux it works out of the box.

use anyhow::{bail, Context as AnyhowContext, Result};
use rusb::{Context, DeviceHandle, Direction, Recipient, RequestType, TransferType, UsbContext};
use std::time::{Duration, Instant};

use crate::proto::{self, req, SignalInfo, Status, Variant, Version};

/// Default libusb transfer timeout for control requests.
const CTRL_TIMEOUT: Duration = proto::REQ_TIMEOUT;

pub struct Dtmb {
    handle: DeviceHandle<Context>,
    pub version: Version,
    pub variant: Variant,
    pub bus: u8,
    pub address: u8,
    ctrl_stalled: bool,
}

impl Dtmb {
    /// Open the first matching dongle.
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
            let bus = dev.bus_number();
            let address = dev.address();
            let handle = dev.open().with_context(|| {
                format!("opening {:04x}:{:04x}", proto::VID, proto::PID)
            })?;

            // Linux: the dvb-usb kernel module may already own the device.
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
                bus,
                address,
                ctrl_stalled: false,
            };

            // A previous process may have died mid-stream, leaving the FX2 FIFO
            // full and the tuner wedged: power-cycle before doing anything else.
            let _ = this.ctrl_out(req::POWER_OFF, &[]);
            std::thread::sleep(Duration::from_millis(120));
            let _ = this.ctrl_out(req::POWER_ON, &[]);
            std::thread::sleep(Duration::from_millis(250));
            let _ = this.ctrl_out(req::STREAM_OFF, &[]);

            let mut v = [0u8; 4];
            let n = this.ctrl_in(req::GET_VERSION, &mut v)?;
            if n != 4 {
                bail!("GET_VERSION returned {n} bytes, expected 4");
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
            "no DTMB dongle found ({:04x}:{:04x}). Plugged in? On Windows bind WinUSB via Zadig.",
            proto::VID,
            proto::PID
        )
    }

    /// Enumerate descriptors without claiming the device (diagnostics).
    pub fn describe_all() -> Result<String> {
        let ctx = Context::new().context("libusb init")?;
        let devices = ctx.devices()?;
        let mut out = String::new();
        for dev in devices.iter() {
            let desc = match dev.device_descriptor() {
                Ok(d) => d,
                Err(_) => continue,
            };
            if desc.vendor_id() != proto::VID || desc.product_id() != proto::PID {
                continue;
            }
            let handle = dev.open().ok();
            let (man, prod) = match &handle {
                Some(h) => (
                    h.read_manufacturer_string_ascii(&desc).unwrap_or_default(),
                    h.read_product_string_ascii(&desc).unwrap_or_default(),
                ),
                None => (String::new(), String::new()),
            };
            let v = desc.device_version();
            out.push_str(&format!(
                "bus {:03} addr {:03}  {:04x}:{:04x}  vendor={:?} product={:?}  bcdDevice={}.{}.{}\n",
                dev.bus_number(),
                dev.address(),
                desc.vendor_id(),
                desc.product_id(),
                man,
                prod,
                v.major(),
                v.minor(),
                v.sub_minor()
            ));
            if let Ok(cfg) = dev.active_config_descriptor() {
                for iface in cfg.interfaces() {
                    for alt in iface.descriptors() {
                        out.push_str(&format!(
                            "   iface {} alt {} class={:02x} sub={:02x} proto={:02x}\n",
                            alt.interface_number(),
                            alt.setting_number(),
                            alt.class_code(),
                            alt.sub_class_code(),
                            alt.protocol_code()
                        ));
                        for ep in alt.endpoint_descriptors() {
                            out.push_str(&format!(
                                "      ep {:02x} {:?} {:?} max_packet={}\n",
                                ep.address(),
                                match ep.direction() {
                                    Direction::In => "IN",
                                    Direction::Out => "OUT",
                                },
                                match ep.transfer_type() {
                                    TransferType::Bulk => "bulk",
                                    TransferType::Interrupt => "interrupt",
                                    TransferType::Isochronous => "iso",
                                    TransferType::Control => "control",
                                },
                                ep.max_packet_size()
                            ));
                        }
                    }
                }
            }
        }
        Ok(out)
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
                Ok(n) => {
                    self.ctrl_stalled = false;
                    return Ok(n);
                }
                Err(rusb::Error::Pipe) if attempt == 0 => {
                    // STALL on EP0 - clear halt and retry
                    self.ctrl_stalled = true;
                    let _ = self.handle.clear_halt(0);
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(rusb::Error::Timeout) if attempt == 0 && len > 0 => {
                    // transient on this hardware right after a retune
                    std::thread::sleep(Duration::from_millis(30));
                    continue;
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!(
                            "control {} 0x{request:02x} failed ({len} bytes)",
                            if dir == Direction::In { "IN" } else { "OUT" }
                        )
                    })
                }
            }
        }
        bail!("control request 0x{request:02x} failed after retry")
    }

    pub fn ctrl_in(&mut self, request: u8, buf: &mut [u8]) -> Result<usize> {
        self.ctrl(Direction::In, request, Some(buf), CTRL_TIMEOUT, true)
    }

    pub fn ctrl_out(&mut self, request: u8, payload: &[u8]) -> Result<usize> {
        if payload.is_empty() {
            self.ctrl(Direction::Out, request, None, CTRL_TIMEOUT, true)
        } else {
            let mut buf = payload.to_vec();
            self.ctrl(Direction::Out, request, Some(&mut buf), CTRL_TIMEOUT, true)
        }
    }

    /// Vendor OUT with an explicit payload buffer (no copy).
    fn ctrl_out_mut(&mut self, request: u8, payload: &mut [u8]) -> Result<usize> {
        self.ctrl(Direction::Out, request, Some(payload), CTRL_TIMEOUT, true)
    }

    /// Fire-and-forget OUT: short timeout, no retry. Used for the tuning kick,
    /// which legitimately NAKs on empty channels and must not slow the scan down.
    fn ctrl_out_quiet(&mut self, request: u8, payload: &mut [u8], timeout: Duration) -> Result<usize> {
        self.ctrl(Direction::Out, request, Some(payload), timeout, false)
    }

    // --------------------------------------------------------------- commands

    pub fn power(&mut self, on: bool) -> Result<()> {
        let r = if on { req::POWER_ON } else { req::POWER_OFF };
        self.ctrl_out(r, &[])?;
        Ok(())
    }

    /// Power-cycle the frontend. Costs ~400 ms but is the only reliable way to
    /// recover from the FX2 wedge that shows up after long tuning/streaming runs.
    pub fn power_cycle(&mut self) {
        let _ = self.ctrl_out(req::POWER_OFF, &[]);
        std::thread::sleep(Duration::from_millis(150));
        let _ = self.ctrl_out(req::POWER_ON, &[]);
        std::thread::sleep(Duration::from_millis(350));
    }

    pub fn stream(&mut self, on: bool) -> Result<()> {
        let r = if on { req::STREAM_ON } else { req::STREAM_OFF };
        self.ctrl_out(r, &[])?;
        Ok(())
    }

    /// Set the tuner frequency. Two writes are needed on the Letv-class firmware:
    /// 0xFC carries the frequency, then 0xEA with the same payload kicks demod lock.
    /// Experimental: kick payload length 0 = no kick, 1 or 4 = bytes written to 0xEA.
    pub fn set_freq_variant(&mut self, hz: u32, kick_len: usize) -> Result<()> {
        let mut off: [u8; 0] = [];
        let _ = self.ctrl(Direction::Out, req::STREAM_OFF, Some(&mut off), Duration::from_millis(120), false);
        let mut payload = proto::encode_freq(hz);
        self.ctrl_out_mut(req::SET_FREQ, &mut payload)?;
        if kick_len > 0 {
            let mut kick = proto::encode_freq(hz).to_vec();
            kick.truncate(kick_len);
            let _ = self.ctrl_out_quiet(req::LOCK, &mut kick, Duration::from_millis(200));
        }
        Ok(())
    }

    /// Time-to-lock measurement helper (ms), or None if it never locked.
    pub fn time_to_lock(&mut self, budget: Duration) -> Option<u128> {
        let t0 = std::time::Instant::now();
        while t0.elapsed() < budget {
            std::thread::sleep(Duration::from_millis(100));
            if self.locked_with(Duration::from_millis(300)).unwrap_or(false) {
                return Some(t0.elapsed().as_millis());
            }
        }
        None
    }

    pub fn set_freq(&mut self, hz: u32) -> Result<()> {
        // the FX2 wedges if the frequency is changed while it is streaming:
        // always stop the pipe, retune, then let the caller restart it
        let mut off: [u8; 0] = [];
        let _ = self.ctrl(Direction::Out, req::STREAM_OFF, Some(&mut off), Duration::from_millis(120), false);
        let mut payload = proto::encode_freq(hz);
        self.ctrl_out_mut(req::SET_FREQ, &mut payload)?;
        // 1-byte kick is what the Letv driver does and locks ~200 ms faster than
        // the 4-byte variant; NERC firmware ignores it either way.
        let mut kick = proto::encode_freq(hz)[..1].to_vec();
        let _ = self.ctrl_out_quiet(req::LOCK, &mut kick, Duration::from_millis(200));
        Ok(())
    }

    pub fn locked(&mut self) -> Result<bool> {
        self.locked_with(Duration::from_millis(400))
    }

    pub fn locked_with(&mut self, timeout: Duration) -> Result<bool> {
        let mut b = [0u8; 1];
        let n = self.ctrl(Direction::In, req::LOCK, Some(&mut b), timeout, false)?;
        Ok(n == 1 && b[0] == 1)
    }

    /// Status reads use a short timeout and no retry: empty channels NAK them
    /// and a 6 s scan must not turn into a 60 s scan.
    fn status_in(&mut self, request: u8, buf: &mut [u8]) -> Result<usize> {
        self.ctrl(Direction::In, request, Some(buf), Duration::from_millis(250), false)
    }

    pub fn has_signal(&mut self) -> Result<bool> {
        let mut b = [0u8; 1];
        let n = self.status_in(req::HAS_SIGNAL, &mut b)?;
        Ok(n == 1 && b[0] != 0)
    }

    pub fn strength_pct(&mut self) -> Result<u8> {
        let mut b = [0u8; 4];
        let n = self.status_in(req::STRENGTH, &mut b)?;
        Ok(if n == 4 { b[3].min(100) } else { 0 })
    }

    pub fn snr_db(&mut self) -> Result<f32> {
        let mut b = [0u8; 2];
        let n = self.status_in(req::SNR, &mut b)?;
        Ok(if n == 2 {
            b[0] as f32 + b[1] as f32 / 100.0
        } else {
            0.0
        })
    }

    pub fn signal_info(&mut self) -> Result<SignalInfo> {
        let mut b = [0u8; 6];
        let n = self.status_in(req::SIGNAL_INFO, &mut b)?;
        Ok(if n == 6 {
            SignalInfo { raw: b }
        } else {
            SignalInfo::default()
        })
    }

    pub fn quality(&mut self) -> Result<u32> {
        let mut b = [0u8; 3];
        let n = self.status_in(req::QUALITY, &mut b)?;
        Ok(if n == 3 {
            b[1] as u32 * 100 + b[0] as u32 * 10_000 + b[2] as u32
        } else {
            0
        })
    }

    /// Tune + settle + snapshot, as one call.
    pub fn tune(&mut self, hz: u32, settle: Duration) -> Result<Status> {
        self.set_freq(hz)?;
        let deadline = Instant::now() + settle;
        let mut status = Status {
            tuned_hz: hz,
            ..Default::default()
        };
        // poll lock while settling: the demod usually locks in ~100..800 ms
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(80));
            if self.locked_with(Duration::from_millis(250)).unwrap_or(false) {
                break;
            }
        }
        status.locked = self.locked().unwrap_or(false);
        status.has_signal = self.has_signal().unwrap_or(false);
        status.strength_pct = self.strength_pct().unwrap_or(0);
        status.snr_db = self.snr_db().unwrap_or(0.0);
        status.info = self.signal_info().unwrap_or_default();
        if status.locked {
            status.quality = self.quality().unwrap_or(0);
        }
        Ok(status)
    }

    // ------------------------------------------------------------------- data

    /// Blocking bulk read of MPEG-TS bytes from endpoint 0x82.
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

    /// Release the interface and let the stick idle down.
    pub fn close(mut self) {
        let _ = self.stream(false);
        let _ = self.power(false);
        let _ = self.handle.release_interface(proto::IFACE);
    }
}

impl Drop for Dtmb {
    fn drop(&mut self) {
        let _ = self.stream(false);
        let _ = self.handle.release_interface(proto::IFACE);
    }
}
