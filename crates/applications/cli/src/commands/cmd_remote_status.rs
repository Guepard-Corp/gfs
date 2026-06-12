use std::path::PathBuf;

use anyhow::Result;
use serde_json::json;

use super::remote_support::{console_client_for_repo, require_remote_config};
use crate::cli_utils::get_repo_dir;
use crate::output::{bold, cyan, dimmed, green, red, yellow};

pub async fn run(path: Option<PathBuf>, json_output: bool) -> Result<i32> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let client = console_client_for_repo(&repo_path)?;
    let (cfg, remote) = require_remote_config(&repo_path)?;

    let status = client.deployment_status(remote.deployment_id()).await?;
    let connection = client.deployment_connection(remote.deployment_id()).await?;

    let compute_status = status
        .get("computeStatus")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "remote": true,
                "deployment_id": remote.deployment_id(),
                "database_id": remote.cp_database_id(),
                "node_id": remote.node_id,
                "provider": cfg.environment.as_ref().map(|e| &e.database_provider),
                "version": cfg.environment.as_ref().map(|e| &e.database_version),
                "compute": {
                    "compute_status": compute_status,
                    "cp_status": status.get("cpStatus"),
                    "live": status.get("live"),
                },
                "connection_info": connection.get("connection_info"),
            }))?
        );
    } else {
        let color_status = match compute_status {
            "running" => green(compute_status),
            "stopped" => yellow(compute_status),
            _ => red(compute_status),
        };
        println!("{} {}", bold("Remote deployment"), cyan(remote.deployment_id()));
        println!("  {:<18} {}", dimmed("Compute"), color_status);
        println!("  {:<18} {}", dimmed("CP database"), remote.cp_database_id());
        if let Some(info) = connection.get("connection_info") {
            if !info.is_null() {
                println!("  {:<18} {}", dimmed("Connection"), info);
            }
        }
    }

    Ok(if compute_status == "running" { 0 } else { 1 })
}
