//! Write actions: run GFS operations in-process via the CLI's exported command
//! functions (`gfs_cli::commands::*`) — no subprocess, no reimplementation.

use std::path::PathBuf;

use gfs_cli::ComputeAction;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CloneRequest {
    /// Remote Postgres URL, e.g. `postgres://user:pass@host:5432/db`.
    pub from: String,
    /// Where to initialize the clone repo (default: a generated dir).
    pub path: Option<String>,
    /// Host port to bind for the local clone container.
    pub port: Option<u16>,
    pub database_version: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ComputeRequest {
    /// Path to the GFS repo whose container to act on.
    pub repo: String,
    /// One of `start` | `stop` | `restart`.
    pub action: String,
}

#[derive(Debug, Serialize)]
pub struct ActionResult {
    pub ok: bool,
    pub error: Option<String>,
}

fn clone_dir() -> PathBuf {
    let base = std::env::var("GFS_CLONES_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/tmp/gfs-clones".to_string());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    PathBuf::from(base).join(format!("clone-{stamp}"))
}

/// Instant clone via `gfs_cli::commands::cmd_clone::clone` (init + bootstrap).
pub async fn clone(req: CloneRequest) -> ActionResult {
    let path = req.path.map(PathBuf::from).unwrap_or_else(clone_dir);
    match gfs_cli::commands::cmd_clone::clone(
        req.from,
        Some(path),
        req.database_version,
        None,
        None,
        req.port,
        true,
    )
    .await
    {
        Ok(()) => ActionResult { ok: true, error: None },
        Err(e) => ActionResult { ok: false, error: Some(e.to_string()) },
    }
}

/// start/stop/restart via `gfs_cli::commands::cmd_compute::run`.
pub async fn compute(req: ComputeRequest) -> ActionResult {
    let action = match req.action.as_str() {
        "start" => ComputeAction::Start { id: None },
        "stop" => ComputeAction::Stop { id: None },
        "restart" => ComputeAction::Restart { id: None },
        other => {
            return ActionResult {
                ok: false,
                error: Some(format!("unsupported compute action: {other}")),
            };
        }
    };
    match gfs_cli::commands::cmd_compute::run(Some(PathBuf::from(req.repo)), action, true).await {
        Ok(()) => ActionResult { ok: true, error: None },
        Err(e) => ActionResult { ok: false, error: Some(e.to_string()) },
    }
}
