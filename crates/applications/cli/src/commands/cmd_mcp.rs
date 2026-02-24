//! `gfs mcp` — MCP server with embedded handler (stdio, web, daemon).
//!
//! The server supports:
//! - `gfs mcp` or `gfs mcp stdio` - runs embedded MCP handler with stdio transport
//! - `gfs mcp web [--port N]` - runs embedded MCP handler with HTTP (foreground)
//! - `gfs mcp start/stop/restart/status` - HTTP daemon management
//! PID file: .gfs/mcp.pid, log file: .gfs/mcp.log.

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use axum::Router;
use gfs_mcp::GfsMcpHandler;
use rmcp::transport::{
    streamable_http_server::{session::local::LocalSessionManager, tower::StreamableHttpService},
    stdio, StreamableHttpServerConfig,
};
use rmcp::ServiceExt;

use crate::cli_utils::get_repo_dir;
use crate::McpAction;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(path: Option<PathBuf>, action: McpAction) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let gfs_dir = repo_path.join(".gfs");
    let pid_file = gfs_dir.join("mcp.pid");
    let log_file = gfs_dir.join("mcp.log");

    const DEFAULT_HTTP_PORT: u16 = 3000;

    match action {
        McpAction::Start => {
            // For daemon start, we still spawn a subprocess for background execution
            spawn_daemon_process(&repo_path, &pid_file, &log_file, DEFAULT_HTTP_PORT).await
        }
        McpAction::Stop => stop(&pid_file).await,
        McpAction::Restart => {
            stop(&pid_file).await?;
            spawn_daemon_process(&repo_path, &pid_file, &log_file, DEFAULT_HTTP_PORT).await
        }
        McpAction::Status => status(&pid_file, DEFAULT_HTTP_PORT).await,
        McpAction::Stdio => run_stdio_embedded(&repo_path).await,
        McpAction::Web { port } => run_web_embedded(port).await,
        McpAction::Version => show_version(),
    }
}

// ---------------------------------------------------------------------------
// Embedded Stdio transport
// ---------------------------------------------------------------------------

async fn run_stdio_embedded(repo_path: &std::path::Path) -> Result<()> {
    tracing::info!(
        repo_path = %repo_path.display(),
        "gfs-mcp starting (embedded stdio transport)"
    );

    let service = match GfsMcpHandler::new().serve(stdio()).await {
        Ok(s) => s,
        Err(e) => {
            let msg = e.to_string();
            tracing::error!("failed to start MCP server: {msg}");
            if msg.contains("connection closed") && msg.contains("initialized request") {
                tracing::error!(
                    "stdin was closed before the client sent the initialize request. \
                     This server must be started by your MCP client (e.g. Cursor). \
                     Use 'gfs mcp start' to run the server as a daemon instead."
                );
            }
            return Err(e.into());
        }
    };

    tracing::info!("gfs-mcp ready, waiting for requests");
    service.waiting().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Embedded HTTP transport (foreground)
// ---------------------------------------------------------------------------

async fn run_web_embedded(port: u16) -> Result<()> {
    let ct = tokio_util::sync::CancellationToken::new();
    let bind: SocketAddr = format!("127.0.0.1:{}", port).parse()?;

    let config = StreamableHttpServerConfig {
        cancellation_token: ct.child_token(),
        ..Default::default()
    };

    let mcp_service = StreamableHttpService::new(
        || Ok(GfsMcpHandler::new()),
        std::sync::Arc::new(LocalSessionManager::default()),
        config,
    );

    let app = Router::new().nest_service("/mcp", mcp_service);

    tracing::info!(
        addr = %bind,
        "gfs-mcp starting (embedded HTTP, no auth). Connect to: POST http://{}/mcp",
        bind
    );

    println!("Starting MCP server on http://127.0.0.1:{}/mcp", port);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            ct.cancel();
        })
        .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon (spawns a subprocess for background execution)
// ---------------------------------------------------------------------------

