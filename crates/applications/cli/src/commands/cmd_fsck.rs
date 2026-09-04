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
use std::collections::HashMap;

use crate::cli_utils::get_repo_dir;

/// The check itself could not be completed, so the report says nothing about the
/// repository. Distinct from 1 ("ran, found unreachable objects") and 2 ("ran,
/// found corruption"), both of which are statements about the repository.
const EXIT_COULD_NOT_RUN: i32 = 3;
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
    let known: Option<HashMap<String, Option<u64>>> = if runtime_is_kubernetes(&repo_path) {
        match gfs_storage_kubernetes::KubernetesStorage::new(None).await {
            Ok(s) => match s.list_ready_snapshot_hashes().await {
                Ok(set) => {
                    // Sizes are a bonus, not a requirement: if the join to ZFS
                    // fails the inventory is still correct and worth using, so
                    // each hash carries None rather than the call failing.
                    let sizes = s.list_ready_snapshot_usage().await.unwrap_or_default();
                    Some(
                        set.into_iter()
                            .map(|h| {
                                let size = sizes.get(&h).copied();
                                (h, size)
                            })
                            .collect(),
                    )
                }
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

    let report = match fsck::check_with(&repo_path, grace, &source) {
        Ok(r) => r,
        Err(e) => {
            // Only claim "not a GFS repository" after looking. The check used to
            // assert it for every failure, so a corrupt ref inside a perfectly
            // valid repository was reported as the user being in the wrong
            // directory — sending them to fix the one thing that was not wrong.
            if !repo_path.join(GFS_DIR).is_dir() {
                eprintln!(
                    "{} not a GFS repository: no {GFS_DIR} in {} (run from a repo root or use \
                     --path <dir>)",
                    red("error:"),
                    repo_path.display()
                );
            } else {
                eprintln!("{} could not complete the check: {e}", red("error:"));
            }
            // Exit 3, not 1. 1 means "the check ran and found unreachable
            // objects" — a routine, actionable outcome that a cron job may well
            // ignore. Reusing it here would let a check that never ran be read
            // as a check that found some garbage, which is the one confusion a
            // status code exists to prevent.
            return Ok(EXIT_COULD_NOT_RUN);
        }
    };

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
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let root = repo_path.join(GFS_DIR).join(GC_DIR);

    // `create_dir_all` succeeds on a directory that already exists, so two runs
    // in the same second used to land on one id and the second silently
    // overwrote the first plan — losing the record of what the first run
    // decided, which is the only reason the artefact exists. `create_dir`
    // fails on collision instead, and the suffix keeps both.
    let (mark_id, dir) = {
        let mut chosen = None;
        for n in 0..100 {
            let id = if n == 0 {
                stamp.clone()
            } else {
                format!("{stamp}-{n}")
            };
            let dir = root.join(&id);
            match std::fs::create_dir_all(&root).and_then(|()| std::fs::create_dir(&dir)) {
                Ok(()) => {
                    chosen = Some((id, dir));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(anyhow::Error::from(e)).with_context(|| {
                        format!("failed to create the plan directory at {}", dir.display())
                    });
                }
            }
        }
        chosen.ok_or_else(|| {
            anyhow::anyhow!(
                "failed to allocate a plan id under {}: 100 already exist for this second",
                root.display()
            )
        })?
    };

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

    if !report.reachability_complete {
        println_safe!(
            "{}",
            red(
                "a ref could not be resolved, so the walk did not start from every root: \
                 nothing is reported as collectable in this run, because everything behind \
                 that ref would look unreached. Repair the ref below, then run again"
            )
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
            dimmed(match report.exclusive_bytes {
                // The backend told us what would actually be freed.
                Some(exclusive) => format!(
                    "{} objects, {} would be freed",
                    report.unreachable.len(),
                    fmt_bytes(exclusive)
                ),
                None => format!(
                    "{} objects, {} referenced",
                    report.unreachable.len(),
                    fmt_bytes(report.referenced_bytes)
                ),
            })
        )?;
        println_safe!(
            "  {}",
            dimmed(if report.exclusive_bytes.is_some() {
                "measured by the storage backend as the blocks these hold exclusively, so it \
                 is what would actually be freed"
            } else {
                "an upper bound, as `du` reports it: on a copy-on-write filesystem these share \
                 blocks with what they were cloned from, so the space freed is between zero \
                 and this"
            })
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
                "{} working copies, {} \u{2014} listed, NOT safe to delete: checkout only \
                 restores a workspace that is absent, so removing one destroys live database \
                 state it would otherwise have preserved",
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
        // Deliberately does not say "everything above is collectable": the
        // working-copy section directly above says the opposite, and the two
        // lines contradicting each other is worse than either alone.
        if report.unreachable.is_empty() {
            println_safe!(
                "repository is consistent; the working copies above are unneeded but NOT safe \
                 to remove"
            )?;
        } else if report.reclaimable_workspaces.is_empty() {
            println_safe!("repository is consistent; the objects above are collectable")?;
        } else {
            println_safe!(
                "repository is consistent; the objects above are collectable, the working \
                 copies above are not"
            )?;
        }
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
