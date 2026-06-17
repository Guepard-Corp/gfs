use std::path::PathBuf;

use anyhow::Result;
use serde_json::json;

use super::remote_support::console_client_for_repo;
use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, dimmed, green};

pub async fn commit(path: Option<PathBuf>, message: String, json_output: bool) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let client = console_client_for_repo(&repo_path)?;
    let (_cfg, remote) = super::remote_support::require_remote_config(&repo_path)?;
    let result = client.commit(&remote, &message).await?;

    let hash = result
        .get("commit")
        .or_else(|| result.get("hash"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if json_output {
        println!(
            "{}",
            json!({ "hash": hash, "message": message, "remote": true })
        );
    } else {
        let short = if hash.len() >= 7 { &hash[..7] } else { hash };
        println!(
            "{} [remote] {}  {}",
            green("✓"),
            cyan(short),
            dimmed(&message)
        );
    }
    Ok(())
}
