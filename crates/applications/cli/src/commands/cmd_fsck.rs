//! `gfs fsck` — report unreachable and dangling objects. Removes nothing.
//!
//! - `gfs fsck` — walk from every root and report what is not reachable
//! - `gfs fsck --plan` — additionally record the marked set for a later `gfs gc`

use std::path::PathBuf;

use anyhow::{Context, Result};
use gfs_domain::model::fsck::FsckReport;
use gfs_domain::model::layout::{GC_DIR, GFS_DIR};
use gfs_domain::repo_utils::fsck::{self, DEFAULT_GRACE, SnapshotSource};
use serde_json::json;
use std::collections::HashSet;

use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, dimmed, fmt_bytes, gold, green, red, yellow};
use crate::println_safe;

pub async fn run(
    path: Option<PathBuf>,
    plan: bool,
    grace_seconds: Option<u64>,
    json_output: bool,
) -> Result<i32> {
    let repo_path = path.unwrap_or_else(get_repo_dir);

    let grace = grace_seconds
        .map(std::time::Duration::from_secs)
        .unwrap_or(DEFAULT_GRACE);

    // On Kubernetes a snapshot is a VolumeSnapshot object, not a directory, so
    // the only way to know whether a commit's snapshot still exists is to ask
    // the cluster. Asking is done here rather than in the domain: fsck should
    // not know what Kubernetes is.
    let known: Option<HashSet<String>> = if runtime_is_kubernetes(&repo_path) {
        match gfs_storage_kubernetes::KubernetesStorage::new(None).await {
            Ok(s) => match s.list_ready_snapshot_hashes().await {
                Ok(set) => Some(set),
                Err(e) => {
                    // Reported, not fatal: the rest of the check is still
                    // worth running, and the report will say snapshots were
                    // not verified rather than implying they passed.
                    tracing::warn!("could not list VolumeSnapshots ({e}); snapshots unverified");
                    None
                }
            },
            Err(e) => {
                tracing::warn!("could not reach the cluster ({e}); snapshots unverified");
                None
            }
        }
    } else {
        None
    };

    let source = match (&known, runtime_is_kubernetes(&repo_path)) {
        (Some(set), _) => SnapshotSource::Known(set),
        (None, true) => SnapshotSource::Unavailable,
        (None, false) => SnapshotSource::Filesystem,
    };

    let report = fsck::check_with(&repo_path, grace, &source)
        .context("not a GFS repository (run from a repo root or use --path <dir>)")?;

    // A plan is the prelude to a deletion, so refuse to produce one for a
    // repository that is already inconsistent — that is exactly the state in
    // which a collector turns a recoverable incident into an unopenable one.
    // `--json` still reports everything, so nothing is hidden by this.
    // A plan drawn without seeing the snapshots would list objects as
    // collectable while knowing nothing about half the graph. Refuse: this is
    // the same failure as reporting a repository clean when a check did not run.
    if plan && !report.snapshots_checked {
        eprintln!(
            "{} refusing to write a collection plan: snapshots were not verified, so this run \
             cannot tell which of them any commit still needs. Retry when the cluster is \
             reachable",
            red("error:")
        );
        return Ok(2);
    }

    // Exits 2, not 1: the refusal is caused by corruption, and 1 already means
    // "unreachable objects found". Returning 1 here would make a script unable
    // to tell a broken repository from one that merely has garbage.
    if plan && !report.is_clean() && report.exit_code() == 2 {
        eprintln!(
            "{} refusing to write a collection plan: {} reference(s) point at something \
             missing and {} entr(ies) could not be identified. Run `gfs fsck` to see them; \
             a collector must not run against this repository",
            red("error:"),
            report.dangling.len(),
            report.unrecognised.len()
        );
        return Ok(2);
    }

    let plan_id = if plan {
        Some(write_plan(&repo_path, &report)?)
    } else {
        None
    };

    if json_output {
        render_json(&report, plan_id.as_deref())?;
    } else {
        render_text(&report, plan_id.as_deref())?;
    }

    Ok(report.exit_code())
}

/// Record the marked set so a later `gfs gc` can sweep exactly what was
/// inspected here, rather than walking again and possibly deciding differently.
///
/// Written now, before anything has been removed by anyone. lakeFS writes its
/// equivalent report in a `finally` after the sweep, which is why a killed run
/// of theirs cannot be resumed.
/// Whether this repository's snapshots live in Kubernetes rather than on disk.
fn runtime_is_kubernetes(repo_path: &std::path::Path) -> bool {
    gfs_domain::model::config::GfsConfig::load(repo_path)
        .ok()
        .and_then(|c| c.runtime)
        .map(|r| {
            let p = r.runtime_provider.trim().to_ascii_lowercase();
            p == "kubernetes" || p == "k8s"
        })
        .unwrap_or(false)
}

