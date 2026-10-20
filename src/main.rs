//! dtmb — a Raspberry-Pi sized DTMB → IPTV box.
//!
//!   天线 → 电视棒(USB) → MPEG-TS → HTTP → 安卓电视上的 IPTV 播放器
//!
//! Only two commands exist on purpose:
//!   `dtmb scan`  — build/refresh the channel table (first run does it automatically)
//!   `dtmb serve` — the daemon systemd starts at boot
//!
//! Layering (each layer only talks to the one below):
//!   L1 usb/    vendor control protocol + bulk TS transport (libusb)
//!   L2 ts     188-byte alignment, PAT/PMT/SDT, PID filtering
//!   L3 tuner   single-tuner worker: lazy tune, zapping, recovery, fan-out
//!   L4 catalog channel table, playlist, SDT-less name patches
//!   L5 http    playlist + per-service MPEG-TS endpoints for the TV app
//!   L6 mdns    `dtmb.local` discovery
//!   main       CLI, first-run scan, wiring

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

mod catalog;
mod http;
mod mdns;
mod ts;
mod tuner;
mod usb;

use catalog::ChannelTable;
use usb::Device;

#[derive(Parser)]
#[command(name = "dtmb", version, about = "DTMB USB stick → IPTV over your LAN")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scan the band and write the channel table (first run does this automatically).
    Scan {
        #[arg(long, default_value_t = 474.0)]
        start_mhz: f64,
        #[arg(long, default_value_t = 866.0)]
        end_mhz: f64,
        #[arg(long, default_value_t = 8.0)]
        step_mhz: f64,
        /// How long to wait for demod lock per channel.
        #[arg(long, default_value_t = 900)]
        dwell_ms: u64,
        /// Seconds of TS to collect per mux to learn its programme list.
        #[arg(long, default_value_t = 1.5)]
        grab_secs: f64,
        /// Name patch for muxes that carry no SDT (see presets/).
        #[arg(long)]
        names: Option<PathBuf>,
        #[arg(long)]
        save: Option<PathBuf>,
    },
    /// Antenna meter: live signal readings on one frequency, or a fast sweep
    /// to find the strongest mux while aiming the antenna.
    Meter {
        /// Frequency to watch. Omit (or pass --scan) to sweep the band.
        #[arg(long)]
        freq_mhz: Option<f64>,
        /// Sweep the band and rank every mux by signal quality.
        #[arg(long)]
        scan: bool,
        #[arg(long, default_value_t = 474.0)]
        start_mhz: f64,
        #[arg(long, default_value_t = 866.0)]
        end_mhz: f64,
        #[arg(long, default_value_t = 8.0)]
        step_mhz: f64,
        /// How long to wait for lock on each frequency while sweeping.
        #[arg(long, default_value_t = 700)]
        settle_ms: u64,
        /// Live refresh interval.
        #[arg(long, default_value_t = 300)]
        interval_ms: u64,
        /// Machine readable output (sweep only).
        #[arg(long)]
        json: bool,
    },
    /// Run the IPTV server (this is what systemd starts).
    Serve {
        #[arg(long, default_value = "0.0.0.0:8080")]
        bind: String,
        /// Channel table; defaults to ~/.dtmb/channels.json.
        #[arg(long)]
        table: Option<PathBuf>,
        /// Name patch used only when the channel table has to be built on first run.
        #[arg(long)]
        names: Option<PathBuf>,
        /// Hostname advertised over mDNS and used in the playlist URL.
        #[arg(long, default_value = "dtmb")]
        hostname: String,
        #[arg(long)]
        no_mdns: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Scan {
            start_mhz,
            end_mhz,
            step_mhz,
            dwell_ms,
            grab_secs,
            names,
            save,
        } => {
            let table = scan(ScanOpts {
                start_mhz,
                end_mhz,
                step_mhz,
                dwell_ms,
                grab_secs,
                names,
            })?;
            let path = save.unwrap_or_else(catalog::default_table_path);
            table.save(&path)?;
            println!(
                "{} channel(s) in {} mux(es) → {}",
                table.channels().count(),
                table.muxes.len(),
                path.display()
            );
            Ok(())
        }
        Cmd::Meter {
            freq_mhz,
            scan,
            start_mhz,
            end_mhz,
            step_mhz,
            settle_ms,
            interval_ms,
            json,
        } => meter(MeterOpts {
            freq_mhz,
            scan,
            start_mhz,
            end_mhz,
            step_mhz,
            settle_ms,
            interval_ms,
            json,
        }),
        Cmd::Serve {
            bind,
            table,
            names,
            hostname,
            no_mdns,
        } => serve(bind, table, names, hostname, no_mdns),
    }
}

