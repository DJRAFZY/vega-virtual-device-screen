//! gRPC capture: subscribe to the emulator's `streamScreenshot` (push-based,
//! up to the device frame rate), JPEG-encode each frame, and publish the latest
//! JPEG to a watch channel that the HTTP server fans out as MJPEG.
//!
//! Notes learned the hard way about this Vega emulator build (34.1.15) on macOS:
//!   * gRPC must be enabled with `-grpc <port> -grpc-use-token`; every call must
//!     carry `authorization: Bearer <grpc.token>`.
//!   * `getStatus` CRASHES the emulator (it reads hardwareConfig -> GPU strings on
//!     a non-robust GL context). We never call it. `streamScreenshot`/`getVmState`
//!     are safe.
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::sync::watch;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::Request;

use crate::pb::emu::emulator_controller_client::EmulatorControllerClient;
use crate::pb::emu::{image_format::ImgFormat, ImageFormat};

#[derive(Clone)]
pub struct CaptureOpts {
    pub endpoint: String, // e.g. http://127.0.0.1:8554
    pub token: String,
    pub width: u32, // 0 = native device width
    pub display: u32,
    pub quality: u8, // JPEG quality 1..=100
}

#[derive(Default)]
pub struct Stats {
    pub frames: AtomicU64,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub last_frame_unix_ms: AtomicU64,
    pub jpeg_bytes: AtomicU64,
    pub connected: AtomicBool,
}

/// Run forever: connect, stream, encode, publish; reconnect with backoff on error.
pub async fn run(opts: CaptureOpts, tx: watch::Sender<Bytes>, stats: Arc<Stats>) {
    let mut backoff = Duration::from_millis(250);
    loop {
        match run_once(&opts, &tx, &stats).await {
            Ok(()) => {
                tracing::warn!("screenshot stream ended; reconnecting");
                backoff = Duration::from_millis(250);
            }
            Err(e) => tracing::warn!("capture error: {e:#}; retrying in {:?}", backoff),
        }
        stats.connected.store(false, Ordering::Relaxed);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(3));
    }
}

async fn run_once(opts: &CaptureOpts, tx: &watch::Sender<Bytes>, stats: &Arc<Stats>) -> Result<()> {
    let channel = Channel::from_shared(opts.endpoint.clone())?
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await
        .context("connect gRPC channel")?;

    let bearer: MetadataValue<_> = format!("Bearer {}", opts.token)
        .parse()
        .context("build auth header")?;
    let mut client =
        EmulatorControllerClient::with_interceptor(channel, move |mut req: Request<()>| {
            req.metadata_mut().insert("authorization", bearer.clone());
            Ok(req)
        })
        .max_decoding_message_size(256 * 1024 * 1024);

    let fmt = ImageFormat {
        format: ImgFormat::Rgb888 as i32,
        width: opts.width,
        height: 0,
        display: opts.display,
        ..Default::default()
    };

    let mut stream = client
        .stream_screenshot(fmt)
        .await
        .context("streamScreenshot")?
        .into_inner();
    stats.connected.store(true, Ordering::Relaxed);
    tracing::info!("gRPC connected; receiving frames");

    while let Some(img) = stream.message().await.context("receive frame")? {
        if img.image.is_empty() {
            continue; // display inactive
        }
        let (w, h) = img.format.as_ref().map(|f| (f.width, f.height)).unwrap_or((0, 0));
        if w == 0 || h == 0 {
            continue;
        }
        let needed = (w as usize) * (h as usize) * 3;
        if img.image.len() < needed {
            tracing::debug!("short frame {} < {}", img.image.len(), needed);
            continue;
        }
        let quality = opts.quality;
        let rgb = img.image;
        let jpeg =
            tokio::task::spawn_blocking(move || encode_jpeg(&rgb[..needed], w as u16, h as u16, quality))
                .await
                .context("join encoder")??;

        stats.frames.fetch_add(1, Ordering::Relaxed);
        stats.width.store(w, Ordering::Relaxed);
        stats.height.store(h, Ordering::Relaxed);
        stats.jpeg_bytes.store(jpeg.len() as u64, Ordering::Relaxed);
        stats.last_frame_unix_ms.store(now_ms(), Ordering::Relaxed);
        let _ = tx.send(Bytes::from(jpeg));
    }
    Ok(())
}

fn encode_jpeg(rgb: &[u8], w: u16, h: u16, quality: u8) -> Result<Vec<u8>> {
    use jpeg_encoder::{ColorType, Encoder};
    let mut out = Vec::with_capacity(rgb.len() / 8 + 1024);
    let enc = Encoder::new(&mut out, quality);
    enc.encode(rgb, w, h, ColorType::Rgb).context("jpeg encode")?;
    Ok(out)
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
