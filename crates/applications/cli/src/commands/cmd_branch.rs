//! `gfs branch` — list, create, and manage branches.
//!
//! - `gfs branch` — list all branches (current branch marked with *)
//! - `gfs branch <name>` — create a new branch at HEAD
//! - `gfs branch <name> <start>` — create a new branch at a specific commit/branch
//! - `gfs branch -d <name>` — delete a branch

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::model::layout::{GFS_DIR, HEADS_DIR, REFS_DIR};
use gfs_domain::ports::repository::Repository;
use gfs_domain::repo_utils::repo_layout;
use serde_json::json;

use super::cmd_checkout;
use crate::cli_utils::{get_repo_dir, list_branch_tips};
use crate::output::{cyan, dimmed, gold, green};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(
    path: Option<PathBuf>,
    name: Option<String>,
    start_point: Option<String>,
    delete: Option<String>,
    switch: bool,
    json_output: bool,
) -> Result<()> {
    let repo_path = path.clone().unwrap_or_else(get_repo_dir);

    if super::remote_support::is_remote_repo(&repo_path)? {
        return super::cmd_remote_branch::run(path, name, start_point, delete, switch, json_output)
            .await;
    }

    if let Some(ref branch_name) = delete {
        return delete_branch(&repo_path, branch_name, json_output);
    }

    match name {
        Some(branch_name) => {
            create_branch(
                &repo_path,
                &branch_name,
                start_point.as_deref(),
                switch,
                json_output,
            )
            .await
        }
        None => list_branches(&repo_path, json_output),
    }
}

// ---------------------------------------------------------------------------
// List branches
// ---------------------------------------------------------------------------

fn list_branches(repo_path: &std::path::Path, json_output: bool) -> Result<()> {
    let branches = list_branch_tips(repo_path, false)?;
    if branches.is_empty() {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "branches": [] }))?
            );
            return Ok(());
        }
        println!("  (no branches)");
        return Ok(());
    }

    let current = repo_layout::get_current_branch(repo_path).unwrap_or_default();

    // Sort branches: current first, then alphabetically.
    let mut sorted: Vec<(String, String)> = branches;
    sorted.sort_by(|(a, _), (b, _)| {
        if *a == current {
            std::cmp::Ordering::Less
        } else if *b == current {
            std::cmp::Ordering::Greater
        } else {
            a.cmp(b)
        }
    });

    if json_output {
        let out: Vec<_> = sorted
            .iter()
            .map(|(name, hash)| {
                let subject = if hash == "0" || hash.len() < 7 {
                    String::new()
                } else {
                    repo_layout::get_commit_from_hash(repo_path, hash)
                        .map(|c| c.message.lines().next().unwrap_or("").to_string())
                        .unwrap_or_default()
                };
                json!({
                    "name": name,
                    "hash": hash,
                    "subject": subject,
                    "current": *name == current,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "branches": out }))?
        );
        return Ok(());
    }

    for (name, hash) in &sorted {
        let short_hash = &hash[..7.min(hash.len())];

        // Get the commit message for this branch tip.
        let subject = if hash == "0" || hash.len() < 7 {
            String::new()
        } else {
            repo_layout::get_commit_from_hash(repo_path, hash)
                .map(|c| c.message.lines().next().unwrap_or("").to_string())
                .unwrap_or_default()
        };

        if *name == current {
            println!(
                "  {} {} {} {}",
                gold("*"),
                green(name),
                dimmed(short_hash),
                subject
            );
        } else {
            println!("    {} {} {}", cyan(name), dimmed(short_hash), subject);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Create branch (optionally switch to it)
// ---------------------------------------------------------------------------

async fn create_branch(
    repo_path: &std::path::Path,
    name: &str,
    start_point: Option<&str>,
    switch: bool,
    json_output: bool,
) -> Result<()> {
    if switch {
        return cmd_checkout::checkout(
            Some(repo_path.to_path_buf()),
            start_point.map(|s| s.to_string()),
            Some(name.to_string()),
            json_output,
        )
        .await;
    } else {
        // Just create the ref — don't switch.
        let commit_hash = if let Some(rev) = start_point {
            repo_layout::rev_parse(repo_path, rev)
                .map_err(|e| anyhow::anyhow!("failed to resolve '{}': {e}", rev))?
        } else {
            repo_layout::get_current_commit_id(repo_path)
                .map_err(|e| anyhow::anyhow!("failed to get HEAD: {e}"))?
        };

        // Check if branch already exists.
        if repo_layout::is_branch(repo_path, name) {
            anyhow::bail!("branch '{}' already exists", name);
        }

        // Write the ref file.
        let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
        repository
            .create_branch(repo_path, name, &commit_hash)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let start_label = start_point.unwrap_or("HEAD");

        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "action": "create",
                    "branch": name,
                    "hash": commit_hash,
                    "start_point": start_label,
                }))?
            );
            return Ok(());
        }

        let short_hash = &commit_hash[..7.min(commit_hash.len())];
        println!(
            "{} Created branch '{}' at {} ({})",
            green("✓"),
            cyan(name),
            start_label,
            dimmed(short_hash)
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Delete branch
// ---------------------------------------------------------------------------

fn delete_branch(repo_path: &std::path::Path, name: &str, json_output: bool) -> Result<()> {
    let current = repo_layout::get_current_branch(repo_path).unwrap_or_default();
    if name == current {
        anyhow::bail!("cannot delete the currently checked out branch '{}'", name);
    }

    let refs_dir = repo_path.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR);
    let ref_path = refs_dir.join(name);

    if !ref_path.exists() {
        anyhow::bail!("branch '{}' not found", name);
    }

    std::fs::remove_file(&ref_path)
        .with_context(|| format!("failed to delete branch ref '{}'", name))?;

    // Clean up empty parent directories (for nested branches like feature/foo).
    let mut parent = ref_path.parent();
    while let Some(dir) = parent {
        if dir == refs_dir {
            break;
        }
        if dir.read_dir().map_or(true, |mut d| d.next().is_none()) {
            let _ = std::fs::remove_dir(dir);
        }
        parent = dir.parent();
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "action": "delete",
                "branch": name,
            }))?
        );
        return Ok(());
    }

    println!("{} Deleted branch '{}'", green("✓"), name);
    Ok(())
}
