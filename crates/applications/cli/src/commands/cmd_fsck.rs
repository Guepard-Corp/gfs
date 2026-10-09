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
        // `refuse_plan`, not `anyhow::bail!`. A bail is reported by the global
        // handler, which exits 1 and emits an error object with no `exit_code`
        // field -- so this refusal was indistinguishable from "the repository has
        // collectable objects", and a consumer reading the JSON could not recover
        // the status it should have had.
        let message = format!(
            "a grace of {}s is below the {}s floor, so a commit still in flight could be \
             reported as garbage. Pass --disable-grace-period-check to override it \
             deliberately",
            grace.as_secs(),
            MIN_GRACE.as_secs()
        );
        return refuse_plan(json_output, &message, EXIT_COULD_NOT_RUN);
    }

    // On Kubernetes a snapshot is a VolumeSnapshot object, not a directory, so
    // the only way to know whether a commit's snapshot still exists is to ask
    // the cluster. Asking is done here rather than in the domain: fsck should
    // not know what Kubernetes is.
    // Scoped to this repository's own snapshot directory. The cluster namespace
    // is shared, so an unscoped listing hands fsck other deployments' snapshots,
    // which no local commit references and which therefore look collectable.
    // Canonicalized, because the label this is matched against was. `commit`
    // canonicalizes the repository before writing the snapshot's owner label
    // (gfs_repository.rs:493 and :577), and the adapter matches with
    // `starts_with`. A `--path` through a symlink -- or plain `--path .` --
    // produced a prefix that matched nothing, so the Known map came back empty
    // and EVERY commit read as dangling: exit 2, corruption invented out of a
    // path spelling, on the one backend that holds production repositories.
    //
    // Falls back to the uncanonicalized path rather than failing: if the
    // repository cannot be resolved the check has bigger problems, and the
    // report already distinguishes "could not verify" from "verified clean".
    let owner_prefix = repo_path
        .canonicalize()
        .unwrap_or_else(|_| repo_path.clone())
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
            // Only warn when there is a repository whose config would not read.
            // With no `.gfs` at all the config is absent for the obvious reason,
            // and this warning fired first and then contradicted itself: a
            // directory with no repository printed "could not read the repository
            // config" and, one line later, "not a GFS repository".
            // `is_dir()`, not `exists()`: a `.gfs` that is a regular file is not a
            // repository either, and gating on mere existence let the warning fire
            // beside "<path>/.gfs exists but is not a directory" -- two messages
            // for one problem, the first of them beside the point.
            // And not under `--json`. This is human prose on stderr, which no
            // `RUST_LOG` or `NO_COLOR` setting suppresses because it is not a
            // tracing event, so a caller merging the streams -- the shape most
            // scripts and agent harnesses use -- stopped receiving parseable JSON
            // on exactly this path. The report already carries the same fact in
            // `snapshots_checked: false`, which is where a machine should read it.
            if !json_output && repo_path.join(GFS_DIR).is_dir() {
                eprintln!(
                    "{} could not read the repository config, so the storage backend is \
                     unknown; snapshots will not be verified",
                    yellow("warning:")
                );
            }
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
            // `.gfs` present but not a directory is its own case, and it used to
            // fall through to the raw repository error -- which told the user no
            // repository was found "in <dir> or any parent directory" when the
            // thing was right there, as a file.
            let not_a_directory = matches!(
                std::fs::metadata(repo_path.join(GFS_DIR)),
                Ok(ref m) if !m.is_dir()
            );
            let message = if missing {
                format!(
                    "not a GFS repository: no {GFS_DIR} in {} (run from a repo root or use \
                     --path <dir>)",
                    repo_path.display()
                )
            } else if not_a_directory {
                format!(
                    "not a GFS repository: {} exists but is not a directory",
                    repo_path.join(GFS_DIR).display()
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
                        // `exit_code` belongs here too. `refuse_plan` carries it and
                        // this path did not, so the only structural difference
                        // between "your flags were wrong" and "your repository is
                        // damaged and I refused to plan" was whether the field
                        // happened to be present -- and a consumer defaulting a
                        // missing one to 0 read "not a GFS repository" as clean.
                        "error": {
                            "message": message,
                            "details": format!("{e:#}"),
                            "exit_code": EXIT_COULD_NOT_RUN,
                        }
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
        // `report.exit_code()`, not a literal 2. Unverified snapshots make a run
        // incomplete, which is 3; hardcoding 2 told a caller the repository was
        // CORRUPT when nothing had been found wrong with it, and 2 is documented
        // as "a collector must not run against this repository". On the
        // Kubernetes backend with no reachable cluster that was the default
        // answer. If there is also real corruption, `exit_code` is 2 anyway.
        return refuse_plan(
            json_output,
            "refusing to write a collection plan: snapshots were not verified, so this run \
             cannot tell which of them any commit still needs",
            report.exit_code(),
        );
    }

    // Any run that did not complete, for any reason: an unreadable object, a
    // commit that would not parse, a ref that would not resolve. A plan drawn
    // from an incomplete walk is a list of what the check could not see, which
    // is precisely the live data. This gate previously fired only on exit 2, so
    // a run that exited 3 still wrote a plan directory.
    if plan && report.exit_code() == 3 {
        return refuse_plan(
            json_output,
            "refusing to write a collection plan: the check did not complete, so what it did \
             not reach is indistinguishable from what nothing references",
            report.exit_code(),
        );
    }

    // Exits 2, not 1: the refusal is caused by corruption, and 1 already means
    // "unreachable objects found". Returning 1 here would make a script unable
    // to tell a broken repository from one that merely has garbage.
    if plan && !report.is_clean() && report.exit_code() == 2 {
        return refuse_plan(
            json_output,
            &format!(
                "refusing to write a collection plan: {} reference(s) point at something \
                 missing and {} entr(ies) could not be identified. Run `gfs fsck` to see \
                 them; a collector must not run against this repository",
                report.dangling.len(),
                report.unrecognised.len()
            ),
            report.exit_code(),
        );
    }

    // A write failure is not a verdict. `write_plan(..)?` propagated an Err, and
    // `main` turns any Err into exit 1 -- so on a clean repository whose `.gfs`
    // is read-only (an operator inspecting a root-owned repo, or a read-only
    // mount) `fsck --plan` exited 1, claiming a collector would have work to do,
    // when the check had found nothing and the command had simply failed.
    // Reported as could-not-run, which is what it is, and the check's own
    // verdict goes with it.
    let plan_id = if plan {
        match write_plan(&repo_path, &report) {
            Ok(id) => Some(id),
            Err(e) => {
                return refuse_plan(
                    json_output,
                    &format!(
                        "the check completed and returned {}, but the plan could not be \
                         written: {e:#}",
                        report.exit_code()
                    ),
                    EXIT_COULD_NOT_RUN,
                );
            }
        }
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
        gfs_domain::model::config::is_kubernetes_provider(&p)
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
        // TWO instants, because a collector needs both and they are not the same.
        //
        // `mark_started` is when the walk began: RFC 009 D5's "anything created
        // after the mark began is out of scope by construction", the way Nessie's
        // --max-file-modification defaults to the mark epoch. `cutoff` is that
        // instant minus the grace window, which is how old something must be
        // before this pass considered it at all.
        //
        // Only `cutoff` used to be here, and a collector reading it as the mark
        // epoch would be a whole grace window early -- 24 hours at the default.
        // Conservative, so not dangerous, but D5 could not be implemented from the
        // plan. `mark_id` is no substitute: it is the clock at WRITE time, after
        // the walk, so everything created during the mark is older than it and a
        // collector keying on it would exclude nothing.
        //
        // Both forced to millisecond precision. `to_rfc3339()` truncates to the
        // second, which puts up to a second of slack back into a value whose whole
        // purpose was to record a 52-90ms discrepancy.
        "mark_started": report
            .mark_started_unix_millis
            .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64))
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        "cutoff": report
            .cutoff_unix_millis
            .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64))
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
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
/// Report a refusal to write a plan, on the channel the caller asked for.
///
/// Every one of these used to be a bare `eprintln!`, so `--plan --json` left
/// stdout empty on all three paths while the comment twenty lines above
/// explained why that was wrong for the could-not-run path. A helper, so a
/// fourth refusal cannot be added that forgets again.
///
/// `exit_code` comes from the report rather than a literal: a refusal is a
/// statement about the repository, and the report already knows which one.
fn refuse_plan(json_output: bool, message: &str, exit_code: i32) -> Result<i32> {
    if json_output {
        println_safe!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "error": { "message": message, "details": message, "exit_code": exit_code }
            }))
            .unwrap_or_else(|_| "{\"error\":{\"message\":\"serialization failed\"}}".into())
        )?;
    } else {
        eprintln!("{} {message}", red("error:"));
    }
    Ok(exit_code)
}
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
    // Not a bare `println!`: on a closed reader that panics, and the process
    // then exits 101 -- outside the documented 0..=3 scheme and indistinguishable
    // from a crash. `gfs fsck --json | head -1` is ordinary usage.
    crate::println_safe!("{}", serde_json::to_string_pretty(&out)?)?;
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
        // Split by `matches_its_commit` rather than asserting none of them are. This
        // line said "not collected" unconditionally, which was true while the field
        // could only be false and became a flat contradiction of the JSON once it
        // could be true -- the per-entry reason printed just above already
        // disagreed with it.
        let safe_count = report
            .reclaimable_workspaces
            .iter()
            .filter(|w| w.matches_its_commit)
            .count();
        let total = report.reclaimable_workspaces.len();
        let summary = if safe_count == 0 {
            format!(
                "{total} working copies, {} \u{2014} listed for review, not collected: the \
                 branch each belonged to is gone, so nothing will restore them, and nothing \
                 here can tell whether one holds uncommitted work no commit records",
                fmt_bytes(report.reclaimable_workspace_bytes)
            )
        } else {
            format!(
                "{total} working copies, {} \u{2014} {safe_count} match the commit they came \
                 from in size and modification time and are collectable; the rest are listed \
                 for review, because nothing here can tell whether one holds uncommitted work \
                 no commit records",
                fmt_bytes(report.reclaimable_workspace_bytes)
            )
        };
        println_safe!("  {}", dimmed(summary))?;
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

    if !report.misaddressed.is_empty() {
        println_safe!("")?;
        println_safe!("{}", red("objects that do not hash to their own address:"))?;
        for m in &report.misaddressed {
            println_safe!(
                "  {} {}  stored here, but its content hashes to {}",
                dimmed(m.kind.as_str()),
                gold(short(&m.stored_at)),
                gold(short(&m.hashes_to))
            )?;
        }
        // Said here rather than left to the reader: the unreachable list above is
        // not independent evidence when this fires. An overwritten object lost the
        // references it carried, so whatever it used to point at stops being
        // reachable and appears in that list -- which is how a tampered object
        // turns into a recommendation to delete the history it displaced.
        if !report.unreachable.is_empty() {
            println_safe!(
                "{}",
                yellow(
                    "  anything listed as unreachable above may be unreachable BECAUSE of \
                     this - the overwritten object's references went with it"
                )
            )?;
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
    } else if report.dangling.is_empty()
        && report.unrecognised.is_empty()
        && report.misaddressed.is_empty()
    {
        // Follows `matches_its_commit` rather than asserting what it must be. The
        // comment that stood here said two contradicting lines are worse than
        // either alone, which was right -- and then the field became derivable and
        // these lines became the contradiction, claiming nothing is collected while
        // the JSON for the same run said a working copy matched its commit.
        let any_safe = report
            .reclaimable_workspaces
            .iter()
            .any(|w| w.matches_its_commit);
        if report.reclaimable_workspaces.is_empty() {
            println_safe!("repository is consistent; the objects above are collectable")?;
        } else if report.unreachable.is_empty() && !any_safe {
            println_safe!(
                "repository is consistent; the working copies above are unneeded but are \
                 listed for review rather than collected"
            )?;
        } else if any_safe {
            println_safe!(
                "repository is consistent; the objects above are collectable, and the working \
                 copies that still match the commit they came from -- the rest are \
                 listed for review"
            )?;
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
