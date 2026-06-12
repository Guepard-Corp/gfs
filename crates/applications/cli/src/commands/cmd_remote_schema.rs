use std::path::PathBuf;

use anyhow::{Result, bail};
use super::remote_support::{console_client_for_repo, require_remote_config};
use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, green};

pub async fn run_extract(path: Option<PathBuf>, json_output: bool) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let client = console_client_for_repo(&repo_path)?;
    let (_cfg, remote) = require_remote_config(&repo_path)?;
    let result = client.schema_show(&remote).await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(())
}

pub async fn run_show(
    _commit: String,
    _path: Option<PathBuf>,
    _metadata_only: bool,
    _ddl_only: bool,
) -> Result<()> {
    bail!("`gfs schema show <commit>` is not supported for remote repos; use `gfs schema extract`")
}

pub async fn run_diff(
    commit1: String,
    commit2: String,
    path: Option<PathBuf>,
    json_output: bool,
) -> Result<i32> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let client = console_client_for_repo(&repo_path)?;
    let (_cfg, remote) = require_remote_config(&repo_path)?;
    let result = client
        .schema_diff(&remote, Some(&commit1), Some(&commit2))
        .await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&result)?);
        println!(
            "{} schema diff {}..{}",
            green("✓"),
            cyan(&commit1),
            cyan(&commit2)
        );
    }
    Ok(0)
}
