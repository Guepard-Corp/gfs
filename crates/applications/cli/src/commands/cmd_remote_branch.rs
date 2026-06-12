use std::path::PathBuf;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::cmd_checkout;
use super::remote_support::{console_client_for_repo, require_remote_config};
use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, dimmed, gold};

pub async fn run(
    path: Option<PathBuf>,
    name: Option<String>,
    start_point: Option<String>,
    delete: Option<String>,
    switch: bool,
    json_output: bool,
) -> Result<()> {
    if delete.is_some() {
        bail!("branch delete is not supported for remote repos");
    }

    let repo_path = path.unwrap_or_else(get_repo_dir);

    match name {
        None => list_branches(&repo_path, json_output).await,
        Some(branch_name) => {
            let revision = start_point.unwrap_or_else(|| "HEAD".to_string());
            if switch {
                cmd_checkout::checkout(
                    Some(repo_path),
                    Some(revision),
                    Some(branch_name),
                    json_output,
                )
                .await
            } else {
                let client = console_client_for_repo(&repo_path)?;
                let (_cfg, remote) = require_remote_config(&repo_path)?;
                let result = client
                    .checkout(&remote, &revision, Some(&branch_name))
                    .await?;
                if json_output {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else {
                    println!("{} branch {}", cyan(&branch_name), dimmed("created"));
                }
                Ok(())
            }
        }
    }
}

async fn list_branches(repo_path: &std::path::Path, json_output: bool) -> Result<()> {
    let client = console_client_for_repo(repo_path)?;
    let (_cfg, remote) = require_remote_config(repo_path)?;
    let log = client.log(&remote, 1).await?;

    let branches = extract_branches(&log);
    if json_output {
        println!("{}", json!({ "branches": branches }));
        return Ok(());
    }

    for b in branches {
        let marker = if b.get("current").and_then(|v| v.as_bool()).unwrap_or(false) {
            gold("* ")
        } else {
            "  ".to_string()
        };
        let name = b
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        println!("{}{}", marker, cyan(name));
    }
    Ok(())
}

fn extract_branches(log: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let entries = log
        .as_array()
        .or_else(|| log.get("commits").and_then(|v| v.as_array()));

    if let Some(entries) = entries {
        for entry in entries {
            let commit = entry
                .get("hash")
                .or_else(|| entry.get("commit"))
                .and_then(|v| v.as_str());
            let Some(refs) = entry.get("refs").and_then(|v| v.as_array()) else {
                continue;
            };
            for r in refs {
                let Some(raw) = r.as_str() else { continue };
                let (name, current) = if let Some(branch) = raw.strip_prefix("HEAD -> ") {
                    (branch.to_string(), true)
                } else if raw == "HEAD" {
                    continue;
                } else {
                    (raw.to_string(), false)
                };
                if seen.insert(name.clone()) {
                    out.push(json!({
                        "name": name,
                        "commit": commit,
                        "current": current,
                    }));
                }
            }
        }
    }

    if out.is_empty() {
        if let Some(head) = log.get("head").and_then(|v| v.as_str()) {
            out.push(json!({ "name": "main", "commit": head, "current": true }));
        }
    }
    out
}
