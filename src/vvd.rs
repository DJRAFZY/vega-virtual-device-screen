//! VVD lifecycle: obtain a gRPC endpoint + auth token to capture from.
//!
//! Strategy:
//!   1. If a gRPC server is already listening on `port` and we can read its token
//!      from the emulator discovery file, attach to it (don't touch its lifecycle).
//!   2. Otherwise (and unless --no-autostart), (re)launch the VVD with gRPC:
//!        - make sure a vega VVD is running (`vega virtual-device start`),
//!        - read the exact emulator argv it used (from `ps`),
//!        - stop it, then relaunch the SAME instance via the `agent/emulator`
//!          launcher with `-grpc <port> -grpc-use-token` added.
//!      The launcher (not the engine binary) is used on purpose: it performs the
//!      macOS app/window + DYLD setup that keeps the renderer from crashing.
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::net::TcpStream;
use tokio::process::Command;

pub struct GrpcTarget {
    pub port: u16,
    pub token: String,
}

pub async fn ensure_grpc(port: u16, autostart: bool) -> Result<GrpcTarget> {
    if grpc_reachable(port).await {
        if let Some(token) = find_token(port) {
            tracing::info!("attaching to existing gRPC VVD on 127.0.0.1:{port}");
            return Ok(GrpcTarget { port, token });
        }
        if !autostart {
            bail!("gRPC reachable on :{port} but no token found in discovery files; cannot authenticate");
        }
        tracing::warn!("gRPC on :{port} reachable but token unknown; relaunching with -grpc-use-token");
    } else if !autostart {
        bail!("no gRPC VVD on 127.0.0.1:{port} and --no-autostart was given.\n\
               Launch one with: <agent>/emulator <args> -grpc {port} -grpc-use-token");
    }
    relaunch_with_grpc(port).await
}

async fn relaunch_with_grpc(port: u16) -> Result<GrpcTarget> {
    if !vega_running().await {
        tracing::info!("no VVD running; starting one via `vega virtual-device start`");
        vega_start().await.context("vega virtual-device start")?;
    }
    let argv = engine_argv().await.context("read running emulator argv")?;
    let engine = argv.first().cloned().ok_or_else(|| anyhow!("empty emulator argv"))?;
    let agent = agent_dir(&engine)?;
    let launcher = format!("{agent}/emulator");
    if !Path::new(&launcher).exists() {
        bail!("emulator launcher not found at {launcher}");
    }

    tracing::info!("stopping current VVD to relaunch with gRPC enabled");
    let _ = vega_stop().await;
    wait_engine_gone(Duration::from_secs(25)).await;
    cleanup_sockets();

    // Build launcher args: keep the instance args, insert `-grpc <port> -grpc-use-token`
    // before the `-qemu` passthrough section.
    let rest = &argv[1..];
    let qemu_at = rest.iter().position(|a| a == "-qemu");
    let (pre, post) = match qemu_at {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, &[][..]),
    };
    let mut args: Vec<String> = Vec::with_capacity(rest.len() + 3);
    args.extend(pre.iter().cloned());
    args.push("-grpc".into());
    args.push(port.to_string());
    args.push("-grpc-use-token".into());
    args.extend(post.iter().cloned());

    tracing::info!("relaunching: {launcher} … -grpc {port} -grpc-use-token");
    spawn_detached(&launcher, &args).context("spawn emulator launcher")?;

    wait_grpc(port, Duration::from_secs(120))
        .await
        .context("waiting for gRPC server to come up")?;
    let token = wait_token(port, Duration::from_secs(15))
        .await
        .ok_or_else(|| anyhow!("gRPC up on :{port} but no token appeared in discovery files"))?;
    Ok(GrpcTarget { port, token })
}

// ---- probing ---------------------------------------------------------------

async fn grpc_reachable(port: u16) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_millis(800), TcpStream::connect(("127.0.0.1", port))).await,
        Ok(Ok(_))
    )
}

