//! `gfs fsck` — report unreachable and dangling objects. Removes nothing.
//!
//! - `gfs fsck` — walk from every root and report what is not reachable
//! - `gfs fsck --plan` — additionally record the marked set for a later `gfs gc`

use std::path::PathBuf;

use anyhow::{Context, Result};
use gfs_domain::model::fsck::FsckReport;
use gfs_domain::model::fsck::SnapshotFacts;
use gfs_domain::model::layout::{GC_DIR, GFS_DIR, SNAPSHOTS_DIR};
use gfs_domain::repo_utils::fsck::{self, DEFAULT_GRACE, MIN_GRACE, SnapshotSource};
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
    disable_grace_period_check: bool,
    json_output: bool,
) -> Result<i32> {
    let repo_path = path.unwrap_or_else(get_repo_dir);

    let grace = grace_seconds
        .map(std::time::Duration::from_secs)
        .unwrap_or(DEFAULT_GRACE);

    // RFC 009 D4: a floor, and zero only behind a flag that names what it
    // disables. Rejected here, before the repository is touched, so a mistyped
    // window cannot produce a report at all -- let alone a `--plan` artefact
    // naming a snapshot a running commit is about to reference.
    if grace < MIN_GRACE && !disable_grace_period_check {
        anyhow::bail!(
            "a grace of {}s is below the {}s floor, so a commit still in flight              could be reported as garbage. Pass --disable-grace-period-check to              override it deliberately",
            grace.as_secs(),
            MIN_GRACE.as_secs()
        );
    }

    // On Kubernetes a snapshot is a VolumeSnapshot object, not a directory, so
    // the only way to know whether a commit's snapshot still exists is to ask
    // the cluster. Asking is done here rather than in the domain: fsck should
    // not know what Kubernetes is.
    // Scoped to this repository's own snapshot directory. The cluster namespace
    // is shared, so an unscoped listing hands fsck other deployments' snapshots,
    // which no local commit references and which therefore look collectable.
    let owner_prefix = repo_path
        .join(GFS_DIR)
        .join(SNAPSHOTS_DIR)
        .to_string_lossy()
        .into_owned();

    let runtime = runtime_is_kubernetes(&repo_path);

    let known: Option<HashMap<String, SnapshotFacts>> = if runtime == Some(true) {
        match gfs_storage_kubernetes::KubernetesStorage::new(None).await {
            Ok(s) => match s.list_snapshot_facts(&owner_prefix).await {
                Ok(facts) => Some(facts),
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

    let source = match (&known, runtime) {
        (Some(set), _) => SnapshotSource::Known(set),
        // Kubernetes, but the cluster could not be asked.
        (None, Some(true)) => SnapshotSource::Unavailable,
        (None, Some(false)) => SnapshotSource::Filesystem,
        // The config would not read, so which backend holds the snapshots is
        // unknown. Verifying nothing is the honest outcome; guessing either way
        // invents a finding.
        (None, None) => {
            eprintln!(
                "{} could not read the repository config, so the storage backend is unknown; \
                 snapshots will not be verified",
                yellow("warning:")
            );
            SnapshotSource::Unavailable
        }
    };

    let report = match fsck::check_with(&repo_path, grace, &source) {
        Ok(r) => r,
        Err(e) => {
            // Only claim "not a GFS repository" after looking. The check used to
            // assert it for every failure, so a corrupt ref inside a perfectly
            // valid repository was reported as the user being in the wrong
            // directory — sending them to fix the one thing that was not wrong.
            // `is_dir()` is false for a directory that exists but cannot be
            // opened, so this asks for the reason rather than accepting the
            // bare false — the same collapse this error message was written to
            // fix in the first place.
            let missing = matches!(
                std::fs::metadata(repo_path.join(GFS_DIR)),
                Err(ref e) if e.kind() == std::io::ErrorKind::NotFound
            );
            let message = if missing {
                format!(
                    "not a GFS repository: no {GFS_DIR} in {} (run from a repo root or use \
                     --path <dir>)",
                    repo_path.display()
                )
            } else {
                format!("could not complete the check: {e}")
            };
            // `--json` means every outcome is machine-readable, including this
            // one. Printing the error to stderr and nothing to stdout left a
            // caller parsing an empty string -- and every sibling command
            // (`status`, `log`, `schema show`) emits this shape on the same
            // failure, so fsck was the one that broke the contract. Same object
            // as `main.rs` builds for a returned Err; fsck cannot use that path
            // because it reports this outcome as Ok(3) rather than an Err.
            if json_output {
                println_safe!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "error": { "message": message, "details": format!("{e:#}") }
                    }))
                    .unwrap_or_else(|_| "{\"error\":{\"message\":\"serialization failed\"}}".into())
                )?;
            } else {
                eprintln!("{} {message}", red("error:"));
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

    // Any run that did not complete, for any reason: an unreadable object, a
    // commit that would not parse, a ref that would not resolve. A plan drawn
    // from an incomplete walk is a list of what the check could not see, which
    // is precisely the live data. This gate previously fired only on exit 2, so
    // a run that exited 3 still wrote a plan directory.
    if plan && report.exit_code() == 3 {
        eprintln!(
            "{} refusing to write a collection plan: the check did not complete, so what it \
             did not reach is indistinguishable from what nothing references",
            red("error:")
        );
        return Ok(3);
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
/// Whether this repository's snapshots live in Kubernetes rather than on disk,
/// or `None` when the config could not be read and the answer is unknown.
///
/// `None` rather than `false`, for the reason the domain's `Blind` type exists:
/// an unreadable config guessed as "not Kubernetes" makes the check walk a
/// snapshot directory that a Kubernetes repository never creates, find nothing,
/// and report every commit in it as dangling — corruption manufactured out of an
/// unreadable file.
fn runtime_is_kubernetes(repo_path: &std::path::Path) -> Option<bool> {
    let config = gfs_domain::model::config::GfsConfig::load(repo_path).ok()?;
    Some(config.runtime.is_some_and(|r| {
        let p = r.runtime_provider.trim().to_ascii_lowercase();
        p == "kubernetes" || p == "k8s"
    }))
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

/// Abbreviate a hash, and leave anything else alone.
///
/// These lists no longer hold only hashes: an unexpected file is reported by
/// path, a broken ref by name, a missing thing by a parenthetical description.
/// Blindly taking the first seven characters turned `.gfs/objects` into
/// `.gfs/ob` and `(snapshot directory is empty)` into `(snapsh`.
fn short(value: &str) -> String {
    if value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit()) {
        value.chars().take(7).collect()
    } else {
        value.to_string()
    }
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
                "the walk could not reach the whole graph \u{2014} a ref that would not \
                 resolve, a commit that would not parse, or an object that could not be \
                 opened. Nothing is reported as collectable in this run, because everything \
                 behind that point would look unreached whether or not it is live. Repair \
                 what is listed below, then run again"
            )
        )?;
    }

    if !report.snapshots_checked {
        println_safe!(
            "{}",
            // Says what happened, not why. The reason is printed at the point
            // it is known — an unreachable cluster and an unreadable config both
            // land here, and asserting the first for both told the reader the
            // repository uses a runtime nobody had managed to determine.
            dimmed(
                "snapshots were NOT verified: the backend holding them could not be asked, so \
                 nothing here says whether the snapshots a commit needs still exist"
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
            let short = short(&u.hash);
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
                // Both bounds, because neither is the answer on its own. The
                // referenced figure counts every shared block once per sharer,
                // so it is an upper bound; the backend's figure sums what each
                // snapshot holds *exclusively*, and blocks shared between two
                // doomed snapshots belong to neither total, so it is a lower
                // one. The truth is in between, and printing only the second
                // under-reported a measured pool by 7x.
                Some(exclusive) => format!(
                    "{} objects, between {} and {} would be freed{}",
                    report.unreachable.len(),
                    fmt_bytes(exclusive),
                    fmt_bytes(report.referenced_bytes),
                    if report.exclusive_is_partial {
                        " (some could not be measured)"
                    } else {
                        ""
                    }
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
                "the lower figure is what the storage backend says these hold exclusively, the \
                 upper is what `du` would show; blocks shared with anything surviving are \
                 freed by neither"
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
            let from = short(&d.from_commit);
            let missing = short(&d.missing);
            println_safe!(
                "  {} references {} {}",
                cyan(&from),
                d.kind.as_str(),
                gold(&missing)
            )?;
        }
    }

    if !report.unreadable.is_empty() {
        println_safe!("")?;
        println_safe!(
            "{}",
            // Not "present, but unopenable": an absent .gfs/HEAD lands here too, and
            // `init` always writes one, so its absence is a hole rather than an answer.
            // Saying "present" about a file that is gone is the class of wrong message
            // this section exists to avoid.
            yellow("the walk could not read these, so it did not start from every root:")
        )?;
        for u in &report.unreadable {
            println_safe!("  {}  {}", gold(short(&u.hash)), dimmed(&u.reason))?;
        }
        println_safe!(
            "  {}",
            dimmed(
                "not a claim about the data, which may be perfectly intact \u{2014} usually a \
                 permission the running user lacks, or a file that should exist and does \
                 not. Fix what is listed and run again"
            )
        )?;
    }

    if !report.unrecognised.is_empty() {
        println_safe!("")?;
        println_safe!("{}", red("unrecognised entries in the object store:"))?;
        for u in &report.unrecognised {
            println_safe!("  {}  {}", gold(short(&u.hash)), dimmed(&u.reason))?;
        }
    }

    println_safe!("")?;
    if report.is_clean() {
        println_safe!("{} repository is consistent", green("\u{2713}"))?;
    } else if !report.unreadable.is_empty() && report.dangling.is_empty() {
        println_safe!(
            "{}",
            yellow(
                "the check did not complete \u{2014} part of the store could not be read, so \
                 what it referenced could not be followed"
            )
        )?;
    } else if !report.snapshots_checked && report.dangling.is_empty() {
        // No tick and no "consistent": half the graph was never looked at.
        println_safe!(
            "{}",
            yellow(
                "the check did not complete \u{2014} the objects checked out fine, but the \
                 snapshots were not verified, so this says nothing about whether the \
                 repository is whole"
            )
        )?;
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
