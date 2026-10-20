//! MPEG-TS: byte-stream re-alignment, PSI parsing (PAT/PMT/SDT), per-PID filtering.
//!
//! The dongle hands us a raw TS byte stream with no packet framing guarantees:
//! transfer boundaries land anywhere, and the first FIFO dump may contain junk.
//! Everything downstream therefore goes through [`Aligner`], which locks onto
//! 0x47 every 188 bytes and hides the framing from the rest of the program.

use std::collections::BTreeMap;

pub const PKT: usize = 188;
pub const SYNC: u8 = 0x47;
pub const PID_PAT: u16 = 0x0000;
#[allow(dead_code)]
pub const PID_NULL: u16 = 0x1FFF;
pub const PID_SDT: u16 = 0x0011;

/// Re-aligns an arbitrary byte stream into 188-byte TS packets.
pub struct Aligner {
    rem: Vec<u8>,
    locked: bool,
    /// packets dropped while hunting for sync (diagnostics)
    pub resyncs: u64,
}

impl Default for Aligner {
    fn default() -> Self {
        Self {
            rem: Vec::with_capacity(PKT * 8),
            locked: false,
            resyncs: 0,
        }
    }
}

impl Aligner {
    /// Feed raw bytes, append whole aligned packets to `out`.
    pub fn push(&mut self, data: &[u8], out: &mut Vec<u8>) {
        self.rem.extend_from_slice(data);

        if !self.locked {
            match find_sync(&self.rem) {
                Some(off) => {
                    if off > 0 {
                        self.rem.drain(..off);
                        self.resyncs += 1;
                    }
                    self.locked = true;
                }
                None => {
                    // no usable sync yet: keep a tail in case a packet straddles
                    if self.rem.len() > PKT * 4 {
                        let keep = self.rem.len() - PKT;
                        self.rem.drain(..keep);
                    }
                    return;
                }
            }
        }

        let n = self.rem.len() / PKT * PKT;
        if n == 0 {
            return;
        }
        // verify we are still in sync at the end of the chunk, otherwise re-hunt
        if self.rem[n - PKT] != SYNC {
            self.locked = false;
            let tail = self.rem.split_off(n.saturating_sub(PKT));
            self.rem = tail;
            self.push(&[], out);
            return;
        }
        out.extend_from_slice(&self.rem[..n]);
        self.rem.drain(..n);
    }

    #[allow(dead_code)]
    pub fn remainder(&self) -> &[u8] {
        &self.rem
    }
}

