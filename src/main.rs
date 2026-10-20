use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::sync::mpsc;
use std::time::{Duration, Instant};

mod hub;
mod proto;
mod registry;
mod serve;
mod ts;
mod usb;

use proto::{freq_from_mhz, Status};
use usb::Dtmb;

#[derive(Parser)]
#[command(name = "dtmb", version, about = "DTMB USB dongle → MPEG-TS / IPTV (cross-platform, no kernel driver)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show device info: USB descriptors, firmware version, variant.
    Info,
    /// Scan the DTMB channel raster, optionally saving a channel table.
    Scan {
        #[arg(long, default_value_t = 474.0)]
        start_mhz: f64,
        #[arg(long, default_value_t = 866.0)]
        end_mhz: f64,
        #[arg(long, default_value_t = 8.0)]
        step_mhz: f64,
        /// How long to wait for demod lock per channel.
        #[arg(long, default_value_t = 600)]
        dwell_ms: u64,
        /// Seconds of TS to collect per locked mux to learn its programs.
        #[arg(long, default_value_t = 1.5)]
        grab_secs: f64,
        /// Where to write the channel table (default: ~/.dtmb/channels.json).
        #[arg(long)]
        save: Option<String>,
        /// Name patch (JSON) for muxes that carry no SDT.
        #[arg(long)]
        names: Option<String>,
    },
    /// Tune one frequency and dump raw MPEG-TS to a file.
    Dump {
        #[arg(long, default_value_t = 698.0)]
        freq_mhz: f64,
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
        #[arg(long, default_value = "capture.ts")]
        out: String,
        #[arg(long)]
        stats: bool,
    },
    /// Serve MPEG-TS over HTTP + optionally UDP (single tuner, zaps on demand).
    Serve {
        /// Starting frequency; when omitted the channel table (or autotune) decides.
        #[arg(long)]
        freq_mhz: Option<f64>,
        /// HTTP listen address.
        #[arg(long, default_value = "0.0.0.0:8080")]
        http: String,
        /// Also push MPEG-TS to this UDP target (e.g. 239.255.0.1:5000).
        #[arg(long)]
        udp: Option<String>,
        /// Record the multiplex to this file while serving.
        #[arg(long)]
        record: Option<String>,
        /// Channel table produced by `dtmb scan` (default: ~/.dtmb/channels.json).
        #[arg(long)]
        table: Option<String>,
        /// Base URL written into the playlist (default: derived from --http).
        #[arg(long)]
        public_base: Option<String>,
    },
    /// Parse a recorded .ts file and print its programs (no hardware needed).
    Tsinfo {
        file: String,
        #[arg(long, default_value_t = 0.0)]
        seconds: f64,
    },
    /// Debug: measure lock reliability for a frequency with different kick payloads.
    #[command(hide = true)]
    Tunetest {
        #[arg(long, default_value_t = 698.0)]
        freq_mhz: f64,
        #[arg(long, default_value_t = 8)]
        repeats: u32,
        #[arg(long, default_value_t = 4)]
        kick: usize,
        #[arg(long, default_value_t = 2000)]
        budget_ms: u64,
        /// Power-cycle the frontend before each try.
        #[arg(long)]
        pc: bool,
    },
    /// Print the playlist a `serve` instance would expose (reads the table only).
    Playlist {
        #[arg(long)]
        table: Option<String>,
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        base: String,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dtmb=info".into()),
        )
        .init();

    match Cli::parse().cmd {
        Cmd::Info => info(),
        Cmd::Scan {
            start_mhz,
            end_mhz,
            step_mhz,
            dwell_ms,
            grab_secs,
            save,
            names,
        } => scan(start_mhz, end_mhz, step_mhz, dwell_ms, grab_secs, save, names),
        Cmd::Dump {
            freq_mhz,
            seconds,
            out,
            stats,
        } => dump(freq_mhz, seconds, &out, stats),
        Cmd::Serve {
            freq_mhz,
            http,
            udp,
            record,
            table,
            public_base,
        } => serve_cmd(freq_mhz, http, udp, record, table, public_base),
        Cmd::Tsinfo { file, seconds } => tsinfo(&file, seconds),
        Cmd::Tunetest {
            freq_mhz,
            repeats,
            kick,
            budget_ms,
            pc,
        } => tunetest(freq_mhz, repeats, kick, budget_ms, pc),
        Cmd::Playlist { table, base } => playlist_cmd(table, base),
    }
}

