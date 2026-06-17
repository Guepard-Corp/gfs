use std::path::PathBuf;

use anyhow::Result;
use serde_json::json;

use super::remote_support::{console_client_for_repo, require_remote_config};
use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, green};

pub async fn checkout(
    path: Option<PathBuf>,
    revision: Option<String>,
    create_branch: Option<String>,
    json_output: bool,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let client = console_client_for_repo(&repo_path)?;
    let (_cfg, remote) = require_remote_config(&repo_path)?;

    let (revision, create_branch) = match (&revision, &create_branch) {
        (Some(r), None) => (r.clone(), None),
        (None, Some(b)) => ("HEAD".to_string(), Some(b.clone())),
        (Some(r), Some(b)) => (r.clone(), Some(b.clone())),
        (None, None) => {
            anyhow::bail!("revision required or use -b <branch_name>");
        }
    };

    let result = client
        .checkout(&remote, &revision, create_branch.as_deref())
        .await?;

    if json_output {
        println!("{}", json!({ "revision": revision, "result": result }));
    } else {
        println!(
            "{} checked out {} on remote engine",
            green("✓"),
            cyan(&revision)
        );
    }
    Ok(())
}