fn write_plan(repo_path: &std::path::Path, report: &FsckReport) -> Result<String> {
    let mark_id = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let dir = repo_path.join(GFS_DIR).join(GC_DIR).join(&mark_id);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create the plan directory at {}", dir.display()))?;

    let plan = json!({
        "mark_id": mark_id,
        // Anything created after this instant is out of scope for the sweep by
        // construction, the way Nessie's --max-file-modification defaults to
        // the mark epoch.
        "cutoff": chrono::Utc::now().to_rfc3339(),
        "report": report,
    });
    let path = dir.join("plan.json");
    std::fs::write(&path, serde_json::to_string_pretty(&plan)?)
        .with_context(|| format!("failed to write the plan at {}", path.display()))?;
    Ok(mark_id)
}

fn render_json(report: &FsckReport, plan_id: Option<&str>) -> Result<()> {
    let mut out = json!({
        "fsck": report,
        // Mirrored into the payload so a JSON consumer need not read the
        // process exit code, matching `gfs schema diff`.
        "exit_code": report.exit_code(),
    });
    if let Some(id) = plan_id {
        out["plan"] = json!({ "mark_id": id });
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// `println_safe!` yields an `io::Result`, so this propagates: a broken pipe
/// exits 0 inside the macro, and any other write error is worth surfacing
/// rather than silently dropping half a report.
fn render_text(report: &FsckReport, plan_id: Option<&str>) -> std::io::Result<()> {
    println_safe!(
        "checked {} commits, {} file lists{}",
        report.checked_commits,
        report.checked_file_lists,
        if report.snapshots_checked {
            format!(", {} snapshots", report.checked_snapshots)
        } else {
            String::new()
        }
    )?;

    if report.protected_by_grace > 0 {
        println_safe!(
            "{}",
            dimmed(format!(
                "{} recent entr(ies) held back by the {}h grace period; \
                 a commit writes its snapshot before the object that references it, \
                 so anything recent may still be in flight",
                report.protected_by_grace,
                report.grace_seconds / 3600
            ))
        )?;
    }

    if !report.snapshots_checked {
        println_safe!(
            "{}",
            dimmed(
                "snapshots were NOT verified: this repository uses the Kubernetes runtime and \
                 the cluster could not be reached, so nothing here says whether the snapshots \
                 a commit needs still exist"
            )
        )?;
    }

    if !report.unreachable.is_empty() {
        println_safe!("")?;
        println_safe!(
            "{}",
            yellow("unreachable (no branch or HEAD reaches these):")
        )?;
        for u in &report.unreachable {
            let short: String = u.hash.chars().take(7).collect();
            println_safe!(
                "  {:<9} {}  {:>10}{}",
                u.kind.as_str(),
                gold(&short),
                fmt_bytes(u.bytes),
                u.summary
                    .as_deref()
                    .map(|m| format!("  {}", dimmed(m.lines().next().unwrap_or(""))))
                    .unwrap_or_default()
            )?;
        }
        println_safe!(
            "  {}",
            dimmed(format!(
                "{} objects, {}",
                report.unreachable.len(),
                fmt_bytes(report.referenced_bytes)
            ))
        )?;
        println_safe!(
            "  {}",
            dimmed(
                "this is what a collector could remove, not space already free \u{2014} on a \
                 copy-on-write filesystem these trees share blocks with the ones they \
                 were cloned from"
            )
        )?;
    }

    if !report.reclaimable_workspaces.is_empty() {
        println_safe!("")?;
        println_safe!(
            "{}",
            yellow("stale working copies (no branch or reachable commit needs these):")
        )?;
        for w in &report.reclaimable_workspaces {
            println_safe!(
                "  {:<34} {:>10}  {}",
                w.path,
                fmt_bytes(w.bytes),
                dimmed(&w.reason)
            )?;
        }
        println_safe!(
            "  {}",
            dimmed(format!(
                "{} working copies, {} \u{2014} rebuilt from the snapshot on the next checkout, \
                 so removing them costs nothing but time",
                report.reclaimable_workspaces.len(),
                fmt_bytes(report.reclaimable_workspace_bytes)
            ))
        )?;
    }

    if !report.dangling.is_empty() {
        println_safe!("")?;
        println_safe!(
            "{}",
            red("dangling (a commit references something missing):")
        )?;
        for d in &report.dangling {
            let from: String = d.from_commit.chars().take(7).collect();
            let missing: String = d.missing.chars().take(7).collect();
            println_safe!(
                "  commit {} references {} {}, which is missing",
                cyan(&from),
                d.kind.as_str(),
                gold(&missing)
            )?;
        }
    }

    if !report.unrecognised.is_empty() {
        println_safe!("")?;
        println_safe!("{}", red("unrecognised entries in the object store:"))?;
        for u in &report.unrecognised {
            let short: String = u.hash.chars().take(7).collect();
            println_safe!("  {}  {}", gold(&short), dimmed(&u.reason))?;
        }
    }

    println_safe!("")?;
    if report.is_clean() {
        println_safe!("{} repository is consistent", green("\u{2713}"))?;
    } else if report.dangling.is_empty() && report.unrecognised.is_empty() {
        println_safe!("repository is consistent; everything listed above is collectable")?;
    } else {
        println_safe!(
            "{}",
            red("repository is inconsistent \u{2014} a collector must not be run against it")
        )?;
    }

    if let Some(id) = plan_id {
        println_safe!(
            "  {}",
            dimmed(format!(".gfs/{GC_DIR}/{id}/plan.json written"))
        )?;
    }
    Ok(())
}
