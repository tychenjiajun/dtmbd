//! L4 — catalogue: the channel table produced by a scan, the M3U playlist the
//! TV subscribes to, and external name patches for muxes that carry no SDT.
//!
//! The table is the only thing that knows about frequencies other than the one
//! currently tuned, so it must survive restarts (JSON on disk).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::usb::proto::Status;
use crate::ts::{EsInfo, Program};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Channel {
    pub sid: u16,
    pub name: Option<String>,
    pub provider: Option<String>,
    pub freq_hz: u32,
    pub pmt_pid: u16,
    pub pcr_pid: u16,
    pub streams: Vec<EsInfo>,
}

impl Channel {
    pub fn label(&self) -> String {
        self.name
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("service {}", self.sid))
    }

    /// `tvg-id` used in the playlist: stable across rescans.
    pub fn tvg_id(&self) -> String {
        format!("dtmb-{}-{}", self.freq_hz / 1_000_000, self.sid)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MuxEntry {
    pub freq_hz: u32,
    pub strength_pct: u8,
    pub snr_db: f32,
    pub channels: Vec<Channel>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ChannelTable {
    pub generated_at: Option<String>,
    pub muxes: Vec<MuxEntry>,
}

impl ChannelTable {
    pub fn add_mux(&mut self, freq_hz: u32, status: Status, programs: Vec<Program>) {
        self.muxes.retain(|m| m.freq_hz != freq_hz);
        self.muxes.push(MuxEntry {
            freq_hz,
            strength_pct: status.strength_pct,
            snr_db: status.snr_db,
            channels: programs
                .into_iter()
                .map(|p| Channel {
                    sid: p.service_id,
                    name: p.name,
                    provider: p.provider,
                    freq_hz,
                    pmt_pid: p.pmt_pid,
                    pcr_pid: p.pcr_pid,
                    streams: p.streams,
                })
                .collect(),
        });
        self.muxes.sort_by_key(|m| m.freq_hz);
        self.generated_at = Some(now_iso());
    }

    pub fn channels(&self) -> impl Iterator<Item = &Channel> {
        self.muxes.iter().flat_map(|m| m.channels.iter())
    }

    pub fn find(&self, sid: u16) -> Option<&Channel> {
        self.channels().find(|c| c.sid == sid)
    }

    pub fn find_by_label(&self, needle: &str) -> Option<&Channel> {
        let n = needle.to_lowercase();
        self.channels()
            .find(|c| c.label().to_lowercase() == n)
            .or_else(|| {
                self.channels()
                    .find(|c| c.label().to_lowercase().contains(&n))
            })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read_to_string(path)
            .with_context(|| format!("reading channel table {}", path.display()))?;
        serde_json::from_str(&data).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).ok();
            }
        }
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(path, data).with_context(|| format!("writing {}", path.display()))
    }

    /// Playlist entries; `base` is the public http root, e.g. `http://host:8080`.
    pub fn m3u(&self, base: &str) -> String {
        let mut out = String::from("#EXTM3U\n");
        for c in self.channels() {
            let group = c
                .provider
                .clone()
                .unwrap_or_else(|| format!("{:.0}MHz", c.freq_hz as f64 / 1e6));
            out.push_str(&format!(
                "#EXTINF:-1 tvg-id=\"{id}\" tvg-name=\"{name}\" group-title=\"{group}\",{name}\n{base}/p/{sid}.ts\n",
                id = c.tvg_id(),
                name = c.label(),
                group = group,
                sid = c.sid,
                base = base
            ));
        }
        out
    }
}

/// External name patch: many Chinese muxes carry no SDT, so the channel names
/// have to come from a curated list (freq + service id).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NameEntry {
    #[serde(default)]
    pub freq_hz: Option<u32>,
    pub sid: u16,
    pub name: String,
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct NamePatch {
    #[serde(default)]
    pub names: Vec<NameEntry>,
}

impl ChannelTable {
    /// Overlay curated names onto the scanned table. Returns how many channels changed.
    pub fn apply_names(&mut self, patch: &NamePatch) -> usize {
        let mut n = 0;
        for m in self.muxes.iter_mut() {
            for c in m.channels.iter_mut() {
                let hit = patch.names.iter().find(|e| {
                    e.sid == c.sid && e.freq_hz.map(|f| f == c.freq_hz).unwrap_or(true)
                });
                if let Some(e) = hit {
                    if c.name.as_deref() != Some(e.name.as_str()) {
                        c.name = Some(e.name.clone());
                        n += 1;
                    }
                    if let Some(p) = &e.provider {
                        c.provider = Some(p.clone());
                    }
                }
            }
        }
        n
    }
}

pub fn default_table_path() -> PathBuf {
    if let Ok(p) = std::env::var("DTMB_TABLE") {
        return PathBuf::from(p);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".dtmb/channels.json");
    }
    PathBuf::from("channels.json")
}

fn now_iso() -> String {
    // no chrono dependency: seconds since epoch is enough for a "generated_at"
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}
