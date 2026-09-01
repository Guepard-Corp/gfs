//! `gfs freeze` -- seal a lazy clone into a point-in-time snapshot (#132).
//!
//! Re-copies every non-diverged table from ONE source instant (a single remote
//! REPEATABLE READ snapshot -- the detail #131 verified), keeps tables with
//! local writes exactly as they are (they are the user's branch), then detaches
//! the clone for good: drift checks stop, the router never federates again, and
//! the source can disappear without the clone noticing.
//!
//! Honest limitation (from the issue): freezing pays the FULL copy, so
//! time = data size / link speed. `gfs.freeze_prepare()` therefore measures the
//! real byte size on the source first (one round trip, no data moved) and this
//! command refuses above a budget -- default 1 GiB -- unless `--max-bytes`
//! raises it or `--force` bypasses it. For big sources the better answer is
//! cloning from an already-frozen endpoint (snapshot, backup, paused replica).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use serde_json::json;

use crate::cli_utils::get_repo_dir;
use crate::commands::cmd_source::{frozen_info, rows, run_sql};
use crate::output::{bold, cyan, dimmed, green, red, yellow};

/// Default copy budget for the full copy a freeze requires: 1 GiB.
pub(crate) const DEFAULT_MAX_BYTES: u64 = 1_073_741_824;