async fn wait_grpc(port: u16, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if grpc_reachable(port).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("gRPC :{port} did not come up within {timeout:?}")
}

async fn wait_token(port: u16, timeout: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Some(t) = find_token(port) {
            return Some(t);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    None
}

// ---- emulator discovery files (grpc.port / grpc.token) ---------------------

fn discovery_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join("Library/Caches/TemporaryItems/avd/running"));
    }
    if let Some(x) = std::env::var_os("XDG_RUNTIME_DIR") {
        dirs.push(Path::new(&x).join("avd/running"));
    }
    dirs
}

/// Newest `pid_*.ini` whose `grpc.port` matches; returns its `grpc.token`.
fn find_token(port: u16) -> Option<String> {
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for dir in discovery_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("pid_") && name.ends_with(".ini")) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
            let kv = parse_ini(&text);
            if kv.get("grpc.port").and_then(|p| p.parse::<u16>().ok()) != Some(port) {
                continue;
            }
            let Some(token) = kv.get("grpc.token") else { continue };
            let mtime = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                best = Some((mtime, token.clone()));
            }
        }
    }
    best.map(|(_, t)| t)
}

fn parse_ini(text: &str) -> std::collections::HashMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect()
}

// ---- vega CLI + process helpers --------------------------------------------

async fn vega_running() -> bool {
    let out = Command::new("vega")
        .args(["virtual-device", "status"])
        .output()
        .await;
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains("\"running\":true"),
        Err(_) => false,
    }
}

async fn vega_start() -> Result<()> {
    let status = Command::new("vega")
        .args(["virtual-device", "start"])
        .status()
        .await?;
    if !status.success() {
        bail!("`vega virtual-device start` failed ({status})");
    }
    Ok(())
}

async fn vega_stop() -> Result<()> {
    Command::new("vega").args(["virtual-device", "stop"]).status().await?;
    Ok(())
}

/// Full argv of the running emulator engine, via `ps` (no /proc on macOS).
async fn engine_argv() -> Result<Vec<String>> {
    let pid = first_engine_pid().await.ok_or_else(|| anyhow!("emulator process not found"))?;
    let out = Command::new("ps")
        .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
        .output()
        .await?;
    let line = String::from_utf8_lossy(&out.stdout);
    let argv: Vec<String> = line.split_whitespace().map(String::from).collect();
    if argv.is_empty() {
        bail!("could not read argv for pid {pid}");
    }
    Ok(argv)
}

async fn first_engine_pid() -> Option<u32> {
    let out = Command::new("pgrep").args(["-f", "vega-virtual-device"]).output().await.ok()?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        .next()
}

async fn wait_engine_gone(timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if first_engine_pid().await.is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

fn cleanup_sockets() {
    if let Ok(entries) = std::fs::read_dir("/tmp") {
        for e in entries.flatten() {
            let n = e.file_name();
            let n = n.to_string_lossy();
            if n.starts_with("qmp-socket-") && n.ends_with(".sock") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn agent_dir(engine: &str) -> Result<String> {
    // engine: .../vmtools/agent/qemu/<host>/vega-virtual-device  ->  .../vmtools/agent
    let idx = engine
        .find("/qemu/")
        .ok_or_else(|| anyhow!("cannot derive agent dir from emulator path: {engine}"))?;
    Ok(engine[..idx].to_string())
}

/// Spawn the launcher fully detached (new process group), logging to a temp file,
/// so the VVD keeps running after this tool exits.
fn spawn_detached(program: &str, args: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command as StdCommand, Stdio};

    let log_path = std::env::temp_dir().join("vvd-screen-emulator.log");
    let log = std::fs::File::create(&log_path).with_context(|| format!("create {log_path:?}"))?;
    let log2 = log.try_clone()?;

    let mut cmd = StdCommand::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .process_group(0); // detach from our process group so Ctrl-C here doesn't kill it
    cmd.spawn().with_context(|| format!("spawn {program}"))?;
    tracing::info!("emulator launcher spawned; log: {}", log_path.display());
    Ok(())
}
