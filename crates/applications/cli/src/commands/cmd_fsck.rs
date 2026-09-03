//! `gfs fsck` — report unreachable and dangling objects. Removes nothing.
//!
//! - `gfs fsck` — walk from every root and report what is not reachable
//! - `gfs fsck --plan` — additionally record the marked set for a later `gfs gc`

use std::path::PathBuf;

use anyhow::{Context, Result};
use gfs_domain::model::fsck::FsckReport;
use gfs_domain::model::layout::{GC_DIR, GFS_DIR};
use gfs_domain::repo_utils::fsck;
use serde_json::json;

use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, dimmed, fmt_bytes, gold, green, red, yellow};
use crate::println_safe;

pub async fn run(path: Option<PathBuf>, plan: bool, json_output: bool) -> Result<i32> {
    let repo_path = path.unwrap_or_else(get_repo_dir);

    let report = fsck::check(&repo_path)
        .context("not a GFS repository (run from a repo root or use --path <dir>)")?;

    // A plan is the prelude to a deletion, so refuse to produce one for a
    // repository that is already inconsistent — that is exactly the state in
    // which a collector turns a recoverable incident into an unopenable one.
    // `--json` still reports everything, so nothing is hidden by this.
    if plan && !report.dangling.is_empty() {
        anyhow::bail!(
            "refusing to write a collection plan: {} reference(s) point at something missing. \
             Run `gfs fsck` to see them; a collector must not run against this repository",
            report.dangling.len()
        );
    }
    if plan && !report.unrecognised.is_empty() {
        anyhow::bail!(
            "refusing to write a collection plan: {} entr(ies) in the object store could not \
             be identified. Run `gfs fsck` to see them",
            report.unrecognised.len()
        );
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
        if report.snapshots_checked_on_disk {
            format!(", {} snapshots", report.checked_snapshots)
        } else {
            String::new()
        }
    )?;

    if !report.snapshots_checked_on_disk {
        println_safe!(
            "{}",
            dimmed(
                "snapshots were not checked: this repository uses the Kubernetes runtime, \
                 where a snapshot is a VolumeSnapshot object rather than a directory"
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
                fmt_bytes(report.reclaimable_bytes)
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
        println_safe!("repository is consistent; the unreachable objects above are collectable")?;
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
