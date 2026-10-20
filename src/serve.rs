//! Outputs: HTTP (MPEG-TS over chunked) + UDP push, both fed from the broadcast hub.

use anyhow::{Context as AnyhowContext, Result};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::hub::Hub;
use crate::ts::{self, PKT};

pub struct ServeConfig {
    pub listen: SocketAddr,
    pub udp: Option<String>,
}

pub fn run(hub: Arc<Hub>, cfg: ServeConfig) -> Result<()> {
    if let Some(target) = &cfg.udp {
        spawn_udp(hub.clone(), target.clone());
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let app = Router::new()
            .route("/", get(index))
            .route("/status.json", get(status_json))
            .route("/stream.ts", get(full_mux))
            .route("/p/{*path}", get(program_ts))
            .route("/healthz", get(|| async { "ok" }))
            .with_state(hub.clone());

        let listener = tokio::net::TcpListener::bind(cfg.listen)
            .await
            .with_context(|| format!("binding {}", cfg.listen))?;
        println!("HTTP:  http://{}/stream.ts   (MRL for VLC / mpv / ffplay)", cfg.listen);
        if let Some(u) = &cfg.udp {
            println!("UDP:   pushing MPEG-TS to {u}");
        }
        axum::serve(listener, app).await?;
        Ok::<(), anyhow::Error>(())
    })
}

// ------------------------------------------------------------------ handlers

async fn index(State(hub): State<Arc<Hub>>) -> Html<String> {
    let (freq, st, progs, mbps, _bytes, resyncs, packets) = hub.snapshot();
    let mut rows = String::new();
    for p in &progs {
        rows.push_str(&format!(
            "<tr><td>{sid}</td><td>{name}</td><td><a href=\"/p/{sid}.ts\">/p/{sid}.ts</a></td><td>{pmt}</td><td>{es}</td></tr>",
            sid = p.service_id,
            name = p
                .name
                .clone()
                .unwrap_or_else(|| "(no SDT)".into()),
            pmt = p.pmt_pid,
            es = p
                .streams
                .iter()
                .map(|e| format!("{}:{}", e.kind, e.pid))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    Html(format!(
        r#"<!doctype html><meta charset="utf-8"><title>dtmb</title>
<style>body{{font:14px/1.5 ui-monospace,monospace;margin:2rem;max-width:60rem}}
table{{border-collapse:collapse}}td,th{{border:1px solid #ccc;padding:.25rem .6rem;text-align:left}}
code{{background:#f4f4f4;padding:.1rem .3rem}}</style>
<h1>dtmb</h1>
<p>freq <b>{freq:.3} MHz</b> · lock <b>{lock}</b> · strength {str}% · SNR {snr:.1} dB ·
 bitrate {mbps:.2} Mbit/s · {packets} packets · resyncs {resyncs}</p>
<p>stream: <a href="/stream.ts"><code>/stream.ts</code></a> —
 <code>vlc http://{addr}/stream.ts</code></p>
<h2>programs</h2>
<table><tr><th>service</th><th>name</th><th>stream</th><th>PMT PID</th><th>ES</th></tr>{rows}</table>
<p><a href="/status.json">status.json</a></p>"#,
        freq = freq as f64 / 1e6,
        lock = st.locked as u8,
        str = st.strength_pct,
        snr = st.snr_db,
        mbps = mbps,
        packets = packets,
        resyncs = resyncs,
        addr = "127.0.0.1:8080",
        rows = rows
    ))
}

async fn status_json(State(hub): State<Arc<Hub>>) -> Response {
    let (freq, st, progs, mbps, bytes, resyncs, packets) = hub.snapshot();
    let body = serde_json::json!({
        "freq_hz": freq,
        "locked": st.locked,
        "strength_pct": st.strength_pct,
        "snr_db": st.snr_db,
        "bitrate_mbps": mbps,
        "bytes_total": bytes,
        "packets": packets,
        "resyncs": resyncs,
        "programs": progs,
    });
    axum::Json(body).into_response()
}

async fn full_mux(State(hub): State<Arc<Hub>>) -> Response {
    stream_body(hub, None)
}

async fn program_ts(State(hub): State<Arc<Hub>>, Path(path): Path<String>) -> Response {
    // /p/401 or /p/401.ts
    let Some(sid) = path
        .split('.')
        .next()
        .and_then(|s| s.parse::<u16>().ok())
    else {
        return (axum::http::StatusCode::BAD_REQUEST, "bad service id").into_response();
    };
    let wanted = {
        let g = hub.inner.lock().unwrap();
        g.analyzer.programs.get(&sid).map(|p| p.pids())
    };
    match wanted {
        Some(pids) => stream_body(hub, Some(pids)),
        None => (axum::http::StatusCode::NOT_FOUND, "unknown service id").into_response(),
    }
}

// ------------------------------------------------------------------- streaming

fn stream_body(hub: Arc<Hub>, wanted: Option<Vec<u16>>) -> Response {
    let rx = hub.subscribe();
    let psi = {
        let g = hub.inner.lock().unwrap();
        g.analyzer.psi_snapshot()
    };

    // Replay PAT/PMT so a late joiner can lock immediately.
    let mut head = Vec::new();
    for (pid, section) in &psi {
        let keep = match &wanted {
            None => true,
            Some(w) => *pid == ts::PID_PAT || w.contains(pid),
        };
        if keep {
            head.extend_from_slice(&ts::psi_packets(*pid, section));
        }
    }

    let mut filter = wanted.map(PidFilter::new);
    let head_stream = tokio_stream::once(Ok::<Bytes, std::io::Error>(Bytes::from(head)));
    let tail = BroadcastStream::new(rx).filter_map(move |res| {
        let out = match res {
            Ok(chunk) => match filter.as_mut() {
                None => Some(chunk),
                Some(f) => {
                    let v = f.apply(&chunk);
                    if v.is_empty() {
                        None
                    } else {
                        Some(Bytes::from(v))
                    }
                }
            },
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

struct PidFilter {
    wanted: Vec<u16>,
}

impl PidFilter {
    fn new(mut wanted: Vec<u16>) -> Self {
        wanted.sort_unstable();
        wanted.dedup();
        Self { wanted }
    }

    fn apply(&self, chunk: &[u8]) -> Vec<u8> {
        let n = chunk.len() / PKT;
        let mut out = Vec::with_capacity(chunk.len() / 2);
        for i in 0..n {
            let pkt = &chunk[i * PKT..(i + 1) * PKT];
            if pkt[0] != ts::SYNC {
                continue;
            }
            let pid = (((pkt[1] & 0x1F) as u16) << 8) | pkt[2] as u16;
            if ts::keep(pid, &self.wanted) {
                out.extend_from_slice(pkt);
            }
        }
        out
    }
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
            println!("udp: pushing MPEG-TS to {target}");
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