fn info() -> Result<()> {
    println!("{}", Dtmb::describe_all()?.trim_end());
    let mut d = Dtmb::open()?;
    println!(
        "\nfirmware: V{}   variant: {:?}",
        d.version.to_string(),
        d.variant
    );
    d.power(true)?;
    let st = d.tune(freq_from_mhz(698.0), Duration::from_millis(600))?;
    print_status(&st);
    d.close();
    Ok(())
}

fn scan(
    start_mhz: f64,
    end_mhz: f64,
    step_mhz: f64,
    dwell_ms: u64,
    grab_secs: f64,
    save: Option<String>,
    names: Option<String>,
) -> Result<()> {
    let mut d = Dtmb::open()?;
    println!(
        "firmware V{} ({:?})  scanning {start_mhz:.0}–{end_mhz:.0} MHz step {step_mhz:.0} MHz",
        d.version.to_string(),
        d.variant
    );
    d.power(true)?;

    let mut table = registry::ChannelTable::default();
    let step = (step_mhz * 1_000_000.0).round() as u32;
    let mut mhz = start_mhz;
    let mut found = 0usize;
    // the demod keeps reporting the *previous* mux after a failed retune, so a
    // locked bit is not trustworthy: require real TS bytes and a fresh PAT
    let mut prev_pat: Option<Vec<u8>> = None;
    while mhz <= end_mhz + 0.001 {
        let hz = freq_from_mhz(mhz);
        let t0 = Instant::now();
        d.power_cycle();
        let st = d.tune(hz, Duration::from_millis(dwell_ms))?;
        let mut grab = grab_mux(&mut d, grab_secs.max(0.5))?;
        // 2 KB of FIFO residue is not a signal: retry once from a clean power state
        if grab.bytes < 8192 {
            d.power_cycle();
            d.tune(hz, Duration::from_millis(dwell_ms))?;
            grab = grab_mux(&mut d, grab_secs.max(0.5))?;
        }
        let fresh = grab.bytes > 0 && grab.pat.is_some() && grab.pat != prev_pat;
        let programs = if fresh { grab.programs } else { Vec::new() };
        let tag = match (st.locked, fresh) {
            (_, true) => "LOCK",
            (true, false) => "STALE",
            (false, false) => "     ",
        };
        println!(
            "{:>7.3} MHz  {tag}  str {:>3}%  snr {:>5.1} dB  tsid {:>5}  {} {} {} {} {}  {} ch  ({} ms) [{} B]",
            mhz,
            st.strength_pct,
            st.snr_db,
            grab.tsid
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".into()),
            st.info.carrier(),
            st.info.guard_interval(),
            st.info.fec(),
            st.info.interleaving(),
            st.info.modulation(),
            programs.len(),
            t0.elapsed().as_millis(),
            grab.bytes
        );
        for p in &programs {
            println!(
                "           └ {:<10} sid {:<5} pmt {:<5}  {}",
                p.name.clone().unwrap_or_else(|| "(no SDT)".into()),
                p.service_id,
                p.pmt_pid,
                p.streams
                    .iter()
                    .map(|e| format!("{}:{}", e.kind, e.pid))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        if fresh {
            found += 1;
            prev_pat = grab.pat.clone();
            table.add_mux(hz, st, programs);
        }
        mhz += step as f64 / 1_000_000.0;
    }

    if let Some(np) = names {
        let data = std::fs::read_to_string(&np).with_context(|| format!("reading {np}"))?;
        let patch: registry::NamePatch = serde_json::from_str(&data)
            .with_context(|| format!("parsing name patch {np}"))?;
        let changed = table.apply_names(&patch);
        println!("names: applied {changed} name(s) from {np}");
    }

    let path = save
        .map(std::path::PathBuf::from)
        .unwrap_or_else(registry::default_table_path);
    table.save(&path)?;
    println!(
        "\n{found} locked mux(es), {} channel(s) → {}",
        table.channels().count(),
        path.display()
    );
    d.close();
    Ok(())
}

struct Grab {
    bytes: u64,
    programs: Vec<ts::Program>,
    pat: Option<Vec<u8>>,
    tsid: Option<u16>,
}

/// Stream a mux for a moment and report what actually arrived.
fn grab_mux(d: &mut Dtmb, secs: f64) -> Result<Grab> {
    let mut aligner = ts::Aligner::default();
    let mut analyzer = ts::Analyzer::default();
    let mut buf = vec![0u8; 64 * 1024];
    d.stream(true)?;
    let mut bytes = 0u64;
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < secs {
        let n = d.read_ts(&mut buf, Duration::from_millis(250))?;
        if n == 0 {
            continue;
        }
        bytes += n as u64;
        let mut aligned = Vec::with_capacity(n + ts::PKT);
        aligner.push(&buf[..n], &mut aligned);
        analyzer.feed(&aligned);
    }
    d.stream(false)?;
    Ok(Grab {
        bytes,
        programs: analyzer.programs.values().cloned().collect(),
        pat: analyzer.last_pat.clone(),
        tsid: analyzer.tsid,
    })
}

fn dump(freq_mhz: f64, seconds: f64, out: &str, stats: bool) -> Result<()> {
    let mut d = Dtmb::open()?;
    println!("firmware V{} ({:?})", d.version.to_string(), d.variant);
    d.power(true)?;
    let st = d.tune(freq_from_mhz(freq_mhz), Duration::from_millis(900))?;
    print_status(&st);
    if !st.locked {
        eprintln!("warning: no demod lock — stream may be empty");
    }

    d.stream(true)?;
    let mut file = std::fs::File::create(out).with_context(|| format!("creating {out}"))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    let mut sync_ok: u64 = 0;
    let mut sync_bad: u64 = 0;
    let mut zero_reads: u64 = 0;
    let t0 = Instant::now();
    let mut next_report = t0 + Duration::from_secs(1);

    while t0.elapsed().as_secs_f64() < seconds {
        let n = d.read_ts(&mut buf, Duration::from_millis(500))?;
        if n == 0 {
            zero_reads += 1;
            continue;
        }
        std::io::Write::write_all(&mut file, &buf[..n])?;
        total += n as u64;
        if stats {
            let mut off = 0;
            while off + 188 <= n {
                if buf[off] == 0x47 {
                    sync_ok += 1;
                } else {
                    sync_bad += 1;
                }
                off += 188;
            }
        }
        if Instant::now() >= next_report {
            let secs = t0.elapsed().as_secs_f64();
            println!(
                "  {:5.1}s  {:8.2} Mbit/s  {:6.2} MB  empty_reads {}",
                secs,
                total as f64 * 8.0 / secs / 1e6,
                total as f64 / 1e6,
                zero_reads
            );
            next_report = Instant::now() + Duration::from_secs(1);
        }
    }

    let secs = t0.elapsed().as_secs_f64();
    println!(
        "\nwrote {out}: {:.2} MB in {:.1}s = {:.3} Mbit/s",
        total as f64 / 1e6,
        secs,
        total as f64 * 8.0 / secs / 1e6
    );
    if stats {
        let tot = sync_ok + sync_bad;
        println!(
            "TS sync: {sync_ok}/{tot} aligned ({:.2}%)",
            if tot > 0 {
                sync_ok as f64 * 100.0 / tot as f64
            } else {
                0.0
            }
        );
    }
    d.close();
    Ok(())
}

fn log_line(msg: &str) {
    println!("{msg}");
}

fn serve_cmd(
    freq_mhz: Option<f64>,
    http: String,
    udp: Option<String>,
    record: Option<String>,
    table: Option<String>,
    public_base: Option<String>,
) -> Result<()> {
    let table_path = table
        .map(std::path::PathBuf::from)
        .unwrap_or_else(registry::default_table_path);
    let table = if table_path.exists() {
        let t = registry::ChannelTable::load(&table_path)?;
        println!(
            "table: {} ({} channels, {} muxes)",
            table_path.display(),
            t.channels().count(),
            t.muxes.len()
        );
        t
    } else {
        println!("table: {} not found — run `dtmb scan` for a cross-mux playlist", table_path.display());
        registry::ChannelTable::default()
    };

    let listen: std::net::SocketAddr = http.parse().context("parsing --http address")?;
    let base = public_base.unwrap_or_else(|| {
        let host = if listen.ip().is_unspecified() {
            "127.0.0.1".to_string()
        } else {
            listen.ip().to_string()
        };
        format!("http://{host}:{}", listen.port())
    });

    // start on the requested frequency, else the first table mux, else autotune
    let start_freq = freq_mhz
        .map(freq_from_mhz)
        .or_else(|| table.muxes.first().map(|m| m.freq_hz));

    let (tune_tx, tune_rx) = mpsc::channel();
    let hub = hub::Hub::new(tune_tx);
    let cfg = hub::CaptureConfig {
        freq_hz: start_freq,
        record: record.map(std::path::PathBuf::from),
        log: log_line,
    };
    hub::spawn(hub.clone(), tune_rx, cfg);

    // wait up to 8 s for the first lock so the banner is truthful
    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(100));
        let snap = hub.snapshot();
        if snap.freq_hz != 0 && snap.status.locked {
            println!(
                "locked {:.3} MHz | {} program(s) | {:.2} Mbit/s",
                snap.freq_hz as f64 / 1e6,
                snap.programs.len(),
                snap.bitrate_mbps
            );
            break;
        }
    }

    serve::run(
        hub,
        serve::ServeConfig {
            listen,
            udp,
            table: std::sync::Arc::new(table),
            public_base: base,
        },
    )
}