async fn spawn_daemon_process(
    repo_path: &std::path::Path,
    pid_file: &std::path::Path,
    log_file: &std::path::Path,
    port: u16,
) -> Result<()> {
    if let Some(pid) = read_pid(pid_file)? {
        if process_exists(pid) {
            anyhow::bail!(
                "MCP daemon already running (PID {}). Use 'gfs mcp stop' first.",
                pid
            );
        }
        fs::remove_file(pid_file).ok();
    }

    let gfs_dir = repo_path.join(".gfs");
    if !gfs_dir.exists() {
        anyhow::bail!(
            "not a GFS repository (no .gfs directory at {}). Run 'gfs init' first.",
            repo_path.display()
        );
    }

    // Try to find the gfs-mcp binary for spawning as daemon
    let mcp_bin = resolve_mcp_binary().ok();

    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)
        .context("open MCP log file")?;

    if let Some(mcp_bin) = mcp_bin {
        // If binary exists, spawn it as daemon
        let child = std::process::Command::new(&mcp_bin)
            .arg("--http")
            .arg(port.to_string())
            .env("GFS_REPO_PATH", repo_path.as_os_str())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()
            .context("spawn gfs-mcp")?;

        let pid = child.id();
        fs::write(pid_file, pid.to_string()).context("write PID file")?;
        drop(child);

        println!(
            "MCP daemon started (PID {}, http://127.0.0.1:{}/mcp, repo {}). No authentication required.",
            pid, port, repo_path.display()
        );
        Ok(())
    } else {
        anyhow::bail!(
            "Cannot spawn MCP daemon: gfs-mcp binary not found. \
             The embedded handler for daemon mode is not available yet. \
             Please ensure gfs-mcp binary is in PATH or next to the gfs binary."
        )
    }
}

// ---------------------------------------------------------------------------
// Stop daemon
// ---------------------------------------------------------------------------

async fn stop(pid_file: &std::path::Path) -> Result<()> {
    let pid = match read_pid(pid_file)? {
        Some(p) => p,
        None => {
            println!("MCP daemon is not running (no PID file)");
            return Ok(());
        }
    };

    if !process_exists(pid) {
        fs::remove_file(pid_file).ok();
        println!("MCP daemon is not running (stale PID {})", pid);
        return Ok(());
    }

    kill_process(pid)?;
    fs::remove_file(pid_file).context("remove PID file")?;
    println!("MCP daemon stopped (PID {})", pid);
    Ok(())
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

async fn status(pid_file: &std::path::Path, default_port: u16) -> Result<()> {
    println!("MCP Embedded Handler Status");
    println!();

    let running = pid_file.exists().then(|| read_pid(pid_file).ok().flatten()).flatten();
    let running = running.and_then(|pid| process_exists(pid).then_some(pid));

    if let Some(pid) = running {
        println!("Daemon: running (PID {}, http://127.0.0.1:{}/mcp, no auth)", pid, default_port);
    } else {
        if pid_file.exists() {
            println!("Daemon: stopped (use 'gfs mcp stop' to remove stale PID file)");
        } else {
            println!("Daemon: stopped");
        }
    }

    println!();
    println!("Embedded MCP modes:");
    println!("  - Stdio (default): gfs mcp");
    println!("  - HTTP (foreground): gfs mcp web --port 3000");
    println!("  - HTTP (daemon): gfs mcp start");
    Ok(())
}

// ---------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------

fn show_version() -> Result<()> {
    const VERSION: &str = env!("CARGO_PKG_VERSION");
    println!("gfs-mcp version: {}", VERSION);
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn resolve_mcp_binary() -> Result<PathBuf> {
    let current = std::env::current_exe().context("current executable path")?;
    let parent = current.parent().context("executable has no parent directory")?;
    let name = if cfg!(windows) { "gfs-mcp.exe" } else { "gfs-mcp" };
    let path = parent.join(name);
    if path.exists() {
        return Ok(path);
    }
    // Fallback: try without .exe on Windows (e.g. when running via cargo run)
    if cfg!(windows) {
        let path_no_ext = parent.join("gfs-mcp");
        if path_no_ext.exists() {
            return Ok(path_no_ext);
        }
    }
    anyhow::bail!(
        "gfs-mcp binary not found at {} (optional for daemon mode)",
        path.display()
    )
}

fn read_pid(pid_file: &std::path::Path) -> Result<Option<u32>> {
    if !pid_file.exists() {
        return Ok(None);
    }
    let s = fs::read_to_string(pid_file).context("read PID file")?;
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let pid: u32 = s.parse().context("invalid PID in file")?;
    Ok(Some(pid))
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let result = std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    result.map(|s| s.success()).unwrap_or(false)
}

#[cfg(not(unix))]
fn process_exists(pid: u32) -> bool {
    let filter_arg = format!("PID eq {}", pid);
    let out = std::process::Command::new("tasklist")
        .arg("/FI")
        .arg(filter_arg.as_str())
        .arg("/NH")
        .stdin(Stdio::null())
        .output();
    let out = match out {
        Ok(o) => o,
        Err(_) => return false,
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout.contains(&pid.to_string())
}

#[cfg(unix)]
fn kill_process(pid: u32) -> Result<()> {
    let status = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .context("kill command")?;
    if !status.success() {
        anyhow::bail!("kill failed (exit {:?})", status.code());
    }
    Ok(())
}

#[cfg(not(unix))]
fn kill_process(pid: u32) -> Result<()> {
    let status = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status()
        .context("taskkill command")?;
    if !status.success() {
        anyhow::bail!("taskkill failed (exit {:?})", status.code());
    }
    Ok(())
}