// ---------------------------------------------------------------------- serve

fn serve(
    bind: String,
    table_path: Option<PathBuf>,
    names: Option<PathBuf>,
    hostname: String,
    no_mdns: bool,
) -> Result<()> {
    let listen: std::net::SocketAddr = bind.parse().context("parsing --bind")?;
    let table_path = table_path.unwrap_or_else(catalog::default_table_path);

    let table = if table_path.exists() {
        let t = ChannelTable::load(&table_path)?;
        println!(
            "table: {} channel(s) / {} mux(es) from {}",
            t.channels().count(),
            t.muxes.len(),
            table_path.display()
        );
        t
    } else {
        println!("no channel table yet — scanning once (takes a few minutes)");
        let t = scan(ScanOpts {
            names,
            ..ScanOpts::default()
        })?;
        t.save(&table_path)?;
        t
    };

    let lan_ip = mdns::local_ip();
    let base = if no_mdns {
        format!("http://{lan_ip}:{}", listen.port())
    } else {
        format!("http://{hostname}.local:{}", listen.port())
    };
    if !no_mdns {
        mdns::advertise(&hostname, listen.port(), &lan_ip);
    }
    if base.contains(".local") {
        println!("also reachable at http://{lan_ip}:{}/playlist.m3u", listen.port());
    }

    let tuner = tuner::Tuner::spawn(log);
    http::run(tuner, Arc::new(table), listen, base)
}

fn log(msg: &str) {
    println!("{msg}");
}

// ----------------------------------------------------------------------- scan

struct ScanOpts {
    start_mhz: f64,
    end_mhz: f64,
    step_mhz: f64,
    dwell_ms: u64,
    grab_secs: f64,
    names: Option<PathBuf>,
}

impl Default for ScanOpts {
    fn default() -> Self {
        Self {
            start_mhz: usb::proto::FREQ_MIN_HZ as f64 / 1e6,
            end_mhz: usb::proto::FREQ_MAX_HZ as f64 / 1e6,
            step_mhz: usb::proto::FREQ_STEP_HZ as f64 / 1e6,
            dwell_ms: 900,
            grab_secs: 1.5,
            names: None,
        }
    }
}

struct Grab {
    bytes: u64,
    programs: Vec<ts::Program>,
    pat: Option<Vec<u8>>,
}