/// Bytes for humans, binary units (matches `docker`/`pg_size_pretty` habits).
fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// `gfs freeze` -- see the module docs. Also runs as the `--snapshot` step of
/// `gfs clone`, so every message must make sense in both settings.
pub async fn freeze(
    path: Option<PathBuf>,
    max_bytes: Option<u64>,
    force: bool,
    json_output: bool,
) -> Result<()> {
    let repo = path.unwrap_or_else(get_repo_dir);

    // Idempotent: freezing a frozen clone is a statement of intent already
    // satisfied, not an error.
    if let Some((true, at, _lsn)) = frozen_info(&repo).await {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({ "frozen": true, "already": true, "frozen_at": at })
                )?
            );
        } else {
            println!();
            println!(
                "  {} already frozen {}",
                green("✓"),
                dimmed(format!("(at {at})"))
            );
            println!();
        }
        return Ok(());
    }

    // Phase A (its own committed transaction): fresh drift verdicts, enum
    // labels, shape repairs, and the copy-size estimate. Probes the source, so
    // an unreachable source fails HERE, before anything is touched.
    let raw = run_sql(
        &repo,
        "SELECT already_frozen::text, n_copy::text, n_skip::text, n_conflict::text, est_bytes::text \
           FROM gfs.freeze_prepare();",
    )
    .await
    .map_err(|e| {
        anyhow::anyhow!("cannot freeze ({e}); the source must be reachable once more to take the snapshot")
    })?;
    let prep = rows(&raw);
    let r = prep
        .first()
        .context("gfs.freeze_prepare() returned nothing")?;
    let n_copy: u64 = r.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
    let n_skip: u64 = r.get(2).and_then(|v| v.parse().ok()).unwrap_or(0);
    let n_conflict: u64 = r.get(3).and_then(|v| v.parse().ok()).unwrap_or(0);
    let est_bytes: u64 = r.get(4).and_then(|v| v.parse().ok()).unwrap_or(0);

    if n_conflict > 0 {
        bail!(
            "{n_conflict} table(s) have schema conflicts with the source; \
             resolve them first (`gfs pull` shows each one)"
        );
    }
    if n_copy == 0 && n_skip == 0 {
        bail!("no tables are registered for copy-on-read; there is nothing to freeze");
    }

    // THE GUARD. Freezing copies everything; refuse a copy bigger than the
    // budget rather than silently starting a transfer that could take days
    // (a 556M-row source is real -- see #132's honest limitation).
    let cap = max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    if est_bytes > cap && !force {
        if !json_output {
            println!();
            println!(
                "  {} freezing copies everything: ~{} from the source ({} table(s))",
                yellow("!"),
                bold(fmt_bytes(est_bytes)),
                n_copy
            );
            println!(
                "    {}",
                dimmed(format!(
                    "that exceeds the {} budget, so nothing was copied",
                    fmt_bytes(cap)
                ))
            );
            println!(
                "    {}",
                dimmed(format!(
                    "raise it with `gfs freeze --max-bytes {est_bytes}` or bypass with --force"
                ))
            );
            println!(
                "    {}",
                dimmed(
                    "for a large source, prefer cloning from an already-frozen endpoint \
                     (storage snapshot, backup, paused replica): lazy AND a point in time"
                )
            );
            println!();
        }
        bail!(
            "estimated copy of {} exceeds the {} budget (nothing was copied)",
            fmt_bytes(est_bytes),
            fmt_bytes(cap)
        );
    }

    if !json_output {
        println!();
        println!(
            "  {} freezing: copying ~{} from one source instant ({} table(s))...",
            dimmed("\u{b7}"),
            fmt_bytes(est_bytes),
            n_copy
        );
    }

    // Phase B: ONE statement = ONE transaction = atomic. Everything -- the
    // resets, the copies, the bookkeeping and the frozen flag -- commits
    // together or rolls back together.
    //
    // Deliberately NOT wrapped in a local REPEATABLE READ: the point-in-time
    // guarantee is the REMOTE snapshot (postgres_fdw pins one remote RR
    // transaction per local transaction regardless of local isolation). A local
    // RR snapshot actually BROKE the freeze: it is taken before freeze_run
    // waits on the copy worker's advisory lock, the worker then commits its
    // copy_queue job deletions while we wait, and freeze_run's own DELETE hits
    // "could not serialize access due to concurrent delete". READ COMMITTED
    // (the default) sees the worker's committed state and is just as atomic.
    let raw = run_sql(&repo, "SELECT action, tbl, detail FROM gfs.freeze_run();")
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "freeze failed ({e}); the clone was left exactly as it was (the freeze is atomic)"
            )
        })?;
    let actions = rows(&raw);

    let copied = actions.iter().filter(|a| a[0] == "copied").count();
    let kept: Vec<&Vec<String>> = actions.iter().filter(|a| a[0] == "kept").collect();
    let matviews = actions.iter().filter(|a| a[0] == "matview").count();
    let seqs = actions.iter().filter(|a| a[0] == "sequence").count();
    let already = actions.iter().any(|a| a[0] == "noop");
    let frozen_line = actions.iter().find(|a| a[0] == "frozen");

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "frozen": true,
                "already": already,
                "copied": copied,
                "kept": kept.len(),
                "estimated_bytes": est_bytes,
                "actions": actions.iter().map(|a| json!({
                    "action": a[0], "table": a[1], "detail": a[2]
                })).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }

    println!();
    if already {
        println!("  {} already frozen", green("\u{2713}"));
        println!();
        return Ok(());
    }

    let lsn = frozen_line
        .and_then(|a| a[2].split("source LSN ").nth(1).map(str::to_string))
        .unwrap_or_default();
    println!(
        "  {} clone is now a snapshot: {} table(s) copied from one instant{}",
        green("\u{2713}"),
        bold(copied.to_string()),
        if lsn.is_empty() {
            String::new()
        } else {
            format!(" {}", dimmed(format!("(source LSN {lsn})")))
        }
    );
    if matviews > 0 || seqs > 0 {
        println!(
            "  {} {} matview(s) recomputed, {} sequence(s) advanced",
            green("\u{2713}"),
            matviews,
            seqs
        );
    }
    if !kept.is_empty() {
        println!();
        println!(
            "  {} {} table(s) kept your local writes",
            yellow("!"),
            bold(kept.len().to_string())
        );
        for k in &kept {
            println!("    {} {}", red("kept"), cyan(&k[1]));
        }
        println!(
            "    {}",
            dimmed("kept tables are your branch; their source-derived rows may predate the freeze")
        );
    }
    println!();
    println!(
        "  {} detached from source {}",
        green("\u{2713}"),
        dimmed("(fetch/pull are disabled; the source can go away and this clone still answers)")
    );
    println!();
    Ok(())
}