fn find_sync(buf: &[u8]) -> Option<usize> {
    // require two consecutive 0x47 188 bytes apart before declaring lock
    let mut i = 0;
    while i + PKT < buf.len() {
        if buf[i] == SYNC {
            if i + 2 * PKT < buf.len() {
                if buf[i + PKT] == SYNC && buf[i + 2 * PKT] == SYNC {
                    return Some(i);
                }
            } else {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

#[inline]
pub fn pid(pkt: &[u8]) -> u16 {
    (((pkt[1] & 0x1F) as u16) << 8) | pkt[2] as u16
}

#[inline]
pub fn payload_offset(pkt: &[u8]) -> Option<usize> {
    let afc = (pkt[3] >> 4) & 0x3;
    if afc & 0x1 == 0 {
        return None; // no payload
    }
    let mut off = 4;
    if afc & 0x2 != 0 {
        off += 1 + pkt[4] as usize;
    }
    if off >= PKT {
        None
    } else {
        Some(off)
    }
}

/// Collects one PSI section across packets of a PID.
#[derive(Default)]
struct SectionAssembler {
    buf: Vec<u8>,
    want: Option<usize>,
}

impl SectionAssembler {
    fn feed(&mut self, payload: &[u8], sections: &mut Vec<Vec<u8>>) {
        let mut p = payload;
        if p.is_empty() {
            return;
        }
        // pointer_field only valid at section start
        if self.buf.is_empty() {
            let ptr = p[0] as usize;
            if ptr + 1 > p.len() {
                return;
            }
            p = &p[1 + ptr..];
        }
        self.buf.extend_from_slice(p);
        loop {
            if self.want.is_none() {
                if self.buf.len() < 3 {
                    return;
                }
                let len = (((self.buf[1] & 0x0F) as usize) << 8) | self.buf[2] as usize;
                self.want = Some(3 + len);
            }
            let want = self.want.unwrap();
            if self.buf.len() < want {
                return;
            }
            let section: Vec<u8> = self.buf.drain(..want).collect();
            self.want = None;
            if crc32_mpeg(&section) == 0 {
                sections.push(section);
            }
            if self.buf.is_empty() {
                return;
            }
        }
    }
}

pub fn crc32_mpeg(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EsInfo {
    pub stream_type: u8,
    pub pid: u16,
    pub kind: &'static str,
    pub language: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Program {
    pub service_id: u16,
    pub pmt_pid: u16,
    pub pcr_pid: u16,
    pub name: Option<String>,
    pub provider: Option<String>,
    pub streams: Vec<EsInfo>,
}

impl Program {
    /// PIDs a client needs to decode just this service.
    pub fn pids(&self) -> Vec<u16> {
        let mut v = vec![self.pmt_pid, self.pmt_pid];
        v.push(self.pcr_pid);
        v.extend(self.streams.iter().map(|e| e.pid));
        v.sort_unstable();
        v.dedup();
        v
    }
}

pub fn stream_kind(t: u8) -> &'static str {
    match t {
        0x01 | 0x02 => "mpeg2-video",
        0x03 | 0x04 | 0x0f | 0x81 => "audio",
        0x05 => "private",
        0x06 => "pes-private",
        0x0b => "dsmcc",
        0x24 => "hevc",
        0x42 => "avs",
        0x1b => "h264",
        0x86 => "scte35",
        _ => "unknown",
    }
}

/// Live view of the multiplex: PAT → PMT → SDT.
#[derive(Default)]
pub struct Analyzer {
    pat: SectionAssembler,
    pmt: BTreeMap<u16, SectionAssembler>,
    sdt: SectionAssembler,
    /// cached PAT/PMT bytes, replayed to new clients so players lock instantly
    pub cache: BTreeMap<u16, Vec<u8>>,
    pub programs: BTreeMap<u16, Program>,
    pub pat_pids: Vec<(u16, u16)>,
    pub packet_count: u64,
}

impl Analyzer {
    pub fn feed(&mut self, data: &[u8]) {
        for pkt in data.chunks_exact(PKT) {
            if pkt[0] != SYNC {
                continue;
            }
            self.packet_count += 1;
            let p = pid(pkt);
            let Some(payload) = payload_offset(pkt).map(|o| &pkt[o..]) else {
                continue;
            };

            match p {
                PID_PAT => {
                    let mut secs = Vec::new();
                    self.pat.feed(payload, &mut secs);
                    for s in secs {
                        self.on_pat(s);
                    }
                }
                PID_SDT => {
                    let mut secs = Vec::new();
                    self.sdt.feed(payload, &mut secs);
                    for s in secs {
                        self.on_sdt(s);
                    }
                }
                _ => {
                    if self.pat_pids.iter().any(|(_, pmt)| *pmt == p) {
                        let mut secs = Vec::new();
                        self.pmt.entry(p).or_default().feed(payload, &mut secs);
                        for s in secs {
                            self.on_pmt(p, s);
                        }
                    }
                }
            }
        }
    }

    fn on_pat(&mut self, s: Vec<u8>) {
        self.cache.insert(PID_PAT, s.clone());
        let mut pat_pids = Vec::new();
        // skip table_id(1) + section_len(2) + tsid(2) + version(1) + sec(1) + last(1)
        let mut i = 8;
        while i + 4 <= s.len() - 4 {
            let prog = u16::from_be_bytes([s[i], s[i + 1]]);
            let pid = (((s[i + 2] & 0x1F) as u16) << 8) | s[i + 3] as u16;
            if prog != 0 {
                pat_pids.push((prog, pid));
            }
            i += 4;
        }
        if pat_pids != self.pat_pids {
            self.pat_pids = pat_pids.clone();
            self.programs.retain(|sid, _| pat_pids.iter().any(|(p, _)| p == sid));
        }
        for (sid, pmt_pid) in pat_pids {
            let entry = self.programs.entry(sid).or_insert(Program {
                service_id: sid,
                pmt_pid,
                pcr_pid: 0x1FFF,
                name: None,
                provider: None,
                streams: Vec::new(),
            });
            entry.pmt_pid = pmt_pid;
        }
    }

    fn on_pmt(&mut self, pmt_pid: u16, s: Vec<u8>) {
        self.cache.insert(pmt_pid, s.clone());
        if s.len() < 12 {
            return;
        }
        let sid = u16::from_be_bytes([s[3], s[4]]);
        let pcr_pid = (((s[8] & 0x1F) as u16) << 8) | s[9] as u16;
        let prog_info_len = (((s[10] & 0x0F) as usize) << 8) | s[11] as usize;
        let mut i = 12 + prog_info_len;
        let mut streams = Vec::new();
        while i + 5 <= s.len() - 4 {
            let st = s[i];
            let epid = (((s[i + 1] & 0x1F) as u16) << 8) | s[i + 2] as u16;
            let es_len = (((s[i + 3] & 0x0F) as usize) << 8) | s[i + 4] as usize;
            let desc = &s[i + 5..(i + 5 + es_len).min(s.len() - 4)];
            streams.push(EsInfo {
                stream_type: st,
                pid: epid,
                kind: stream_kind(st),
                language: language_from_descriptors(desc),
            });
            i += 5 + es_len;
        }
        if let Some(p) = self.programs.get_mut(&sid) {
            p.pmt_pid = pmt_pid;
            p.pcr_pid = pcr_pid;
            p.streams = streams;
        } else {
            self.programs.insert(
                sid,
                Program {
                    service_id: sid,
                    pmt_pid,
                    pcr_pid,
                    name: None,
                    provider: None,
                    streams,
                },
            );
        }
    }

    fn on_sdt(&mut self, s: Vec<u8>) {
        // table_id 0x42/0x46, then tsid, version, section, last, original_network_id, reserved
        if s.len() < 11 {
            return;
        }
        let mut i = 11;
        while i + 5 <= s.len() - 4 {
            let sid = u16::from_be_bytes([s[i], s[i + 1]]);
            let dlen = (((s[i + 3] & 0x0F) as usize) << 8) | s[i + 4] as usize;
            let body = &s[i + 5..(i + 5 + dlen).min(s.len() - 4)];
            if let Some((name, provider)) = parse_service_descriptor(body) {
                if let Some(p) = self.programs.get_mut(&sid) {
                    p.name = Some(name);
                    p.provider = provider;
                }
            }
            i += 5 + dlen;
        }
    }

    /// PAT/PMT sections cached for replay to late joiners, keyed by PID.
    pub fn psi_snapshot(&self) -> Vec<(u16, Vec<u8>)> {
        self.cache.iter().map(|(k, v)| (*k, v.clone())).collect()
    }
}

fn language_from_descriptors(mut d: &[u8]) -> Option<String> {
    while d.len() >= 2 {
        let tag = d[0];
        let len = d[1] as usize;
        if d.len() < 2 + len {
            break;
        }
        let body = &d[2..2 + len];
        if tag == 0x0A && body.len() >= 3 {
            return Some(String::from_utf8_lossy(&body[..3]).to_string());
        }
        d = &d[2 + len..];
    }
    None
}

/// DVB service_descriptor (tag 0x48), as embedded in SDT entries.
fn parse_service_descriptor(d: &[u8]) -> Option<(String, Option<String>)> {
    let mut i = 0;
    while i + 2 <= d.len() {
        let tag = d[i];
        let len = d[i + 1] as usize;
        if i + 2 + len > d.len() {
            break;
        }
        if tag == 0x48 {
            let b = &d[i + 2..i + 2 + len];
            if b.len() < 3 {
                return None;
            }
            let provider_len = b[1] as usize;
            let provider = decode_dvb_string(&b[2..(2 + provider_len).min(b.len())]);
            let mut j = 2 + provider_len;
            if j >= b.len() {
                return Some((String::new(), Some(provider)));
            }
            let name_len = b[j] as usize;
            j += 1;
            let name = decode_dvb_string(&b[j..(j + name_len).min(b.len())]);
            return Some((name, Some(provider)));
        }
        i += 2 + len;
    }
    None
}

/// DVB text: 0x15 = UTF-8, 0x11 = UCS-2 BE, 0x10 = ISO-8859-1, default GB2312-ish.
fn decode_dvb_string(b: &[u8]) -> String {
    if b.is_empty() {
        return String::new();
    }
    match b[0] {
        0x15 => String::from_utf8_lossy(&b[1..]).trim_end_matches('\0').to_string(),
        0x11 => {
            let u16s: Vec<u16> = b[1..]
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&u16s).trim_end_matches('\0').to_string()
        }
        0x10 => b[1..].iter().map(|&c| c as char).collect::<String>().trim_end_matches('\0').to_string(),
        // GB2312: no decoder on hand — pass ASCII through, drop high bytes
        _ => b
            .iter()
            .filter(|&&c| (0x20..0x7F).contains(&c))
            .map(|&c| c as char)
            .collect::<String>()
            .trim_end()
            .to_string(),
    }
}

/// Whether a packet's PID belongs to the wanted set (or carries PSI we must keep).
#[inline]
pub fn keep(pid: u16, wanted: &[u16]) -> bool {
    pid == PID_PAT || pid == PID_SDT || wanted.binary_search(&pid).is_ok()
}

/// Wrap a PSI section into one or more 188-byte TS packets (PUSI set on the first).
pub fn psi_packets(pid: u16, section: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PKT * 2);
    let mut first = true;
    let mut off = 0usize;
    let mut cc = 0u8;
    while off < section.len() || first {
        let mut pkt = [0xFFu8; PKT];
        pkt[0] = SYNC;
        let pusi: u8 = if first { 0x40 } else { 0x00 };
        pkt[1] = pusi | (((pid >> 8) as u8) & 0x1F);
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x10 | (cc & 0x0F);
        let mut idx = 4;
        if first {
            pkt[4] = 0; // pointer_field
            idx = 5;
        }
        let n = (PKT - idx).min(section.len() - off);
        pkt[idx..idx + n].copy_from_slice(&section[off..off + n]);
        out.extend_from_slice(&pkt);
        off += n;
        first = false;
        cc = cc.wrapping_add(1);
        if off >= section.len() {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(pid: u16, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0xFFu8; PKT];
        p[0] = SYNC;
        p[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
        p[2] = (pid & 0xFF) as u8;
        p[3] = 0x10;
        let n = payload.len().min(PKT - 5);
        p[4] = 0;
        p[5..5 + n].copy_from_slice(&payload[..n]);
        p
    }

    #[test]
    fn aligns_after_garbage() {
        let mut aligner = Aligner::default();
        let mut stream = vec![0u8; 300]; // junk
        for i in 0..10u16 {
            stream.extend_from_slice(&packet(0x100 + i, &[i as u8; 20]));
        }
        let mut out = Vec::new();
        aligner.push(&stream, &mut out);
        assert_eq!(out.len() % PKT, 0);
        assert!(out.len() >= PKT * 9);
        assert_eq!(aligner.resyncs, 1);
        for pkt in out.chunks(PKT) {
            assert_eq!(pkt[0], SYNC);
        }
    }

    #[test]
    fn holds_partial_packet_across_calls() {
        let mut aligner = Aligner::default();
        let mut out = Vec::new();
        let mut stream = Vec::new();
        for i in 0..20u16 {
            stream.extend_from_slice(&packet(0x200 + i, &[0u8; 32]));
        }
        // feed in odd-sized slices
        for chunk in stream.chunks(37) {
            aligner.push(chunk, &mut out);
        }
        assert_eq!(out.len(), 20 * PKT);
    }

    #[test]
    fn psi_packet_is_valid() {
        let section = [
            0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xE1, 0x00, 0x00, 0x00,
            0x00,
        ];
        let pkt = psi_packets(PID_PAT, &section);
        assert_eq!(pkt.len(), PKT);
        assert_eq!(pkt[0], SYNC);
        assert_eq!(pid(&pkt), PID_PAT);
        assert_eq!(pkt[1] & 0x40, 0x40); // PUSI
        assert_eq!(pkt[4], 0); // pointer_field
        assert_eq!(&pkt[5..5 + section.len()], &section);
    }
}
