#![allow(dead_code)]

//! DTMB USB dongle wire protocol (04b4:1004 — NERC / Letv / Aiwa / CVB "DtmbUSB" family).
//!
//! Everything here was derived from the Linux drivers `hd2312` (NERC, HDIC HD2312) and
//! `DtmbUSB` (Letv/Aiwa/CVB) plus the Windows `LeDTMB` source.
//!
//! Key property: the dongle needs **no firmware upload and no kernel driver** — it
//! enumerates already in "warm" state, tuning happens through vendor control transfers
//! (wValue = 0x00FE), and the MPEG-TS arrives on bulk endpoint 0x82.

use std::time::Duration;

pub const VID: u16 = 0x04b4; // Cypress (FX2 style VID reused by NERC)
pub const PID: u16 = 0x1004;

pub const IFACE: u8 = 0;
pub const EP_TS: u8 = 0x82;
/// Second bulk-IN endpoint seen on some boards; unused by every known driver.
pub const EP_AUX: u8 = 0x86;

pub const CTRL_VALUE: u16 = 0x00FE;
pub const REQ_TIMEOUT: Duration = Duration::from_millis(600);

/// Vendor requests (bRequest).
pub mod req {
    // tuning / status
    pub const SET_FREQ: u8 = 0xFC;
    /// IN: lock flag. OUT with a 4-byte big-endian frequency: "kick" the tuner
    /// (required by the Letv-class firmware, harmless on NERC).
    pub const LOCK: u8 = 0xEA;
    pub const HAS_SIGNAL: u8 = 0xEC;
    pub const STRENGTH: u8 = 0xEB;
    pub const SNR: u8 = 0xE8;
    pub const SIGNAL_INFO: u8 = 0xE7;
    pub const QUALITY: u8 = 0xE9;
    // control
    pub const GET_VERSION: u8 = 0xED;
    pub const STREAM_ON: u8 = 0xAB;
    pub const STREAM_OFF: u8 = 0xAC;
    pub const POWER_ON: u8 = 0xAD;
    pub const POWER_OFF: u8 = 0xAE;
    // CVB variant only: send activation key, then read hash.
    pub const CVB_KEY: u8 = 0xBE;
    pub const CVB_HASH: u8 = 0xBF;
    pub const CVB_KEY_PAYLOAD: &[u8] = b"cidanAKEY";
}

/// Which firmware family the stick reports. Determines status decoding quirks.
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
    pub fn major(&self) -> u8 {
        self.raw[0]
    }
    /// `3.8.20.68` style string as printed by the Linux drivers.
    pub fn to_string(&self) -> String {
        format!(
            "{}.{}.{}{}",
            self.raw[0], self.raw[1], self.raw[2], self.raw[3]
        )
    }
}

impl From<[u8; 4]> for Version {
    fn from(raw: [u8; 4]) -> Self {
        Self { raw }
    }
}

impl Version {
    pub fn variant(&self) -> Variant {
        // Linux DtmbUSB driver keys off bytes 1..=3 == 08 20 44.
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

/// DTMB channel raster, China: 8 MHz channels from 474 MHz (ch 13) to 866 MHz.
pub const FREQ_MIN_HZ: u32 = 474_000_000;
pub const FREQ_MAX_HZ: u32 = 866_000_000;
pub const FREQ_STEP_HZ: u32 = 8_000_000;

pub fn channel_list() -> Vec<u32> {
    (FREQ_MIN_HZ..=FREQ_MAX_HZ).step_by(FREQ_STEP_HZ as usize).collect()
}

pub fn freq_from_mhz(mhz: f64) -> u32 {
    (mhz * 1_000_000.0).round() as u32
}

/// Constellation / coding parameters reported by request 0xE7.
#[derive(Debug, Clone, Copy, Default)]
pub struct SignalInfo {
    pub raw: [u8; 6],
}

impl SignalInfo {
    pub fn carrier(&self) -> u8 {
        self.raw[0]
    }
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
    pub fn phase(&self) -> &'static str {
        match self.raw[5] {
            0 => "var",
            1 => "fixed",
            _ => "?",
        }
    }
}

/// One frontend snapshot.
#[derive(Debug, Clone, Copy, Default)]
pub struct Status {
    pub tuned_hz: u32,
    pub locked: bool,
    pub has_signal: bool,
    pub strength_pct: u8,
    pub snr_db: f32,
    pub info: SignalInfo,
    pub quality: u32,
}

impl Status {
    /// Rough "is this a usable mux" heuristic used by the scanner.
    pub fn score(&self) -> i32 {
        if !self.locked {
            return -1;
        }
        self.strength_pct as i32 * 10 + (self.snr_db * 10.0) as i32
    }
}

pub fn encode_freq(hz: u32) -> [u8; 4] {
    hz.to_be_bytes()
}
