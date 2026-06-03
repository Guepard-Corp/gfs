//! `gfs proxy` — supervise the `guepard-proxy-v2` daemon.
//!
//! Mirrors the daemon pattern used by `cmd_mcp.rs`, but **global** rather than
//! per-repo: discovery sees every clone on the machine, so one daemon serves all
//! repos. State lives next to the global config:
//!   - `~/.gfs/proxy.pid`   the running daemon's PID
//!   - `~/.gfs/proxy.log`   stdout+stderr, appended
//!   - `~/.gfs/proxy.args`  JSON `Vec<String>` of the flags last used to start it,
//!                          so `restart` rejoues them verbatim.
//!
//! Defaults to **Docker auto-discovery** (no `--backend`) with warming on; pass
//! `--backend host:port` to front a single backend instead.
//!
//! `status` is enriched: when the daemon is up it scrapes `GET /clones` and
//! prints the live clone→listener map.
//!
//! The binary is located via `GFS_PROXY_BIN`, then next to the `gfs` exe, then
//! `PATH`. A helpful error points at `cargo build -p guepard-proxy-v2` otherwise.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::ProxyStartOpts;

// ---------------------------------------------------------------------------
// State paths (global: ~/.gfs/proxy.{pid,log,args})
// ---------------------------------------------------------------------------

struct State {
    gfs_dir: PathBuf,
    pid_file: PathBuf,
    log_file: PathBuf,
    args_file: PathBuf,
}

impl State {
    fn locate() -> Result<Self> {
        let home: OsString = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .context("cannot locate user home (HOME / USERPROFILE not set)")?;
        let gfs_dir = PathBuf::from(home).join(".gfs");
        Ok(Self {
            pid_file: gfs_dir.join("proxy.pid"),
            log_file: gfs_dir.join("proxy.log"),
            args_file: gfs_dir.join("proxy.args"),
            gfs_dir,
        })
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(action: crate::ProxyAction) -> Result<()> {
    use crate::ProxyAction::*;
    let state = State::locate()?;
    match action {
        Start(opts) => start(&state, opts),
        Stop => stop(&state),
        Restart => restart(&state),
        Status => status(&state),
        Logs { follow, tail } => logs(&state, follow, tail),
        Run(opts) => run_foreground(opts),
    }
}

// ---------------------------------------------------------------------------
// Build the proxy CLI args from ProxyStartOpts
// ---------------------------------------------------------------------------

/// Translate the user's options into the exact argv passed to `guepard-proxy-v2`.
/// Discovery is implicit when no `--backend` is given; warming + cache-metrics are
/// on unless the corresponding `--no-*` flag was set.
fn build_proxy_args(opts: &ProxyStartOpts) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(backend) = &opts.backend {
        args.push("--backend".into());
        args.push(backend.clone());
    } else {
        // Explicit --discover (the proxy also auto-switches when --backend is
        // absent, but being explicit makes the spawned argv self-documenting).
        args.push("--discover".into());
        args.push("--listen-base".into());
        args.push(opts.listen_base.to_string());
    }
    args.push("--metrics".into());
    args.push(opts.metrics.clone());
    if !opts.no_warm {
        args.push("--warm".into());
    }
    if !opts.no_cache_metrics {
        args.push("--cache-metrics".into());
    }
    args.extend(opts.extra.iter().cloned());
    args
}

// ---------------------------------------------------------------------------
// start / stop / restart
// ---------------------------------------------------------------------------

fn start(state: &State, opts: ProxyStartOpts) -> Result<()> {
    if let Some(pid) = read_pid(&state.pid_file)? {
        if process_exists(pid) {
            anyhow::bail!(
                "proxy daemon already running (PID {pid}). Use `gfs proxy stop` first."
            );
        }
        fs::remove_file(&state.pid_file).ok();
    }

    fs::create_dir_all(&state.gfs_dir).context("create ~/.gfs directory")?;
    let args = build_proxy_args(&opts);
    spawn_daemon(state, &args)
}

fn restart(state: &State) -> Result<()> {
    stop(state)?;
    // Replay the flags that were used last; fall back to defaults if missing.
    let args: Vec<String> = match fs::read_to_string(&state.args_file) {
        Ok(s) => serde_json::from_str(&s).context("parse ~/.gfs/proxy.args")?,
        Err(_) => build_proxy_args(&default_opts()),
    };
    fs::create_dir_all(&state.gfs_dir).ok();
    spawn_daemon(state, &args)
}

fn default_opts() -> ProxyStartOpts {
    ProxyStartOpts {
        backend: None,
        listen_base: 55500,
        metrics: "127.0.0.1:9090".into(),
        no_warm: false,
        no_cache_metrics: false,
        extra: Vec::new(),
    }
}

/// Spawn the proxy binary detached from this process's lifetime (stdout/stderr
/// to the log file, stdin closed). Records the PID and the exact argv used.
fn spawn_daemon(state: &State, args: &[String]) -> Result<()> {
    let bin = resolve_proxy_binary()?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&state.log_file)
        .context("open ~/.gfs/proxy.log")?;