fn playlist_cmd(table: Option<String>, base: String) -> Result<()> {
    let path = table
        .map(std::path::PathBuf::from)
        .unwrap_or_else(registry::default_table_path);
    let t = registry::ChannelTable::load(&path)?;
    print!("{}", t.m3u(&base));
    Ok(())
}

fn tsinfo(path: &str, seconds: f64) -> Result<()> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {path}"))?;
    let mut aligner = ts::Aligner::default();
    let mut analyzer = ts::Analyzer::default();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    let limit = if seconds > 0.0 { Some(seconds) } else { None };
    let start = Instant::now();
    loop {
        if let Some(l) = limit {
            if start.elapsed().as_secs_f64() > l {
                break;
            }
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let mut aligned = Vec::with_capacity(n + ts::PKT);
        aligner.push(&buf[..n], &mut aligned);
        analyzer.feed(&aligned);
        total += aligned.len() as u64;
    }
    let secs = start.elapsed().as_secs_f64().max(1e-3);
    println!(
        "{}: {} bytes ({:.2} Mbit/s), {} packets, {} resync(s)",
        path,
        total,
        total as f64 * 8.0 / secs / 1e6,
        analyzer.packet_count,
        aligner.resyncs
    );
    println!("programs: {}", analyzer.programs.len());
    for p in analyzer.programs.values() {
        println!(
            "  service {}  pmt {}  pcr {}  {}{}",
            p.service_id,
            p.pmt_pid,
            p.pcr_pid,
            p.name.clone().unwrap_or_else(|| "(no SDT)".into()),
            p.provider
                .as_ref()
                .map(|pr| format!("  [{pr}]"))
                .unwrap_or_default()
        );
        for e in &p.streams {
            println!(
                "     type 0x{:02x}  pid {:5}  {}  {}",
                e.stream_type,
                e.pid,
                e.kind,
                e.language.clone().unwrap_or_default()
            );
        }
    }
    Ok(())
}

