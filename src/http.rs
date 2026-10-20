//! L5 — delivery: HTTP endpoints the Android TV app talks to.
//!
//! Contract with the client:
//!   GET /playlist.m3u        → IPTV playlist (all muxes, needs no tuner)
//!   GET /p/<sid>.ts          → one service; retunes the stick when needed
//!   GET /stream.ts           → whole mux (debug / other players)
//!   GET /status.json         → machine readable state
//!   GET /                    → tiny status page for the phone

use anyhow::{Context as AnyhowContext, Result};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::catalog::{Channel, ChannelTable};
use crate::tuner::{SignalReading, Tuner};
use crate::ts::{self, PKT};

#[derive(Clone)]
struct App {
    tuner: Arc<Tuner>,
    table: Arc<ChannelTable>,
    base: String,
}

pub fn run(tuner: Arc<Tuner>, table: Arc<ChannelTable>, listen: SocketAddr, base: String) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let app_state = App {
        tuner: tuner.clone(),
        table: table.clone(),
        base: base.clone(),
    };

    rt.block_on(async move {
        let app = Router::new()
            .route("/", get(index))
            .route("/playlist.m3u", get(playlist))
            .route("/p/{*path}", get(service))
            .route("/stream.ts", get(full_mux))
            .route("/status.json", get(status))
            .route("/meter", get(meter_page))
            .route("/api/signal", get(api_signal))
            .route("/healthz", get(|| async { "ok" }))
            .with_state(app_state);

        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .with_context(|| format!("binding {listen}"))?;
        println!(
            "playlist: {}/playlist.m3u   ({} channels / {} muxes)",
            base,
            table.channels().count(),
            table.muxes.len()
        );
        axum::serve(listener, app).await?;
        Ok::<(), anyhow::Error>(())
    })
}

// ------------------------------------------------------------------ handlers

async fn index(State(app): State<App>) -> Html<String> {
    let snap = app.tuner.snapshot();
    let mut rows = String::new();
    for ch in app.table.channels() {
        let live = if ch.freq_hz == snap.freq_hz { "●" } else { "" };
        rows.push_str(&format!(
            "<tr><td>{live}</td><td>{:.0}</td><td><a href=\"/p/{}.ts\">{}</a></td><td>{}</td></tr>",
            ch.freq_hz as f64 / 1e6,
            ch.sid,
            ch.label(),
            ch.provider.clone().unwrap_or_default()
        ));
    }
    Html(format!(
        r#"<!doctype html><meta charset="utf-8"><title>dtmb</title>
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>body{{font:14px/1.5 system-ui,sans-serif;margin:1.5rem;max-width:44rem}}
table{{border-collapse:collapse;width:100%}}td,th{{border-bottom:1px solid #ddd;padding:.3rem}}
code{{background:#f2f2f2;padding:.1rem .3rem}}</style>
<h1>dtmb</h1>
<p>当前频点 <b>{freq:.3} MHz</b> · 锁定 {lock} · 强度 {str}% · SNR {snr:.1} dB · {mbps:.1} Mbit/s</p>
<p>播放列表：<code>{base}/playlist.m3u</code></p>
<table>{rows}</table>"#,
        freq = snap.freq_hz as f64 / 1e6,
        lock = snap.status.locked as u8,
        str = snap.status.strength_pct,
        snr = snap.status.snr_db,
        mbps = snap.bitrate_mbps,
        base = app.base,
    ))
}

async fn playlist(State(app): State<App>) -> Response {
    let mut resp = app.table.m3u(&app.base).into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("audio/x-mpegurl"),
    );
    resp
}

async fn status(State(app): State<App>) -> Response {
    let s = app.tuner.snapshot();
    axum::Json(serde_json::json!({
        "freq_hz": s.freq_hz,
        "locked": s.status.locked,
        "strength_pct": s.status.strength_pct,
        "snr_db": s.status.snr_db,
        "modulation": s.status.info.modulation(),
        "guard_interval": s.status.info.guard_interval(),
        "fec": s.status.info.fec(),
        "interleaving": s.status.info.interleaving(),
        "bitrate_mbps": s.bitrate_mbps,
        "packets": s.packets,
        "bytes_total": s.bytes_total,
        "resyncs": s.resyncs,
        "tuning": s.tuning,
        "programs": s.programs,
        "channels": app.table.channels().count(),
        "muxes": app.table.muxes.iter().map(|m| m.freq_hz).collect::<Vec<_>>(),
    }))
    .into_response()
}

async fn full_mux(State(app): State<App>) -> Response {
    stream(app.tuner.clone(), None)
}

async fn service(State(app): State<App>, Path(path): Path<String>) -> Response {
    // /p/401 , /p/401.ts , /p/广东卫视-HD.ts
    let key = path.split('.').next().unwrap_or("").trim();
    let channel: Option<Channel> = match key.parse::<u16>() {
        Ok(sid) => app.table.find(sid).cloned(),
        Err(_) => app.table.find_by_label(key).cloned(),
    };
    let Some(ch) = channel else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            format!("unknown service {key}"),
        )
            .into_response();
    };

    // single tuner: hop to the mux that carries this service
    if app.tuner.snapshot().freq_hz != ch.freq_hz {
        let tuner = app.tuner.clone();
        let freq = ch.freq_hz;
        let res = tokio::task::spawn_blocking(move || tuner.tune_to(freq, Duration::from_secs(8)))
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("tune task failed: {e}")));
        if let Err(e) = res {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                format!("cannot tune {:.3} MHz: {e:#}", ch.freq_hz as f64 / 1e6),
            )
                .into_response();
        }
        // let PSI arrive so the PID filter is exact (fallback: table PIDs)
        for _ in 0..15 {
            if app.tuner.snapshot().programs.iter().any(|p| p.service_id == ch.sid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    stream(app.tuner.clone(), Some(ch))
}

// ---------------------------------------------------------------------- meter

#[derive(serde::Deserialize)]
struct SignalQuery {
    /// MHz to measure. Omit to sample the frequency already being probed.
    freq: Option<f64>,
    /// `stop=1` leaves meter mode and resumes normal playback.
    stop: Option<u8>,
}

async fn api_signal(State(app): State<App>, Query(q): Query<SignalQuery>) -> Response {
    if q.stop.is_some() {
        let tuner = app.tuner.clone();
        let _ = tokio::task::spawn_blocking(move || tuner.meter_stop()).await;
        return axum::Json(serde_json::json!({ "stopped": true })).into_response();
    }
    let hz = q.freq.map(|f| (f * 1e6).round() as u32);
    let tuner = app.tuner.clone();
    match tokio::task::spawn_blocking(move || tuner.meter(hz)).await {
        Ok(Ok(r)) => axum::Json::<SignalReading>(r).into_response(),
        Ok(Err(e)) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            format!("{e:#}"),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            e.to_string(),
        )
            .into_response(),
    }
}

