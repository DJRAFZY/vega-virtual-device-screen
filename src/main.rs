//! vvd-screen — live screen stream for the Amazon Vega Virtual Device.
//!
//! Ensures the VVD's emulator gRPC endpoint is up (relaunching it with
//! `-grpc -grpc-use-token` if needed), subscribes to `streamScreenshot`,
//! JPEG-encodes each frame, and serves it as an MJPEG stream on localhost.
mod capture;
mod pb;
mod server;
mod vvd;

use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use clap::Parser;
use tokio::sync::watch;

#[derive(Parser, Debug)]
#[command(
    name = "vvd-screen",
    about = "Live Vega Virtual Device screen stream over localhost (emulator gRPC streamScreenshot → MJPEG)"
)]
struct Args {
    /// HTTP listen address for the viewer + stream.
    #[arg(long, default_value = "127.0.0.1:8080", env = "VVD_HTTP_ADDR")]
    http: String,

    /// Emulator gRPC port.
    #[arg(long, default_value_t = 8554, env = "VVD_GRPC_PORT")]
    grpc_port: u16,

    /// Requested capture width in px (0 = native device width). Height keeps aspect.
    #[arg(long, default_value_t = 0)]
    width: u32,

    /// Display id to capture (0 = main display).
    #[arg(long, default_value_t = 0)]
    display: u32,

    /// JPEG quality (1..=100). Lower = smaller frames, higher fps headroom.
    #[arg(long, default_value_t = 80)]
    quality: u8,

    /// Only attach to an already-running gRPC VVD; never (re)launch one.
    #[arg(long)]
    no_autostart: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "vvd_screen=info,info".into()),
        )
        .with_target(false)
        .init();

    let args = Args::parse();
    let quality = args.quality.clamp(1, 100);

    let target = vvd::ensure_grpc(args.grpc_port, !args.no_autostart).await?;
    let tok_preview = &target.token[..target.token.len().min(8)];
    tracing::info!("gRPC ready on 127.0.0.1:{} (token {}…)", target.port, tok_preview);

    let (tx, rx) = watch::channel(Bytes::new());
    let stats = Arc::new(capture::Stats::default());

    let opts = capture::CaptureOpts {
        endpoint: format!("http://127.0.0.1:{}", target.port),
        token: target.token,
        width: args.width,
        display: args.display,
        quality,
    };
    tokio::spawn(capture::run(opts, tx, stats.clone()));

    let app = server::router(server::AppState { rx, stats });
    let listener = tokio::net::TcpListener::bind(&args.http).await?;
    let addr = listener.local_addr()?;
    let url = format!("http://{addr}/");

    println!("\n  ▶  Vega Virtual Device screen stream is live:\n      {url}\n      (single frame: {url}frame.jpg)\n");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}