fn print_status(st: &Status) {
    println!(
        "tuned {:.3} MHz  lock={} signal={} strength={}% snr={:.1} dB  [carrier {} | GI {} | FEC {} | IL {} | {} ]",
        st.tuned_hz as f64 / 1e6,
        st.locked as u8,
        st.has_signal as u8,
        st.strength_pct,
        st.snr_db,
        st.info.carrier(),
        st.info.guard_interval(),
        st.info.fec(),
        st.info.interleaving(),
        st.info.modulation()
    );
}

fn tunetest(freq_mhz: f64, repeats: u32, kick: usize, budget_ms: u64, pc: bool) -> Result<()> {
    let mut d = Dtmb::open()?;
    let hz = freq_from_mhz(freq_mhz);
    println!(
        "kick={kick}B freq={freq_mhz} MHz, {repeats} tries (power-cycle between tries)"
    );
    let mut ok = 0;
    for i in 0..repeats {
        if pc {
            d.power_cycle();
        }
        d.set_freq_variant(hz, kick)?;
        let tt = d.time_to_lock(Duration::from_millis(budget_ms));
        match tt {
            Some(ms) => {
                ok += 1;
                let s = d.strength_pct().unwrap_or(0);
                let snr = d.snr_db().unwrap_or(0.0);
                d.stream(true)?;
                let mut buf = vec![0u8; 64 * 1024];
                let mut bytes = 0usize;
                let t = Instant::now();
                while t.elapsed() < Duration::from_millis(700) {
                    if let Ok(n) = d.read_ts(&mut buf, Duration::from_millis(150)) {
                        bytes += n;
                    }
                }
                d.stream(false)?;
                println!("  #{i}: lock in {ms} ms  {s}%  {snr:.1} dB  data {bytes} B");
            }
            None => println!("  #{i}: no lock within {budget_ms} ms"),
        }
    }
    println!("{ok}/{repeats} locked");
    d.close();
    Ok(())
}
