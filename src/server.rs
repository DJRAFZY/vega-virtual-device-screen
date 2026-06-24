//! Localhost HTTP server: serves a viewer page plus an MJPEG
//! (`multipart/x-mixed-replace`) stream fed from the latest captured JPEG.
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use tokio::sync::watch;

use crate::capture::Stats;

#[derive(Clone)]
pub struct AppState {
    pub rx: watch::Receiver<Bytes>,
    pub stats: Arc<Stats>,
}

const BOUNDARY: &str = "vvdframe";

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/stream", get(stream))
        .route("/frame.jpg", get(frame))
        .route("/healthz", get(healthz))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// MJPEG: one JPEG part per new frame, pushed as the watch channel changes.
async fn stream(State(st): State<AppState>) -> Response {
    let mut rx = st.rx.clone();
    let body = async_stream::stream! {
        {
            let cur = rx.borrow_and_update().clone();
            if !cur.is_empty() {
                yield Ok::<Bytes, std::io::Error>(part(&cur));
            }
        }
        loop {
            if rx.changed().await.is_err() {
                break;
            }
            let f = rx.borrow_and_update().clone();
            if f.is_empty() {
                continue;
            }
            yield Ok(part(&f));
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/x-mixed-replace; boundary={BOUNDARY}"),
        )
        .header(header::CACHE_CONTROL, "no-store, no-cache, must-revalidate")
        .header(header::PRAGMA, "no-cache")
        .header(header::CONNECTION, "close")
        .body(Body::from_stream(body))
        .unwrap()
}

fn part(jpeg: &Bytes) -> Bytes {
    let header = format!(
        "--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
        jpeg.len()
    );
    let mut v = Vec::with_capacity(header.len() + jpeg.len() + 2);
    v.extend_from_slice(header.as_bytes());
    v.extend_from_slice(jpeg);
    v.extend_from_slice(b"\r\n");
    Bytes::from(v)
}

/// Single latest frame (handy for scripting / quick checks).
async fn frame(State(st): State<AppState>) -> Response {
    let f = st.rx.borrow().clone();
    if f.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "no frame captured yet").into_response();
    }
    Response::builder()
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(f))
        .unwrap()
}

async fn healthz(State(st): State<AppState>) -> Response {
    let s = &st.stats;
    let now = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
    };
    let last = s.last_frame_unix_ms.load(Ordering::Relaxed);
    let age_ms = if last == 0 { -1i64 } else { (now.saturating_sub(last)) as i64 };
    let json = format!(
        "{{\"connected\":{},\"frames\":{},\"width\":{},\"height\":{},\"jpeg_bytes\":{},\"frame_age_ms\":{}}}",
        s.connected.load(Ordering::Relaxed),
        s.frames.load(Ordering::Relaxed),
        s.width.load(Ordering::Relaxed),
        s.height.load(Ordering::Relaxed),
        s.jpeg_bytes.load(Ordering::Relaxed),
        age_ms,
    );
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(json))
        .unwrap()
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>Vega Virtual Device — live screen</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body { margin:0; background:#0b0d10; color:#e6e9ef; font:14px/1.4 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif; }
  header { display:flex; align-items:center; gap:12px; padding:10px 16px; border-bottom:1px solid #1c2128; }
  header h1 { font-size:14px; font-weight:600; margin:0; letter-spacing:.2px; }
  .dot { width:9px; height:9px; border-radius:50%; background:#f0524d; box-shadow:0 0 0 0 rgba(240,82,77,.4); }
  .dot.live { background:#37d67a; }
  .stats { margin-left:auto; display:flex; gap:16px; color:#9aa4b2; font-variant-numeric:tabular-nums; }
  .stats b { color:#e6e9ef; font-weight:600; }
  main { display:flex; align-items:center; justify-content:center; padding:18px; }
  .frame { max-width:100%; max-height:calc(100vh - 110px); border-radius:10px; border:1px solid #1c2128; background:#000; box-shadow:0 12px 40px rgba(0,0,0,.5); }
  footer { text-align:center; color:#6b7480; padding:6px 0 16px; }
  code { background:#161b22; padding:2px 6px; border-radius:5px; color:#c9d1d9; }
</style>
</head>
<body>
  <header>
    <span class="dot" id="dot"></span>
    <h1>Vega Virtual Device — live screen</h1>
    <div class="stats">
      <span>res <b id="res">—</b></span>
      <span><b id="fps">0</b> fps</span>
      <span><b id="frames">0</b> frames</span>
      <span><b id="kb">0</b> KB/frame</span>
    </div>
  </header>
  <main>
    <img class="frame" id="img" src="/stream" alt="VVD screen stream"/>
  </main>
  <footer>streaming via emulator gRPC <code>streamScreenshot</code> → MJPEG · <code>/frame.jpg</code> for a single shot</footer>
<script>
  const $ = id => document.getElementById(id);
  let last = 0, lastFrames = 0, lastT = performance.now();
  async function poll() {
    try {
      const r = await fetch('/healthz', {cache:'no-store'});
      const s = await r.json();
      const now = performance.now();
      const dt = (now - lastT) / 1000;
      const df = s.frames - lastFrames;
      const fps = dt > 0 ? df / dt : 0;
      lastFrames = s.frames; lastT = now;
      $('res').textContent = (s.width && s.height) ? `${s.width}×${s.height}` : '—';
      $('fps').textContent = fps.toFixed(fps >= 10 ? 0 : 1);
      $('frames').textContent = s.frames;
      $('kb').textContent = (s.jpeg_bytes/1024).toFixed(0);
      const live = s.connected && s.frame_age_ms >= 0 && s.frame_age_ms < 2000;
      $('dot').classList.toggle('live', live);
    } catch (e) {}
    setTimeout(poll, 1000);
  }
  poll();
  // If the MJPEG connection ever drops, reconnect the <img>.
  $('img').addEventListener('error', () => {
    setTimeout(() => { $('img').src = '/stream?t=' + Date.now(); }, 1000);
  });
</script>
</body>
</html>
"#;
