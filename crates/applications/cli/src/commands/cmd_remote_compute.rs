use std::path::PathBuf;

use anyhow::{Result, bail};
use serde_json::json;

use super::remote_support::{console_client_for_repo, require_remote_config};
use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, green};

pub async fn start(path: Option<PathBuf>, json_output: bool) -> Result<()> {
    run_lifecycle(path, LifecycleOp::Start, json_output).await
}

pub async fn stop(path: Option<PathBuf>, json_output: bool) -> Result<()> {
    run_lifecycle(path, LifecycleOp::Stop, json_output).await
}

pub async fn destroy(path: Option<PathBuf>, json_output: bool) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let client = console_client_for_repo(&repo_path)?;
    let (_cfg, remote) = require_remote_config(&repo_path)?;
    client.destroy_deployment(remote.deployment_id()).await?;

    if json_output {
        println!(
            "{}",
            json!({ "destroyed": true, "deployment_id": remote.deployment_id() })
        );
    } else {
        println!(
            "{} destroyed deployment {}",
            green("✓"),
            cyan(remote.deployment_id())
        );
    }
    Ok(())
}

enum LifecycleOp {
    Start,
    Stop,
}

async fn run_lifecycle(
    path: Option<PathBuf>,
    op: LifecycleOp,
    json_output: bool,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let client = console_client_for_repo(&repo_path)?;
    let (_cfg, remote) = require_remote_config(&repo_path)?;
    let deployment_id = remote.deployment_id();

    let result = match op {
        LifecycleOp::Start => client.start_deployment(deployment_id).await?,
        LifecycleOp::Stop => client.stop_deployment(deployment_id).await?,
    };

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        let label = match op {
            LifecycleOp::Start => "started",
            LifecycleOp::Stop => "stopped",
        };
        println!("{} deployment {} {}", green("✓"), cyan(deployment_id), label);
    }
    Ok(())
}

pub async fn unsupported(action: &str) -> Result<()> {
    bail!("`gfs compute {action}` is not supported for remote repos (use start/stop)")
}
