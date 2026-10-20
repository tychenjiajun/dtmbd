//! L1 — wire protocol of the DTMB USB stick (`04b4:1004`, NERC / Letv / Aiwa / CVB).
//!
//! Derived from the Linux drivers `hd2312` (NERC) and `DtmbUSB` (Letv/Aiwa/CVB)
//! plus the Windows `LeDTMB` sources. Every command is a vendor control transfer
//! with `wValue = 0x00FE`, `wIndex = 0`.

use std::time::Duration;

pub const VID: u16 = 0x04b4;
pub const PID: u16 = 0x1004;

pub const IFACE: u8 = 0;
/// Bulk-IN endpoint carrying MPEG-TS.
pub const EP_TS: u8 = 0x82;

pub const CTRL_VALUE: u16 = 0x00FE;
pub const REQ_TIMEOUT: Duration = Duration::from_millis(600);

/// China's DTMB raster: 8 MHz channels, UHF band. VHF (171–219) exists in some
/// cities but is not scanned by default.
pub const FREQ_MIN_HZ: u32 = 474_000_000;
pub const FREQ_MAX_HZ: u32 = 866_000_000;
pub const FREQ_STEP_HZ: u32 = 8_000_000;

pub fn encode_freq(hz: u32) -> [u8; 4] {
    hz.to_be_bytes()
}

/// Vendor request codes.
pub mod req {
    pub const SET_FREQ: u8 = 0xFC;
    /// IN: lock flag. OUT with a frequency payload: tuning kick.
    pub const LOCK: u8 = 0xEA;
    pub const HAS_SIGNAL: u8 = 0xEC;
    pub const STRENGTH: u8 = 0xEB;
    pub const SNR: u8 = 0xE8;
    pub const SIGNAL_INFO: u8 = 0xE7;
    pub const GET_VERSION: u8 = 0xED;
    pub const STREAM_ON: u8 = 0xAB;
    pub const STREAM_OFF: u8 = 0xAC;
    pub const POWER_ON: u8 = 0xAD;
    pub const POWER_OFF: u8 = 0xAE;
    pub const CVB_KEY: u8 = 0xBE;
    pub const CVB_HASH: u8 = 0xBF;
    pub const CVB_KEY_PAYLOAD: &[u8] = b"cidanAKEY";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Nerc,
    Letv,
    Aiwa,
    Cvb,
    Unknown,
}

#[derive(Debug, Clone, Copy)]
pub struct Version {
    pub raw: [u8; 4],
}

impl Version {
    pub fn to_string(&self) -> String {
        format!(
            "{}.{}.{}{}",
            self.raw[0], self.raw[1], self.raw[2], self.raw[3]
        )
    }

    pub fn variant(&self) -> Variant {
        // the Linux driver keys off bytes 1..=3 == 08 20 44
        if self.raw[1] == 0x08 && self.raw[2] == 0x20 && self.raw[3] == 0x44 {
            match self.raw[0] {
                0x03 => Variant::Letv,
                0x05 => Variant::Aiwa,
                0x06 => Variant::Cvb,
                _ => Variant::Unknown,
            }
        } else {
            Variant::Nerc
        }
    }
}

impl From<[u8; 4]> for Version {
    fn from(raw: [u8; 4]) -> Self {
        Self { raw }
    }
}

/// Frontend parameters reported by request 0xE7.
#[derive(Debug, Clone, Copy, Default)]
pub struct SignalInfo {
    pub raw: [u8; 6],
}

impl SignalInfo {
    pub fn guard_interval(&self) -> &'static str {
        match self.raw[1] {
            0 => "PN945",
            1 => "PN595",
            2 => "PN420",
            _ => "?",
        }
    }
    pub fn fec(&self) -> &'static str {
        match self.raw[2] {
            0 => "0.4",
            1 => "0.6",
            2 => "0.8",
            _ => "?",
        }
    }
    pub fn interleaving(&self) -> &'static str {
        match self.raw[3] {
            0 => "720",
            1 => "240",
            _ => "?",
        }
    }
    pub fn modulation(&self) -> &'static str {
        match self.raw[4] {
            0 => "4QAM-NR",
            1 => "4QAM",
            2 => "16QAM",
            3 => "32QAM",
            4 => "64QAM",
            _ => "?",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Status {
    pub tuned_hz: u32,
    pub locked: bool,
    pub has_signal: bool,
    pub strength_pct: u8,
    pub snr_db: f32,
    pub info: SignalInfo,
}
