//! Outputs: HTTP (MPEG-TS over chunked) + UDP push, both fed from the broadcast hub.
//!
//! Single tuner semantics: `/p/<sid>.ts` retunes the stick if the requested
//! service lives on another multiplex, then streams it. Clients connected to the
//! old mux are dropped via the hub epoch.

use anyhow::{Context as AnyhowContext, Result};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::hub::Hub;
use crate::registry::ChannelTable;
use crate::ts::{self, PKT};

pub struct ServeConfig {
    pub listen: SocketAddr,
    pub udp: Option<String>,
    pub table: Arc<ChannelTable>,
    /// Public base url used when generating the playlist.
    pub public_base: String,
}

#[derive(Clone)]
struct AppState {
    hub: Arc<Hub>,
    table: Arc<ChannelTable>,
    public_base: String,
}

pub fn run(hub: Arc<Hub>, cfg: ServeConfig) -> Result<()> {
    if let Some(target) = &cfg.udp {
        spawn_udp(hub.clone(), target.clone());
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let state = AppState {
        hub: hub.clone(),
        table: cfg.table.clone(),
        public_base: cfg.public_base.clone(),
    };
    rt.block_on(async move {
        let app = Router::new()
            .route("/", get(index))
            .route("/status.json", get(status_json))
            .route("/api/channels", get(api_channels))
            .route("/playlist.m3u", get(playlist))
            .route("/stream.ts", get(full_mux))
            .route("/p/{*path}", get(program_ts))
            .route("/healthz", get(|| async { "ok" }))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(cfg.listen)
            .await
            .with_context(|| format!("binding {}", cfg.listen))?;
        println!(
            "HTTP:  {}/playlist.m3u   ({} channels, {} muxes)",
            cfg.public_base,
            cfg.table.channels().count(),
            cfg.table.muxes.len()
        );
        println!("       {}/stream.ts      (current mux)", cfg.public_base);
        if let Some(u) = &cfg.udp {
            println!("UDP:   pushing MPEG-TS to {u}");
        }
        axum::serve(listener, app).await?;
        Ok::<(), anyhow::Error>(())
    })
}

// ------------------------------------------------------------------ handlers

async fn index(State(st): State<AppState>) -> Html<String> {
    let snap = st.hub.snapshot();
    let current = snap.freq_hz;
    let mut rows = String::new();
    for ch in st.table.channels() {
        let live = if ch.freq_hz == current { "●" } else { "" };
        let tracks = ch
            .streams
            .iter()
            .map(|e| {
                let lang = e.language.clone().unwrap_or_default();
                if lang.is_empty() {
                    format!("{}:{}", e.kind, e.pid)
                } else {
                    format!("{}:{}({})", e.kind, e.pid, lang)
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        rows.push_str(&format!(
            "<tr><td>{live}</td><td>{freq:.0} MHz</td><td><a href=\"/p/{sid}.ts\">{name}</a></td>\
             <td>{prov}</td><td>{tracks}</td></tr>",
            live = live,
            freq = ch.freq_hz as f64 / 1e6,
            sid = ch.sid,
            name = ch.label(),
            prov = ch.provider.clone().unwrap_or_default(),
            tracks = tracks
        ));
    }

    // services of the currently tuned mux that are not in the table (e.g. table stale)
    let mut extra = String::new();
    for p in &snap.programs {
        if st.table.find(p.service_id).is_none() {
            extra.push_str(&format!(
                "<tr><td>●</td><td>{:.0} MHz</td><td><a href=\"/p/{}.ts\">{}</a></td><td></td><td>live-only</td></tr>",
                current as f64 / 1e6,
                p.service_id,
                p.name.clone().unwrap_or_else(|| format!("service {}", p.service_id)),
            ));
        }
    }

    Html(format!(
        r#"<!doctype html><meta charset="utf-8"><title>dtmb</title>
<meta http-equiv="refresh" content="10">
<style>body{{font:14px/1.5 ui-monospace,monospace;margin:2rem;max-width:64rem}}
table{{border-collapse:collapse}}td,th{{border:1px solid #ccc;padding:.25rem .6rem;text-align:left}}
code{{background:#f4f4f4;padding:.1rem .3rem}}</style>
<h1>dtmb</h1>
<p>tuner: <b>{freq:.3} MHz</b> · lock <b>{lock}</b> · {str}% · SNR {snr:.1} dB ·
 {mbps:.2} Mbit/s · {packets} packets · resyncs {resyncs}</p>
<p>playlist: <a href="/playlist.m3u"><code>/playlist.m3u</code></a> ·
 whole mux: <a href="/stream.ts"><code>/stream.ts</code></a> ·
 <a href="/status.json">status.json</a> · <a href="/api/channels">api/channels</a></p>
<p><code>vlc {base}/playlist.m3u</code></p>
<h2>channels</h2>
<table><tr><th></th><th>mux</th><th>service</th><th>provider</th><th>PID/ES</th></tr>{rows}{extra}</table>"#,
        freq = current as f64 / 1e6,
        lock = snap.status.locked as u8,
        str = snap.status.strength_pct,
        snr = snap.status.snr_db,
        mbps = snap.bitrate_mbps,
        packets = snap.packets,
        resyncs = snap.resyncs,
        base = st.public_base,
    ))
}

async fn status_json(State(st): State<AppState>) -> Response {
    let snap = st.hub.snapshot();
    axum::Json(serde_json::json!({
        "freq_hz": snap.freq_hz,
        "locked": snap.status.locked,
        "strength_pct": snap.status.strength_pct,
        "snr_db": snap.status.snr_db,
        "bitrate_mbps": snap.bitrate_mbps,
        "bytes_total": snap.bytes_total,
        "packets": snap.packets,
        "resyncs": snap.resyncs,
        "epoch": st.hub.epoch(),
        "programs": snap.programs,
        "muxes": st.table.muxes.iter().map(|m| serde_json::json!({
            "freq_hz": m.freq_hz,
            "strength_pct": m.strength_pct,
            "snr_db": m.snr_db,
            "channels": m.channels.len(),
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

async fn api_channels(State(st): State<AppState>) -> Response {
    axum::Json((*st.table).clone()).into_response()
}

async fn playlist(State(st): State<AppState>) -> Response {
    let body = st.table.m3u(&st.public_base);
    let mut resp = body.into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("audio/x-mpegurl"),
    );
    resp
}

async fn full_mux(State(st): State<AppState>) -> Response {
    stream_body(st.hub.clone(), Filter::All)
}

async fn program_ts(State(st): State<AppState>, Path(path): Path<String>) -> Response {
    // /p/401 , /p/401.ts , /p/CCTV-9.ts
    let key = path.split('.').next().unwrap_or("").trim();
    let channel = match key.parse::<u16>() {
        Ok(sid) => st.table.find(sid).cloned(),
        Err(_) => st.table.find_by_label(key).cloned(),
    };
    let Some(ch) = channel else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            format!("unknown service {key}; try /api/channels"),
        )
            .into_response();
    };

    // single tuner: hop to the mux that carries this service
    if st.hub.snapshot().freq_hz != ch.freq_hz {
        let hub = st.hub.clone();
        let freq = ch.freq_hz;
        let res = tokio::task::spawn_blocking(move || hub.tune_to(freq, Duration::from_secs(6)))
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("tune task failed: {e}")));
        if let Err(e) = res {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                format!("could not tune {:.3} MHz: {e:#}", ch.freq_hz as f64 / 1e6),
            )
                .into_response();
        }
        // give PSI a moment to arrive so the filter list is accurate
        for _ in 0..20 {
            if st.hub.inner.lock().unwrap().analyzer.programs.contains_key(&ch.sid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    stream_body(st.hub.clone(), Filter::Service(ch.sid))
}

// ------------------------------------------------------------------- streaming

#[derive(Clone, Copy)]
enum Filter {
    All,
    Service(u16),
}

fn initial_pids(hub: &Hub, filter: Filter) -> Option<Vec<u16>> {
    match filter {
        Filter::All => None,
        Filter::Service(sid) => {
            let g = hub.inner.lock().unwrap();
            g.analyzer.programs.get(&sid).map(|p| p.pids())
        }
    }
}

fn stream_body(hub: Arc<Hub>, filter: Filter) -> Response {
    let epoch = hub.epoch();
    let rx = hub.subscribe();
    let psi = hub.inner.lock().unwrap().analyzer.psi_snapshot();
    let sid = match filter {
        Filter::All => None,
        Filter::Service(s) => Some(s),
    };

    // Replay PAT/PMT so a late joiner can lock immediately.
    let mut head = Vec::new();
    for (pid, section) in &psi {
        let keep = match &filter {
            Filter::All => true,
            Filter::Service(sid) => {
                *pid == ts::PID_PAT
                    || psi_pid_belongs(&hub, *pid, *sid)
                    || *pid == ts::PID_SDT
            }
        };
        if keep {
            head.extend_from_slice(&ts::psi_packets(*pid, section));
        }
    }

    let mut filt = FilterState {
        hub: hub.clone(),
        sid,
        wanted: initial_pids(&hub, filter),
        last_refresh: Instant::now(),
        epoch,
    };

    let head_stream = tokio_stream::once(Ok::<Bytes, std::io::Error>(Bytes::from(head)));
    let tail = BroadcastStream::new(rx).filter_map(move |res| {
        let out = match res {
            Ok(chunk) => {
                if filt.hub.epoch() != filt.epoch {
                    // mux changed underneath us: end the stream, let the player reconnect
                    filt.epoch = filt.hub.epoch();
                    filt.wanted = None;
                    return None;
                }
                filt.refresh();
                match &filt.wanted {
                    None => Some(chunk),
                    Some(pids) => {
                        let v = filter_chunk(&chunk, pids);
                        if v.is_empty() {
                            None
                        } else {
                            Some(Bytes::from(v))
                        }
                    }
                }
            }
            Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                eprintln!("client lagged, dropped {n} chunks");
                None
            }
        };
        out.map(Ok::<Bytes, std::io::Error>)
    });

    let mut resp = Body::from_stream(head_stream.chain(tail)).into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("video/mp2t"),
    );
    resp.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    resp
}

fn psi_pid_belongs(hub: &Hub, pid: u16, sid: u16) -> bool {
    let g = hub.inner.lock().unwrap();
    g.analyzer
        .programs
        .get(&sid)
        .map(|p| p.pmt_pid == pid)
        .unwrap_or(false)
}

/// Per-connection filter that re-reads the PMT so track/PMT changes are picked up
/// (multi-audio, subtitles, or an operator reconfiguring the mux).
struct FilterState {
    hub: Arc<Hub>,
    sid: Option<u16>,
    wanted: Option<Vec<u16>>,
    last_refresh: Instant,
    epoch: u64,
}

impl FilterState {
    fn refresh(&mut self) {
        let Some(sid) = self.sid else { return };
        if self.last_refresh.elapsed() < Duration::from_millis(800) {
            return;
        }
        self.last_refresh = Instant::now();
        let pids = {
            let g = self.hub.inner.lock().unwrap();
            g.analyzer.programs.get(&sid).map(|p| p.pids())
        };
        if pids.is_some() {
            self.wanted = pids;
        }
    }
}

fn filter_chunk(chunk: &[u8], wanted: &[u16]) -> Vec<u8> {
    let n = chunk.len() / PKT;
    let mut out = Vec::with_capacity(chunk.len() / 2);
    for i in 0..n {
        let pkt = &chunk[i * PKT..(i + 1) * PKT];
        if pkt[0] != ts::SYNC {
            continue;
        }
        let pid = (((pkt[1] & 0x1F) as u16) << 8) | pkt[2] as u16;
        if ts::keep(pid, wanted) {
            out.extend_from_slice(pkt);
        }
    }
    out
}

// ------------------------------------------------------------------------ udp

fn spawn_udp(hub: Arc<Hub>, target: String) {
    std::thread::Builder::new()
        .name("dtmb-udp".into())
        .spawn(move || {
            let mut rx = hub.subscribe();
            let sock = match std::net::UdpSocket::bind("0.0.0.0:0") {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("udp: bind failed: {e}");
                    return;
                }
            };
            if let Err(e) = sock.connect(&target) {
                eprintln!("udp: connect {target} failed: {e}");
                return;
            }
            // 7 × 188 = 1316 bytes fits a single Ethernet frame
            const CHUNK: usize = 188 * 7;
            loop {
                match rx.blocking_recv() {
                    Ok(chunk) => {
                        for part in chunk.chunks(CHUNK) {
                            if sock.send(part).is_err() {
                                std::thread::sleep(Duration::from_millis(1));
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("udp: lagged {n} chunks");
                    }
                    Err(_) => return,
                }
            }
        })
        .expect("spawn udp thread");
}