    let child = Command::new(&bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;

    let pid = child.id();
    fs::write(&state.pid_file, pid.to_string()).context("write ~/.gfs/proxy.pid")?;
    fs::write(
        &state.args_file,
        serde_json::to_string(&args).context("encode proxy.args")?,
    )
    .context("write ~/.gfs/proxy.args")?;
    drop(child);

    let mode = if args.iter().any(|a| a == "--discover") {
        "discovery"
    } else {
        "single-backend"
    };
    println!(
        "proxy daemon started (PID {pid}, {mode}). Logs: {}",
        state.log_file.display()
    );
    println!("  argv: {}", args.join(" "));
    Ok(())
}

fn stop(state: &State) -> Result<()> {
    let pid = match read_pid(&state.pid_file)? {
        Some(p) => p,
        None => {
            println!("proxy daemon is not running (no PID file)");
            return Ok(());
        }
    };
    if !process_exists(pid) {
        fs::remove_file(&state.pid_file).ok();
        println!("proxy daemon is not running (stale PID {pid})");
        return Ok(());
    }
    kill_process(pid)?;
    fs::remove_file(&state.pid_file).context("remove ~/.gfs/proxy.pid")?;
    println!("proxy daemon stopped (PID {pid})");
    Ok(())
}

// ---------------------------------------------------------------------------
// status (enriched with /clones)
// ---------------------------------------------------------------------------

fn status(state: &State) -> Result<()> {
    let running_pid = read_pid(&state.pid_file)?
        .and_then(|pid| process_exists(pid).then_some(pid));

    let saved_args: Option<Vec<String>> = fs::read_to_string(&state.args_file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let metrics_addr = saved_args
        .as_ref()
        .and_then(|a| find_arg_value(a, "--metrics"))
        .unwrap_or_else(|| "127.0.0.1:9090".to_string());

    match running_pid {
        Some(pid) => {
            println!("Daemon: running (PID {pid})");
            println!("Metrics + /clones: http://{}", metrics_addr);
            if let Some(args) = &saved_args {
                println!("argv: {}", args.join(" "));
            }
            print_clones(&metrics_addr);
        }
        None if state.pid_file.exists() => {
            println!("Daemon: stopped (stale PID file at {})", state.pid_file.display());
            println!("  → `gfs proxy stop` to clean it up, or `gfs proxy start` to restart");
        }
        None => {
            println!("Daemon: stopped");
            println!("  → `gfs proxy start` to launch (auto-discovery by default)");
        }
    }
    Ok(())
}

fn find_arg_value(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

/// Pretty-print the live clone→listener map scraped from `GET /clones`. Best-
/// effort: a fetch/parse failure prints a diagnostic and returns — `status` is
/// still useful even if the HTTP endpoint hiccups.
fn print_clones(metrics_addr: &str) {
    let url = format!("http://{metrics_addr}/clones");
    let out = match Command::new("curl")
        .args(["-fsS", "--max-time", "2", &url])
        .stdin(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => {
            println!(
                "\nFronted clones: <curl /clones failed: {}>",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return;
        }
        Err(e) => {
            println!("\nFronted clones: <could not invoke curl: {e}>");
            return;
        }
    };
    let body = match serde_json::from_slice::<serde_json::Value>(&out) {
        Ok(v) => v,
        Err(e) => {
            println!("\nFronted clones: <invalid JSON from /clones: {e}>");
            return;
        }
    };
    let arr = body.get("clones").and_then(|v| v.as_array());
    match arr {
        Some(a) if a.is_empty() => println!("\nFronted clones: (none yet — discovery scans Docker periodically)"),
        Some(a) => {
            println!("\nFronted clones ({}):", a.len());
            for c in a {
                let name = c.get("container").and_then(|v| v.as_str()).unwrap_or("?");
                let backend = c.get("backend").and_then(|v| v.as_str()).unwrap_or("?");
                let port = c.get("listen_port").and_then(|v| v.as_u64()).unwrap_or(0);
                let remote = c
                    .get("remote")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                println!(
                    "  - {name}: localhost:{port}  →  {backend}   (remote: {remote})"
                );
            }
        }
        None => println!("\nFronted clones: <unexpected /clones payload>"),
    }
}

// ---------------------------------------------------------------------------
// logs
// ---------------------------------------------------------------------------

fn logs(state: &State, follow: bool, tail: usize) -> Result<()> {
    if !state.log_file.exists() {
        println!("no log file yet at {}", state.log_file.display());
        return Ok(());
    }
    let mut cmd = Command::new("tail");
    cmd.args(["-n", &tail.to_string()]);
    if follow {
        cmd.arg("-f");
    }
    cmd.arg(&state.log_file);
    let status = cmd.status().context("invoke tail")?;
    if !status.success() && !follow {
        anyhow::bail!("tail failed (exit {:?})", status.code());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// run (foreground)
// ---------------------------------------------------------------------------

fn run_foreground(opts: ProxyStartOpts) -> Result<()> {
    let bin = resolve_proxy_binary()?;
    let args = build_proxy_args(&opts);
    eprintln!("running {} {}", bin.display(), args.join(" "));
    let status = Command::new(&bin)
        .args(&args)
        .status()
        .with_context(|| format!("spawn {}", bin.display()))?;
    if !status.success() {
        anyhow::bail!("proxy exited with {:?}", status.code());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find the proxy binary: env override, then sibling of the running `gfs`, then
/// fall back to whatever a child process would resolve from `PATH`.
fn resolve_proxy_binary() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("GFS_PROXY_BIN") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
        anyhow::bail!("GFS_PROXY_BIN set to {} but the file does not exist", p.display());
    }
    let name = if cfg!(windows) {
        "guepard-proxy-v2.exe"
    } else {
        "guepard-proxy-v2"
    };
    if let Ok(cur) = std::env::current_exe() {
        if let Some(parent) = cur.parent() {
            let sibling = parent.join(name);
            if sibling.exists() {
                return Ok(sibling);
            }
        }
    }
    // Last resort: bare name. Will only work if it's on PATH; if not, the spawn
    // error will say so clearly.
    if which_on_path(name) {
        return Ok(PathBuf::from(name));
    }
    anyhow::bail!(
        "could not find `{name}`: set GFS_PROXY_BIN, place it next to the `gfs` binary, \
         or build it (`cargo build -p guepard-proxy-v2`)"
    )
}

fn which_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|p| p.join(name).exists())
}

fn read_pid(pid_file: &Path) -> Result<Option<u32>> {
    if !pid_file.exists() {
        return Ok(None);
    }
    let s = fs::read_to_string(pid_file).context("read PID file")?;
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    Ok(Some(s.parse::<u32>().context("invalid PID in file")?))
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn process_exists(pid: u32) -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .stdin(Stdio::null())
        .output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}

#[cfg(unix)]
fn kill_process(pid: u32) -> Result<()> {
    let status = Command::new("kill")
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
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status()
        .context("taskkill command")?;
    if !status.success() {
        anyhow::bail!("taskkill failed (exit {:?})", status.code());
    }
    Ok(())
}