fn scan(opts: ScanOpts) -> Result<ChannelTable> {
    let mut dev = Device::open()?;
    dev.power_on()?;
    println!(
        "scanning {:.0}–{:.0} MHz step {:.0} MHz  (firmware V{}, {:?})",
        opts.start_mhz,
        opts.end_mhz,
        opts.step_mhz,
        dev.version.to_string(),
        dev.variant
    );

    let mut table = ChannelTable::default();
    let step = (opts.step_mhz * 1_000_000.0).round() as u32;
    let mut mhz = opts.start_mhz;
    // The demod keeps reporting the previous mux after a failed retune, so the
    // lock bit alone is not trustworthy: require real TS bytes and a fresh PAT.
    let mut prev_pat: Option<Vec<u8>> = None;

    while mhz <= opts.end_mhz + 0.001 {
        let hz = (mhz * 1_000_000.0).round() as u32;
        let t0 = Instant::now();
        dev.power_cycle();
        dev.set_freq(hz)?;
        let mut locked = dev.wait_lock(Duration::from_millis(opts.dwell_ms));
        if !locked {
            // the first tune after a power cycle is occasionally slow
            dev.power_cycle();
            dev.set_freq(hz)?;
            locked = dev.wait_lock(Duration::from_millis(1500));
        }
        if !locked {
            println!("{mhz:>7.3} MHz  -");
            mhz += step as f64 / 1_000_000.0;
            continue;
        }

        let mut grab = grab_mux(&mut dev, opts.grab_secs.max(0.5))?;
        // 2 KB of FIFO residue is not a signal: retry once from a clean state
        if grab.bytes < 8192 {
            dev.power_cycle();
            dev.set_freq(hz)?;
            dev.wait_lock(Duration::from_millis(1500));
            grab = grab_mux(&mut dev, opts.grab_secs.max(0.5))?;
        }

        let fresh = grab.bytes >= 8192 && grab.pat.is_some() && grab.pat != prev_pat;
        if !fresh {
            println!("{mhz:>7.3} MHz  -");
        } else {
            prev_pat = grab.pat.clone();
            let mut st = dev.status();
            st.tuned_hz = hz;
            println!(
                "{mhz:>7.3} MHz  {} ch  snr {:.1} dB  ({} ms)",
                grab.programs.len(),
                st.snr_db,
                t0.elapsed().as_millis()
            );
            for p in &grab.programs {
                println!(
                    "           └ {:<16} sid {:<5} {}",
                    p.name.clone().unwrap_or_else(|| "(no SDT)".into()),
                    p.service_id,
                    p.streams
                        .iter()
                        .map(|e| format!("{}:{}", e.kind, e.pid))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
            table.add_mux(hz, st, grab.programs);
        }
        mhz += step as f64 / 1_000_000.0;
    }

    if let Some(p) = opts.names.as_ref().filter(|p| p.exists()) {
        apply_names(&mut table, p)?;
    }
    dev.close();
    Ok(table)
}

fn grab_mux(dev: &mut Device, secs: f64) -> Result<Grab> {
    let mut aligner = ts::Aligner::default();
    let mut analyzer = ts::Analyzer::default();
    let mut buf = vec![0u8; 64 * 1024];
    dev.stream(true)?;
    let mut bytes = 0u64;
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < secs {
        let n = dev.read_ts(&mut buf, Duration::from_millis(250))?;
        if n == 0 {
            continue;
        }
        bytes += n as u64;
        let mut aligned = Vec::with_capacity(n + ts::PKT);
        aligner.push(&buf[..n], &mut aligned);
        analyzer.feed(&aligned);
    }
    dev.stream(false)?;
    Ok(Grab {
        bytes,
        programs: analyzer.programs.values().cloned().collect(),
        pat: analyzer.last_pat.clone(),
    })
}

fn apply_names(table: &mut ChannelTable, path: &PathBuf) -> Result<()> {
    let data =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let patch: catalog::NamePatch = serde_json::from_str(&data)
        .with_context(|| format!("parsing name patch {}", path.display()))?;
    let n = table.apply_names(&patch);
    println!("names: {n} channel(s) renamed from {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------------- meter

struct MeterOpts {
    freq_mhz: Option<f64>,
    scan: bool,
    start_mhz: f64,
    end_mhz: f64,
    step_mhz: f64,
    settle_ms: u64,
    interval_ms: u64,
    json: bool,
}

fn bar(pct: f32, width: usize) -> String {
    let filled = ((pct.clamp(0.0, 100.0) / 100.0) * width as f32).round() as usize;
    format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(width - filled))
}

fn meter(opts: MeterOpts) -> Result<()> {
    let mut dev = Device::open()?;
    dev.power_on()?;

    if opts.scan || opts.freq_mhz.is_none() {
        return sweep(&mut dev, &opts);
    }

    let hz = (opts.freq_mhz.unwrap() * 1e6).round() as u32;
    let _ = dev.set_freq(hz);
    dev.wait_lock(Duration::from_millis(opts.settle_ms));

    println!("实时测量 {:.3} MHz —— 慢慢转天线，看峰值；Ctrl-C 退出", hz as f64 / 1e6);
    let (mut peak_pct, mut peak_snr) = (0u8, 0f32);
    loop {
        let st = dev.meter_status();
        peak_pct = peak_pct.max(st.strength_pct);
        peak_snr = peak_snr.max(st.snr_db);
        print!(
            "\x1b[2K\r{:.3} MHz  {}  {:>3}% {}  SNR {:>5.1} dB  {} 峰值 {:>3}% / {:.1} dB   {} {} {}",
            hz as f64 / 1e6,
            if st.locked { "LOCK" } else { "----" },
            st.strength_pct,
            bar(st.strength_pct as f32, 16),
            st.snr_db,
            bar(st.snr_db / 30.0 * 100.0, 16),
            peak_pct,
            peak_snr,
            st.info.modulation(),
            st.info.guard_interval(),
            st.info.fec(),
        );
        std::io::Write::flush(&mut std::io::stdout()).ok();
        std::thread::sleep(Duration::from_millis(opts.interval_ms));
    }
}

fn sweep(dev: &mut Device, opts: &MeterOpts) -> Result<()> {
    let mut rows = Vec::new();
    let mut mhz = opts.start_mhz;
    println!(
        "扫频 {:.0}–{:.0} MHz（步进 {:.0}）—— 用于找最佳天线方向",
        opts.start_mhz, opts.end_mhz, opts.step_mhz
    );
    while mhz <= opts.end_mhz + 0.001 {
        let hz = (mhz * 1_000_000.0).round() as u32;
        // a meter keeps going through dead frequencies: never abort the sweep
        let _ = dev.set_freq(hz);
        let mut locked = dev.wait_lock(Duration::from_millis(opts.settle_ms));
        if !locked {
            // a cold frontend occasionally misses the first lock
            dev.power_cycle();
            let _ = dev.set_freq(hz);
            locked = dev.wait_lock(Duration::from_millis(opts.settle_ms));
        }
        let st = dev.meter_status();
        if !opts.json {
            print!(
                "\x1b[2K\r{:>7.3} MHz  {}  {:>3}% {}  SNR {:>5.1} dB",
                mhz,
                if locked { "LOCK" } else { "----" },
                st.strength_pct,
                bar(st.strength_pct as f32, 12),
                st.snr_db
            );
            std::io::Write::flush(&mut std::io::stdout()).ok();
        }
        if locked {
            rows.push((
                hz,
                st.strength_pct,
                st.snr_db,
                st.info.modulation(),
                st.info.guard_interval(),
                st.info.fec(),
            ));
        }
        mhz += opts.step_mhz;
    }
    if !opts.json {
        println!();
    }

    rows.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    if opts.json {
        let arr: Vec<_> = rows
            .iter()
            .map(|(hz, pct, snr, m, gi, fec)| {
                serde_json::json!({
                    "freq_hz": hz,
                    "strength_pct": pct,
                    "snr_db": snr,
                    "modulation": m,
                    "guard_interval": gi,
                    "fec": fec,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&arr)?);
        return Ok(());
    }

    println!("\n锁定 {} 个频点（按 SNR 排序）：", rows.len());
    for (i, (hz, pct, snr, m, gi, fec)) in rows.iter().enumerate() {
        println!(
            "{} {:>7.3} MHz  {:>3}%  SNR {:>5.1} dB  {}  {} {} {}",
            if i == 0 { "★" } else { " " },
            *hz as f64 / 1e6,
            pct,
            snr,
            bar(*snr / 30.0 * 100.0, 20),
            m,
            gi,
            fec
        );
    }
    if let Some((hz, _, snr, ..)) = rows.first() {
        println!(
            "\n最佳：{:.3} MHz（SNR {:.1} dB）—— 转天线到这个方向时看 SNR 最大",
            *hz as f64 / 1e6,
            snr
        );
    } else {
        println!("\n一个都没锁上：检查天线、馈线、供电");
    }
    Ok(())
}