async fn meter_page(State(app): State<App>) -> Html<String> {
    // quick-jump buttons for the muxes we know about
    let mut muxes: Vec<u32> = app.table.muxes.iter().map(|m| m.freq_hz).collect();
    muxes.dedup();
    let first_mhz = muxes
        .first()
        .map(|f| *f as f64 / 1e6)
        .unwrap_or(698.0);
    let buttons = muxes
        .iter()
        .map(|f| {
            format!(
                "<button onclick=\"pick({mhz})\">{mhz} MHz</button>",
                mhz = *f as f64 / 1e6
            )
        })
        .collect::<String>();

    Html(format!(
        r#"<!doctype html><meta charset="utf-8"><title>天线调试 - dtmb</title>
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>
body{{font:16px/1.4 system-ui,sans-serif;margin:1rem;background:#111;color:#eee;max-width:34rem}}
h1{{font-size:1.2rem;margin:.2rem 0 1rem}}
.row{{margin:.9rem 0}}
.label{{display:flex;justify-content:space-between;font-size:.95rem;color:#9ab}}
.big{{font-size:2.2rem;font-weight:700;font-variant-numeric:tabular-nums}}
.bar{{height:26px;background:#222;border-radius:6px;overflow:hidden;margin-top:.25rem}}
.fill{{height:100%;width:0;background:#2ecc71;transition:width .25s}}
.fill.snr{{background:#4aa3ff}}
button{{font:inherit;padding:.5rem .8rem;margin:.2rem .3rem .2rem 0;border-radius:8px;
  border:1px solid #444;background:#1b1b1b;color:#eee}}
input{{font:inherit;padding:.5rem;width:6.5rem;background:#1b1b1b;color:#eee;border:1px solid #444;
  border-radius:8px}}
.lock{{display:inline-block;padding:.15rem .6rem;border-radius:999px;font-size:.85rem}}
.on{{background:#1e6b3a;color:#d7ffe6}} .off{{background:#5a1f1f;color:#ffd7d7}}
small{{color:#888}}
</style>
<h1>天线调试 · <span id="freqLabel">–</span> MHz <span id="lock" class="lock off">未锁定</span></h1>

<div class="row"><div class="label"><span>强度</span><span id="strTxt">–</span></div>
<div class="bar"><div class="fill" id="strBar"></div></div></div>

<div class="row"><div class="label"><span>信噪比 SNR</span><span id="snrTxt">–</span></div>
<div class="bar"><div class="fill snr" id="snrBar"></div></div></div>

<div class="row"><div class="label"><span>峰值（慢慢转天线，盯这个）</span><span id="peakTxt">–</span></div>
<div class="bar"><div class="fill" id="peakBar" style="background:#f5a623"></div></div></div>

<div class="row">
  <input id="freq" type="number" step="1" value="{first_mhz}"> MHz
  <button onclick="pick(+document.getElementById('freq').value)">开始</button>
  <button onclick="resetPeak()">清峰值</button>
  <button onclick="stopMeter()">恢复正常播放</button>
</div>
<div class="row">{buttons}</div>
<p><small>提示：点“开始”后转天线，看“峰值”和 SNR。SNR ≥ 15 dB 基本稳定，&lt; 10 dB 会花屏。
调试期间播放会暂停，点“恢复正常播放”即可回到电视频道。</small></p>

<script>
let freq = {first_mhz}, peak = 0, peakSnr = 0, running = false;
function pick(f) {{
  freq = f; peak = 0; peakSnr = 0; running = true;
  document.getElementById('freq').value = f;
  poll();
}}
function resetPeak() {{ peak = 0; peakSnr = 0; }}
async function stopMeter() {{
  running = false;
  await fetch('/api/signal?stop=1');
}}
async function poll() {{
  try {{
    const r = await fetch('/api/signal?freq=' + freq, {{cache:'no-store'}});
    if (!r.ok) throw new Error(await r.text());
    const d = await r.json();
    document.getElementById('freqLabel').textContent = (d.freq_hz/1e6).toFixed(3);
    const lock = document.getElementById('lock');
    lock.textContent = d.locked ? '已锁定' : '未锁定';
    lock.className = 'lock ' + (d.locked ? 'on' : 'off');
    document.getElementById('strTxt').textContent = d.strength_pct + '%';
    document.getElementById('strBar').style.width = d.strength_pct + '%';
    document.getElementById('snrTxt').textContent = d.snr_db.toFixed(1) + ' dB';
    document.getElementById('snrBar').style.width = Math.min(100, d.snr_db/30*100) + '%';
    peak = Math.max(peak, d.strength_pct);
    peakSnr = Math.max(peakSnr, d.snr_db);
    document.getElementById('peakTxt').textContent = peak + '% / ' + peakSnr.toFixed(1) + ' dB';
    document.getElementById('peakBar').style.width = peak + '%';
  }} catch (e) {{
    document.getElementById('lock').textContent = '测量失败';
    document.getElementById('lock').className = 'lock off';
  }}
  // self-scheduling: never pile up requests while a cold tune takes seconds
  if (running) setTimeout(poll, 600);
}}
// no auto-start: opening this page must not interrupt playback
document.getElementById('freq').value = {first_mhz};
</script>"#,
    ))
}

// ------------------------------------------------------------------ streaming

fn stream(tuner: Arc<Tuner>, channel: Option<Channel>) -> Response {
    let epoch = tuner.epoch();
    let rx = tuner.subscribe();
    let psi = tuner.psi_snapshot();

    // replay PAT/PMT so a late joiner locks immediately
    let mut head = Vec::new();
    for (pid, section) in &psi {
        let keep = match &channel {
            None => true,
            Some(ch) => *pid == ts::PID_PAT || *pid == ts::PID_SDT || *pid == ch.pmt_pid,
        };
        if keep {
            head.extend_from_slice(&ts::psi_packets(*pid, section));
        }
    }

    let mut state = FilterState {
        tuner: tuner.clone(),
        channel: channel.clone(),
        wanted: channel.as_ref().map(pids_from_table),
        epoch,
        last_refresh: Instant::now(),
    };

    let head_stream = tokio_stream::once(Ok::<Bytes, std::io::Error>(Bytes::from(head)));
    let tail = BroadcastStream::new(rx).filter_map(move |res| {
        let out = match res {
            Ok(chunk) => {
                if state.tuner.epoch() != state.epoch {
                    // the stick moved to another mux; end the stream so the
                    // player reconnects instead of decoding the wrong programme
                    state.epoch = state.tuner.epoch();
                    state.wanted = None;
                    return None;
                }
                state.refresh();
                match &state.wanted {
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

fn pids_from_table(ch: &Channel) -> Vec<u16> {
    let mut v = vec![ch.pmt_pid, ch.pcr_pid];
    v.extend(ch.streams.iter().map(|e| e.pid));
    v.sort_unstable();
    v.dedup();
    v
}

struct FilterState {
    tuner: Arc<Tuner>,
    channel: Option<Channel>,
    wanted: Option<Vec<u16>>,
    epoch: u64,
    last_refresh: Instant,
}

impl FilterState {
    /// Re-read the PMT so audio-track / subtitle changes are picked up live.
    fn refresh(&mut self) {
        let Some(ch) = &self.channel else { return };
        if self.last_refresh.elapsed() < Duration::from_millis(800) {
            return;
        }
        self.last_refresh = Instant::now();
        if let Some(p) = self
            .tuner
            .snapshot()
            .programs
            .iter()
            .find(|p| p.service_id == ch.sid)
        {
            self.wanted = Some(p.pids());
        }
    }
}

fn filter_chunk(chunk: &[u8], wanted: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(chunk.len() / 2);
    for pkt in chunk.chunks_exact(PKT) {
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
