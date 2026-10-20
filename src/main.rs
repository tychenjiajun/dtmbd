use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::time::{Duration, Instant};

mod hub;
mod proto;
mod serve;
mod ts;
mod usb;

use proto::{freq_from_mhz, Status};
use usb::Dtmb;

#[derive(Parser)]
#[command(name = "dtmb", version, about = "DTMB USB dongle → MPEG-TS (cross-platform, no kernel driver)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show device info: USB descriptors, firmware version, variant.
    Info,
    /// Scan the DTMB channel raster and report locked muxes.
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
    },
    /// Tune one frequency and dump raw MPEG-TS to a file (or /dev/null).
    Dump {
        #[arg(long, default_value_t = 698.0)]
        freq_mhz: f64,
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
        #[arg(long, default_value = "capture.ts")]
        out: String,
        /// Also write a hexdump-ish summary of the first sync bytes.
        #[arg(long)]
        stats: bool,
    },
    /// Tune (or autotune) and serve MPEG-TS over HTTP + optionally UDP.
    Serve {
        /// Omit to autotune the whole 474–866 MHz raster.
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
    },
    /// Parse a recorded .ts file and print its programs (no hardware needed).
    Tsinfo {
        file: String,
        #[arg(long, default_value_t = 0.0)]
        seconds: f64,
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
        } => scan(start_mhz, end_mhz, step_mhz, dwell_ms),
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
        } => serve_cmd(freq_mhz, http, udp, record),
        Cmd::Tsinfo { file, seconds } => tsinfo(&file, seconds),
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

fn scan(start_mhz: f64, end_mhz: f64, step_mhz: f64, dwell_ms: u64) -> Result<()> {
    let mut d = Dtmb::open()?;
    println!(
        "firmware V{} ({:?})  scanning {start_mhz:.0}–{end_mhz:.0} MHz step {step_mhz:.0} MHz",
        d.version.to_string(),
        d.variant
    );
    d.power(true)?;

    let step = (step_mhz * 1_000_000.0).round() as u32;
    let mut mhz = start_mhz;
    let mut hits: Vec<Status> = Vec::new();
    while mhz <= end_mhz + 0.001 {
        let hz = freq_from_mhz(mhz);
        let t0 = Instant::now();
        let st = d.tune(hz, Duration::from_millis(dwell_ms))?;
        let tag = if st.locked { "LOCK" } else { "    " };
        println!(
            "{:>7.3} MHz  {tag}  str {:>3}%  snr {:>5.1} dB  {} {} {} {} {}  ({} ms)",
            mhz,
            st.strength_pct,
            st.snr_db,
            st.info.carrier(),
            st.info.guard_interval(),
            st.info.fec(),
            st.info.interleaving(),
            st.info.modulation(),
            t0.elapsed().as_millis()
        );
        if st.locked {
            hits.push(st);
        }
        mhz += step as f64 / 1_000_000.0;
    }

    println!("\n{} locked mux(es):", hits.len());
    for h in &hits {
        println!(
            "  {:.3} MHz  strength {}%  snr {:.1} dB  quality {}",
            h.tuned_hz as f64 / 1e6,
            h.strength_pct,
            h.snr_db,
            h.quality
        );
    }
    d.close();
    Ok(())
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

fn log_line(msg: &str) {
    println!("{msg}");
}

fn serve_cmd(freq_mhz: Option<f64>, http: String, udp: Option<String>, record: Option<String>) -> Result<()> {
    let hub = hub::Hub::new();
    let cfg = hub::CaptureConfig {
        freq_hz: freq_mhz.map(freq_from_mhz),
        record: record.map(std::path::PathBuf::from),
        log: log_line,
    };
    hub::spawn(hub.clone(), cfg);

    // wait up to 8 s for the first lock so the banner is truthful
    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(100));
        let (freq, st, progs, mbps, ..) = hub.snapshot();
        if freq != 0 && st.locked {
            println!(
                "locked {:.3} MHz | {} program(s) | {:.2} Mbit/s",
                freq as f64 / 1e6,
                progs.len(),
                mbps
            );
            break;
        }
    }

    serve::run(
        hub,
        serve::ServeConfig {
            listen: http.parse().context("parsing --http address")?,
            udp,
        },
    )
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
        "{}: {} bytes in {:.1}s ({:.2} Mbit/s), {} packets, {} resync(s)",
        path,
        total,
        secs,
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
