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
use gfs_domain::model::config::{DEFAULT_DELETED_RETENTION_DAYS, GfsConfig};
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

#[allow(clippy::too_many_arguments)]
pub async fn run(
    path: Option<PathBuf>,
    name: Option<String>,
    start_point: Option<String>,
    delete: Option<String>,
    switch: bool,
    deleted: bool,
    restore: Option<String>,
    json_output: bool,
) -> Result<()> {
    let repo_path = path.clone().unwrap_or_else(get_repo_dir);

    if let Some(ref branch_name) = restore {
        return restore_branch(&repo_path, branch_name, json_output);
    }

    if deleted {
        return list_deleted(&repo_path, json_output);
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
            // Creating a branch must not silently discard work either.
            false,
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
    // Before the current-branch guard, which compares raw strings: `./main` and
    // `../heads/main` are not equal to `main` but resolve to it once joined onto
    // a path, so an unvalidated name walks straight past the guard.
    repo_layout::validate_branch_name(name)?;

    let current = repo_layout::get_current_branch(repo_path).unwrap_or_default();
    if name == current {
        anyhow::bail!("cannot delete the currently checked out branch '{}'", name);
    }

    let refs_dir = repo_path.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR);
    let ref_path = refs_dir.join(name);

    if !ref_path.exists() {
        anyhow::bail!("branch '{}' not found", name);
    }

    // Moved aside, not unlinked. The ref file holds the only record of which
    // commit this branch pointed at — a commit object stores its parents but not
    // any branch name — so unlinking makes the name unrecoverable even though
    // every commit survives on disk.
    let deleted = repo_layout::soft_delete_branch_ref(repo_path, name)
        .with_context(|| format!("failed to delete branch ref '{}'", name))?;

    // No background process and no collector yet, so expiry is enforced here.
    let retention_ms = retention_ms(repo_path);
    let _ = repo_layout::prune_expired_deleted_refs(repo_path, retention_ms);

    // The working copy outlives the ref unless it goes here too. It is keyed by
    // branch NAME, so a later branch reusing the name would inherit this one's
    // workspace. Reported rather than ignored: a workspace that silently failed
    // to go is exactly the state this is meant to prevent.
    let workspace = repo_layout::branch_workspace_dir(repo_path, name);
    if workspace.exists() {
        #[cfg(unix)]
        let _ = std::process::Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(&workspace)
            .output();
        std::fs::remove_dir_all(&workspace).with_context(|| {
            format!(
                "branch ref '{}' was deleted but its workspace at '{}' could not be removed; \
                 remove it before creating a branch of the same name",
                name,
                workspace.display()
            )
        })?;
    }

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
                "commit": deleted.commit_hash,
                "recoverable": true,
            }))?
        );
        return Ok(());
    }

    println!("{} Deleted branch '{}'", green("✓"), name);
    println!(
        "  {}",
        dimmed(format!("restore with: gfs branch --restore {}", name))
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Deleted branches
// ---------------------------------------------------------------------------

/// Retention window in days, from config, falling back to the built-in default.
/// A malformed or missing config is not worth failing a delete over, so this
/// falls back rather than propagating.
fn deleted_retention_days(repo_path: &std::path::Path) -> u64 {
    GfsConfig::load(repo_path)
        .ok()
        .and_then(|c| c.deleted_branch_retention_days)
        .unwrap_or(DEFAULT_DELETED_RETENTION_DAYS)
}

/// The retention window in milliseconds.
///
/// Saturating: a config of `u64::MAX / 2` days would otherwise wrap and produce
/// a window of a few hours, so the largest-looking settings would give some of
/// the shortest windows.
fn retention_ms(repo_path: &std::path::Path) -> u64 {
    deleted_retention_days(repo_path).saturating_mul(24 * 60 * 60 * 1000)
}

fn format_age(deleted_at_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let secs = now.saturating_sub(deleted_at_ms) / 1000;
    match secs {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

fn list_deleted(repo_path: &std::path::Path, json_output: bool) -> Result<()> {
    let entries = repo_layout::list_recoverable_branch_refs(repo_path, retention_ms(repo_path))
        .context("failed to read deleted branch refs")?;

    if json_output {
        // `restorable` marks the entry `--restore <name>` would pick, so a JSON
        // consumer does not have to re-derive it by max-timestamp per name.
        let mut seen: Vec<&str> = Vec::new();
        let rows: Vec<_> = entries
            .iter()
            .map(|d| {
                let restorable = !seen.contains(&d.name.as_str());
                seen.push(d.name.as_str());
                json!({
                    "branch": d.name,
                    "commit": d.commit_hash,
                    "deleted_at_ms": d.deleted_at_ms,
                    "restorable": restorable,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json!(rows))?);
        return Ok(());
    }

    if entries.is_empty() {
        println!("{}", dimmed("No deleted branches are recoverable."));
        return Ok(());
    }

    println!(
        "{}",
        dimmed(format!(
            "Recoverable for {} days after deletion:",
            deleted_retention_days(repo_path)
        ))
    );
    // Entries are newest-first, so the first occurrence of a name is the one
    // `--restore` would pick. Repeated deletions of the same name are shown
    // rather than collapsed, but only one of them is actionable.
    let mut seen: Vec<&str> = Vec::new();
    for d in &entries {
        let short: String = d.commit_hash.chars().take(7).collect();
        let newest = !seen.contains(&d.name.as_str());
        seen.push(d.name.as_str());
        println!(
            "  {}  {}  {}{}",
            cyan(&d.name),
            gold(&short),
            dimmed(format_age(d.deleted_at_ms)),
            if newest { "" } else { " (older deletion)" }
        );
    }
    println!();
    println!("{}", dimmed("restore with: gfs branch --restore <name>"));
    Ok(())
}

fn restore_branch(repo_path: &std::path::Path, name: &str, json_output: bool) -> Result<()> {
    // Checked here rather than relying on the error chain: `main` renders an
    // anyhow error with `{}`, which shows only the outermost context, so a
    // wrapped cause would be invisible to the user.
    let live = repo_path
        .join(GFS_DIR)
        .join(REFS_DIR)
        .join(HEADS_DIR)
        .join(name);
    // `is_file`, not `exists`: `refs/heads/a` is also a directory when `a/b` is
    // live. Both block the restore, but for different reasons and with
    // different advice.
    if live.is_file() {
        anyhow::bail!(
            "branch '{}' already exists, so restoring the deleted one would overwrite it. \
             Rename or delete the existing branch first",
            name
        );
    }
    if live.is_dir() {
        anyhow::bail!(
            "'{}' cannot be restored while branches nested under it exist \
             (a ref is a file, so it cannot also be a directory). \
             Delete or rename those branches first",
            name
        );
    }
    // The mirror case: restoring `a/b` needs `refs/heads/a` to be a directory,
    // but a live branch `a` is a file there. Neither check above fires — the
    // target itself does not exist — and the failure would otherwise surface as
    // a bare ENOTDIR with the cause hidden by `{}` formatting.
    let heads = repo_path.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR);
    let mut ancestor = heads.clone();
    for segment in name.split('/').filter(|s| !s.is_empty()) {
        if ancestor.is_file() {
            let blocking = ancestor
                .strip_prefix(&heads)
                .unwrap_or(&ancestor)
                .to_string_lossy()
                .to_string();
            anyhow::bail!(
                "'{}' cannot be restored while branch '{}' exists: a ref is a file, \
                 so '{}' cannot also be a directory. Delete or rename '{}' first",
                name,
                blocking,
                blocking,
                blocking
            );
        }
        ancestor = ancestor.join(segment);
    }

    let available = repo_layout::list_recoverable_branch_refs(repo_path, retention_ms(repo_path))
        .context("failed to read deleted branch refs")?;
    if !available.iter().any(|d| d.name == name) {
        if available.is_empty() {
            anyhow::bail!(
                "no deleted branch named '{}' is recoverable, and nothing else is either. \
                 A branch is recoverable for {} days after deletion",
                name,
                deleted_retention_days(repo_path)
            );
        }
        let mut names: Vec<&str> = available.iter().map(|d| d.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        anyhow::bail!(
            "no deleted branch named '{}' is recoverable. Recoverable: {}",
            name,
            names.join(", ")
        );
    }

    let restored = repo_layout::restore_deleted_branch_ref(repo_path, name)
        .with_context(|| format!("failed to restore branch '{}'", name))?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "action": "restore",
                "branch": restored.name,
                "commit": restored.commit_hash,
            }))?
        );
        return Ok(());
    }

    let short: String = restored.commit_hash.chars().take(7).collect();
    println!(
        "{} Restored branch '{}' at {}",
        green("\u{2713}"),
        restored.name,
        gold(&short)
    );
    println!(
        "  {}",
        dimmed(format!("check it out with: gfs checkout {}", restored.name))
    );
    Ok(())
}
