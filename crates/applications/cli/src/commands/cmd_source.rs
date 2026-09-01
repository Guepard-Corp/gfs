//! `gfs fetch` and `gfs pull` — inspect and reconcile a lazy clone against its source.
//!
//! These are the source-sync half of the git verbs. `fetch` only looks (it never
//! modifies the clone); `pull` puts tables the source has changed back on the lazy
//! path so the next read re-copies them.
//!
//! Neither is required for correctness: the router already refuses to serve rows
//! from a table the source has changed, so a clone that is never pulled is slow,
//! not wrong. `pull` exists to make it fast again.
//!
//! Note there is deliberately no `push`: the source is never written to.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use gfs_compute_docker::DockerCompute;
use gfs_domain::model::config::GfsConfig;
use gfs_domain::ports::compute::{Compute, InstanceId};
use serde_json::json;

use crate::cli_utils::get_repo_dir;
use crate::output::{bold, cyan, dimmed, green, red, yellow};

/// Run one SQL statement inside the clone's container and return raw stdout.
///
/// Uses the container's own `psql` (via the compute runtime) rather than a local
/// client, so these commands work without postgres client tools installed. Output
/// is tuples-only and pipe-separated so it can be parsed rather than displayed.
pub(crate) async fn run_sql(repo_path: &Path, sql: &str) -> Result<String> {
    let config =
        GfsConfig::load(repo_path).context("not a GFS repository (run `gfs init` first)")?;

    let environment = config
        .environment
        .as_ref()
        .context("no database configured for this repository")?;
    if environment.database_provider != "postgres" {
        bail!(
            "source sync is only supported for postgres clones (this repository is '{}')",
            environment.database_provider
        );
    }

    let runtime = config
        .runtime
        .as_ref()
        .context("no runtime configured for this repository")?;

    let compute = DockerCompute::new().map_err(|e| anyhow::anyhow!("{e}"))?;
    let id = InstanceId(runtime.container_name.clone());

    // single-quote the statement for the shell, doubling any embedded quotes
    let escaped = sql.replace('\'', r"'\''");
    let cmd = format!("psql -U postgres -d postgres -tA -F '|' -v ON_ERROR_STOP=1 -c '{escaped}'");

    let out = compute
        .exec(&id, &cmd, None)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to reach the clone's database (is it running? try `gfs compute start`)")?;

    if out.exit_code != 0 {
        // the most common cause by far: this repo is not a lazy clone
        if out.stderr.contains("gfs.") && out.stderr.contains("does not exist") {
            bail!("this repository is not a lazy clone (no source to sync with)");
        }
        bail!("{}", out.stderr.trim());
    }
    Ok(out.stdout)
}

/// Rows of a pipe-separated psql result, with blank lines dropped.
pub(crate) fn rows(raw: &str) -> Vec<Vec<String>> {
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split('|').map(|c| c.trim().to_string()).collect())
        .collect()
}

/// #132: (frozen, frozen_at, frozen_lsn) for this clone. `None` when the
/// repository is not a lazy clone, its database is not answering, or the clone
/// predates snapshot mode (no `gfs.clone_mode` table) -- in every one of those
/// cases the caller must behave exactly as before #132.
pub(crate) async fn frozen_info(repo_path: &Path) -> Option<(bool, String, String)> {
    let raw = run_sql(
        repo_path,
        "SELECT frozen::text, \
                COALESCE(to_char(frozen_at, 'YYYY-MM-DD HH24:MI:SS'), ''), \
                COALESCE(frozen_lsn, '') \
           FROM gfs.clone_mode LIMIT 1",
    )
    .await
    .ok()?;
    let r = rows(&raw).into_iter().next()?;
    if r.is_empty() {
        return None;
    }
    Some((
        r[0] == "true" || r[0] == "t",
        r.get(1).cloned().unwrap_or_default(),
        r.get(2).cloned().unwrap_or_default(),
    ))
}

/// How many registered tables carry local writes (the freeze's "kept" set).
/// Used by the detached messages so they can say your branch tables are older.
pub(crate) async fn kept_count(repo_path: &Path) -> u64 {
    match run_sql(
        repo_path,
        "SELECT count(*) FROM gfs.clone_source cs WHERE gfs.relation_diverged_sql(cs.relid)",
    )
    .await
    {
        Ok(out) => out.trim().parse().unwrap_or(0),
        Err(_) => 0,
    }
}

/// #131: the clone's copy-coherence verdict -- does it MIX data copied at
/// different source moments? Computed entirely from gfs.copy_watermark, so it
/// costs no source contact and works offline (and from `gfs status`, which
/// never probes).
///
/// `None` when the database is not answering or the extension predates
/// watermarking (no `gfs.clone_moments`); callers then say nothing, exactly as
/// before #131.
pub(crate) struct CloneMoments {
    pub state: String,    // 'frozen' | 'lazy'
    pub span_min: String, // WAL span the copies cover (empty until copies exist)
    pub span_max: String,
    pub copied: u64,       // tables holding copied content
    pub moment_count: u64, // distinct source moments observed (a lower bound)
    pub torn: bool,
    pub unmarked: u64,       // copied tables with no watermark: moment unknown
    pub diverged_stale: u64, // frozen only: kept tables whose source rows predate the freeze
}

pub(crate) async fn clone_moments(repo_path: &Path) -> Option<CloneMoments> {
    let raw = run_sql(
        repo_path,
        "SELECT state, COALESCE(span_min::text,''), COALESCE(span_max::text,''), \
                copied_tables, moment_count, torn::text, unmarked_tables, diverged_stale \
           FROM gfs.clone_moments()",
    )
    .await
    .ok()?;
    let r = rows(&raw).into_iter().next()?;
    if r.len() < 8 {
        return None;
    }
    Some(CloneMoments {
        state: r[0].clone(),
        span_min: r[1].clone(),
        span_max: r[2].clone(),
        copied: r[3].parse().unwrap_or(0),
        moment_count: r[4].parse().unwrap_or(0),
        torn: r[5] == "t" || r[5] == "true",
        unmarked: r[6].parse().unwrap_or(0),
        diverged_stale: r[7].parse().unwrap_or(0),
    })
}

/// `gfs fetch` — report what has changed on the source. Never modifies the clone.
///
/// Reads the cached verdict by default so it is fast and works while the source is
/// unreachable; `--check` forces a fresh probe.
pub async fn fetch(path: Option<PathBuf>, check: bool, json_output: bool) -> Result<()> {
    let repo = path.unwrap_or_else(get_repo_dir);

    // #132: a frozen clone is detached. Nothing to fetch, and no probe may run,
    // `--check` included: the user sealed the source away.
    if let Some((true, at, lsn)) = frozen_info(&repo).await {
        let kept = kept_count(&repo).await;
        // #131: frozen precedence -- a frozen clone is single-moment BY
        // CONSTRUCTION, so the torn/spans line never renders here. The only
        // #131 fact worth surfacing is diverged_stale: kept tables whose
        // source rows demonstrably predate the freeze.
        let stale = clone_moments(&repo)
            .await
            .map(|m| m.diverged_stale)
            .unwrap_or(0);
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "detached": true, "state": "frozen", "torn": false,
                    "frozen_at": at, "frozen_lsn": lsn, "kept": kept,
                    "diverged_stale": stale
                }))?
            );
            return Ok(());
        }
        println!();
        println!(
            "  {} detached snapshot {} — no source to sync",
            green("\u{2713}"),
            dimmed(format!("(frozen {at}, source LSN {lsn})"))
        );
        if stale > 0 {
            println!(
                "    {}",
                dimmed(format!(
                    "{stale} kept table(s) hold source rows that predate the freeze (they are your branch)"
                ))
            );
        } else if kept > 0 {
            println!(
                "    {}",
                dimmed(format!(
                    "{kept} table(s) kept your local writes; their source-derived rows date from earlier moments"
                ))
            );
        }
        println!();
        return Ok(());
    }

    if check {
        run_sql(&repo, "SELECT gfs.refresh_drift_state();").await?;
    }

    let raw = run_sql(
        &repo,
        "SELECT m.src_schema || '.' || m.src_table, \
                d.drifted::text, \
                COALESCE(d.reason, ''), \
                gfs.relation_diverged_sql(d.relid)::text, \
                to_char(d.checked_at, 'YYYY-MM-DD HH24:MI:SS') \
           FROM gfs.drift_state d JOIN gfs.source_map m ON m.relid = d.relid \
          ORDER BY d.drifted DESC, 1",
    )
    .await?;

    // findings that belong to no registered table (a new table on the source,
    // movement nothing accounts for) live in gfs.drift_notes, not drift_state
    let notes_raw = run_sql(
        &repo,
        "SELECT kind, subject, COALESCE(detail,'') FROM gfs.drift_notes ORDER BY kind, subject",
    )
    .await
    .unwrap_or_default();
    let notes = rows(&notes_raw);

    let all = rows(&raw);
    let changed: Vec<&Vec<String>> = all.iter().filter(|r| r[1] == "true").collect();
    let conflicts = changed.iter().filter(|r| r[3] == "true").count();
    let last_checked = all.first().map(|r| r[4].clone()).unwrap_or_default();

    // #131: the copy-coherence verdict. Local-only (gfs.copy_watermark), so it
    // costs nothing even when the source is unreachable; None on clones whose
    // extension predates watermarking, and the output is then byte-identical
    // to pre-#131.
    let moments = clone_moments(&repo).await;

    if json_output {
        let mut out = json!({
            "tables_tracked": all.len(),
            "tables_changed": changed.len(),
            "conflicts": conflicts,
            "last_checked": last_checked,
            "changed": changed.iter().map(|r| json!({
                "table": r[0], "reason": r[2], "conflict": r[3] == "true"
            })).collect::<Vec<_>>(),
        });
        if let Some(m) = &moments {
            out["state"] = json!(m.state);
            out["moments"] = json!(m.moment_count);
            out["wal_span"] = json!([m.span_min, m.span_max]);
            out["torn"] = json!(m.torn);
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!();
    if all.is_empty() {
        println!("  {} no tables tracked yet", dimmed("·"));
        return Ok(());
    }

    if changed.is_empty() && notes.is_empty() {
        println!(
            "  {} source unchanged {}",
            green("✓"),
            dimmed(format!("({} tables tracked)", all.len()))
        );
    } else {
        println!(
            "  {} {} of {} tables changed on the source",
            yellow("!"),
            bold(changed.len().to_string()),
            all.len()
        );
        println!();
        for r in &changed {
            if r[3] == "true" {
                println!("    {} {}", red("conflict"), cyan(&r[0]));
                println!(
                    "      {}",
                    dimmed("you have local writes AND the source changed")
                );
            } else {
                println!("    {} {}", yellow("changed "), cyan(&r[0]));
                if !r[2].is_empty() {
                    println!("      {}", dimmed(&r[2]));
                }
            }
        }
        println!();
        println!(
            "  {}",
            dimmed("reads of these tables go to the source, so they are")
        );
        println!(
            "  {}",
            dimmed("correct but slower. run `gfs pull` to make them local again.")
        );
    }

    for n in &notes {
        let label = match n[0].as_str() {
            "new_table" => "new table",
            "unattributed" => "unattributed",
            _ => "unaccounted",
        };
        println!("    {} {}", yellow(label), cyan(&n[1]));
        if !n[2].is_empty() {
            println!("      {}", dimmed(&n[2]));
        }
    }

    // #131: coherence. Tornness is a different fact from drift -- a clone can
    // be torn while the drift verdict above is quiet (a pull re-anchored the
    // baseline), and stale-but-coherent while it is loud. Say which.
    if let Some(m) = &moments {
        if m.torn {
            println!();
            println!(
                "  {} this clone spans {} source moments {}",
                yellow("!"),
                bold(m.moment_count.to_string()),
                dimmed(format!("(WAL {} \u{2192} {})", m.span_min, m.span_max))
            );
            println!(
                "    {}",
                dimmed("tables were copied while the source kept moving, so a JOIN across")
            );
            println!(
                "    {}",
                dimmed("them can return combinations that never existed on the source at")
            );
            println!(
                "    {}",
                dimmed("any instant. `gfs freeze` re-copies everything from one instant")
            );
            println!(
                "    {}",
                dimmed("(tables you have written to keep your changes); `gfs pull` re-syncs")
            );
            println!(
                "    {}",
                dimmed("changed tables but stays lazy, so it narrows the span only if the")
            );
            println!(
                "    {}",
                dimmed("next reads happen while the source is quiet.")
            );
        } else if m.copied >= 2 && m.unmarked == 0 {
            println!();
            println!(
                "  {} all copied data is from one source moment",
                green("\u{2713}")
            );
        }
        if m.unmarked > 0 {
            println!(
                "    {}",
                dimmed(format!(
                    "({} copied table(s) carry no copy watermark; their moment is unknown)",
                    m.unmarked
                ))
            );
        }
    }

    if !check && !last_checked.is_empty() {
        println!();
        println!(
            "  {} {}",
            dimmed(format!("as of {last_checked}")),
            dimmed("(use --check to probe the source now)")
        );
    }
    println!();
    Ok(())
}

/// `gfs pull` — put tables the source has changed back on the lazy path.
///
/// Copies nothing itself: it clears the cached state so the next read re-fetches,
/// and the existing cost model decides copy-vs-federate as it would for a fresh
/// clone. Tables with local writes are refused unless `--force`, since resetting
/// them would discard the user's work.
pub async fn pull(
    path: Option<PathBuf>,
    force: bool,
    auto: Option<String>,
    auto_schema: Option<String>,
    json_output: bool,
) -> Result<()> {
    let repo = path.unwrap_or_else(get_repo_dir);

    // #132: a frozen clone has no source to sync; nothing here may run -- not
    // the pull itself and not the auto-pull switches (they would only arm
    // machinery the frozen guards keep off anyway).
    if let Some((true, at, _lsn)) = frozen_info(&repo).await {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "detached": true, "frozen_at": at }))?
            );
            return Ok(());
        }
        println!();
        println!(
            "  {} this clone is detached {} — pull does nothing",
            green("\u{2713}"),
            dimmed(format!("(frozen {at})"))
        );
        if auto.is_some() || auto_schema.is_some() {
            println!(
                "    {}",
                dimmed("auto-pull settings have no effect on a frozen clone")
            );
        }
        println!();
        return Ok(());
    }

    let parse_on = |v: &str, flag: &str| -> Result<bool> {
        match v.to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" | "1" => Ok(true),
            "off" | "false" | "no" | "0" => Ok(false),
            other => bail!("{flag} expects 'on' or 'off' (got '{other}')"),
        }
    };

    // `--auto-schema on|off` flips SHAPE repair, the counterpart of `--auto` for
    // rows. Off by default for the same reason: a clone is a branch, so it does
    // not change under you unless you ask.
    if let Some(a) = auto_schema {
        let on = parse_on(&a, "--auto-schema")?;
        run_sql(
            &repo,
            &format!("UPDATE gfs.sync_policy SET autoschema = {on};"),
        )
        .await?;
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "autoschema": on }))?
            );
        } else {
            println!();
            println!(
                "  {} auto schema repair {}",
                green("✓"),
                bold(if on { "on" } else { "off" })
            );
            println!(
                "    {}",
                dimmed(if on {
                    "a source shape change is re-imported automatically (additive changes only)"
                } else {
                    "a source shape change fails with a clear message until you run `gfs pull`"
                })
            );
            println!(
                "    {}",
                dimmed(
                    "a column dropped on the source is never applied automatically: that could destroy local data"
                )
            );
            println!();
        }
        return Ok(());
    }

    // `--auto on|off` only flips the setting; it does not also pull.
    if let Some(a) = auto {
        let on = parse_on(&a, "--auto")?;
        run_sql(
            &repo,
            &format!("UPDATE gfs.sync_policy SET autopull = {on};"),
        )
        .await?;
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "autopull": on }))?
            );
        } else {
            println!();
            println!(
                "  {} auto-pull {}",
                green("✓"),
                bold(if on { "on" } else { "off" })
            );
            println!(
                "    {}",
                dimmed(if on {
                    "a changed table costs one query from the source, then it is re-copied"
                } else {
                    "changed tables are read from the source until you run `gfs pull`"
                })
            );
            println!();
        }
        return Ok(());
    }

    let raw = run_sql(
        &repo,
        &format!("SELECT action, tbl, detail FROM gfs.pull({force});"),
    )
    .await?;
    let actions = rows(&raw);

    let reset = actions.iter().filter(|r| r[0] == "reset").count();
    let seqs = actions.iter().filter(|r| r[0] == "sequence").count();
    let conflicts: Vec<&Vec<String>> = actions.iter().filter(|r| r[0] == "conflict").collect();

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "reset": reset,
                "conflicts": conflicts.len(),
                "actions": actions.iter().map(|r| json!({
                    "action": r[0], "table": r[1], "detail": r[2]
                })).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }

    println!();
    for r in actions
        .iter()
        .filter(|r| r[0] == "schema" || r[0] == "sequence" || r[0] == "enum")
    {
        println!("  {} {} {}", green("✓"), cyan(&r[1]), dimmed(&r[2]));
    }
    if actions.is_empty() {
        println!("  {} already up to date", green("✓"));
        println!();
        return Ok(());
    }

    if seqs > 0 && reset == 0 {
        println!(
            "    {}",
            dimmed("local inserts would otherwise have collided with rows fetched from the source")
        );
    }
    if reset > 0 {
        println!(
            "  {} {} table(s) back on the lazy path",
            green("✓"),
            bold(reset.to_string())
        );
        for r in actions.iter().filter(|r| r[0] == "reset") {
            println!("    {} {}", dimmed("reset"), cyan(&r[1]));
        }
        println!(
            "    {}",
            dimmed("the next read of each fetches from the source")
        );
    }

    if !conflicts.is_empty() {
        println!();
        println!(
            "  {} {} table(s) NOT touched",
            red("!"),
            bold(conflicts.len().to_string())
        );
        // Print each conflict's OWN reason: a local-write conflict and a schema
        // conflict are different situations, and a single hardcoded line claimed
        // the user had local writes even when the clash was purely structural.
        for r in &conflicts {
            println!("    {} {}", red("conflict"), cyan(&r[1]));
            println!("      {}", dimmed(r[2].trim_start_matches("conflict: ")));
        }
        if conflicts.iter().any(|r| r[2].contains("local writes")) {
            println!(
                "    {}",
                dimmed("use `gfs pull --force` to discard yours and take the source's")
            );
        }
    }
    println!();
    Ok(())
}

/// `gfs remote` -- show the source this clone reads from.
///
/// Like `git remote`, this makes NO network round trip: it reports what is
/// recorded locally (the FDW server the clone was bootstrapped with) and leaves
/// probing to `gfs fetch --check`. A routine "where am I cloned from?" must not
/// hang when the source is slow or gone.
///
/// The query touches pg_catalog only, so the planner hook cannot fire and
/// nothing is hydrated as a side effect of asking. The mapped PASSWORD is
/// deliberately never selected, let alone printed.
pub async fn remote(path: Option<PathBuf>, json_output: bool) -> Result<()> {
    let repo = path.unwrap_or_else(get_repo_dir);

    let raw = run_sql(
        &repo,
        "SELECT COALESCE((SELECT option_value FROM pg_options_to_table(s.srvoptions) \
                           WHERE option_name = 'host'), ''), \
                COALESCE((SELECT option_value FROM pg_options_to_table(s.srvoptions) \
                           WHERE option_name = 'port'), ''), \
                COALESCE((SELECT option_value FROM pg_options_to_table(s.srvoptions) \
                           WHERE option_name = 'dbname'), ''), \
                COALESCE((SELECT u.option_value FROM pg_user_mappings um, \
                                pg_options_to_table(um.umoptions) u \
                           WHERE um.srvname = s.srvname AND u.option_name = 'user' \
                           LIMIT 1), '') \
           FROM pg_foreign_server s WHERE s.srvname = 'gfs_remote_srv'",
    )
    .await?;

    // this query never references gfs.*, so run_sql's "not a lazy clone" check
    // cannot trip; an empty result is how a non-clone repository answers
    let Some(r) = rows(&raw).into_iter().next().filter(|r| r.len() >= 4) else {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "url": null, "push": false }))?
            );
        } else {
            println!();
            println!(
                "  {} this repository has no source (not a clone)",
                dimmed("\u{b7}")
            );
            println!();
        }
        return Ok(());
    };

    let (host, port, dbname, user) = (&r[0], &r[1], &r[2], &r[3]);
    let auth = if user.is_empty() {
        String::new()
    } else {
        format!("{user}@")
    };
    let url = format!("postgres://{auth}{host}:{port}/{dbname}");

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "url": url, "host": host, "port": port, "dbname": dbname,
                "user": user, "push": false,
            }))?
        );
        return Ok(());
    }

    println!();
    println!(
        "  {} {}  {}",
        bold("origin"),
        cyan(&url),
        dimmed("(fetch only)")
    );
    println!(
        "  {}",
        dimmed("GFS never writes to the source; there is no `gfs push`.")
    );
    println!();
    Ok(())
}

/// Fully materialize any not-yet-fetched tables of a lazy clone, returning how many
/// were warmed (0 if none, and simply 0 when this repository is not a clone).
///
/// `gfs export` runs pg_dump against the clone's LOCAL tables, which bypasses the
/// copy-on-read planner hook entirely. A table that was never read holds no rows
/// locally, so it is dumped EMPTY: a valid-looking file silently missing data
/// (#116). Warming first is what makes the dump complete.
///
/// gfs.warm() merges the source's rows UNDER any local writes (ON CONFLICT DO
/// NOTHING plus tombstone exclusion), so a diverged table keeps the user's inserts,
/// updates and deletes rather than having them overwritten by the source.
///
/// A failure to warm is deliberately propagated rather than skipped: silently
/// falling through would reintroduce exactly the incomplete dump this prevents.
pub(crate) async fn materialize_clone(repo_path: &Path, quiet: bool) -> Result<i64> {
    // #132: a frozen clone must not warm anything -- warming contacts the
    // source the user sealed away (gfs.warm refuses, too). By construction
    // every non-diverged table is whole_cached already; tables kept for local
    // writes export exactly as they stand, which is the branch the user chose.
    if let Some((true, _, _)) = frozen_info(repo_path).await {
        if !quiet
            && let Ok(out) = run_sql(
                repo_path,
                "SELECT count(*) FROM gfs.clone_source WHERE NOT whole_cached",
            )
            .await
        {
            let partial: i64 = out.trim().parse().unwrap_or(0);
            if partial > 0 {
                println!(
                    "  {} frozen clone: {} table(s) kept with your local writes export as they stand",
                    dimmed("\u{b7}"),
                    partial
                );
            }
        }
        return Ok(0);
    }

    // Not a clone (no gfs catalog) -> nothing to materialize, and export is a
    // plain dump of a normal repository.
    let pending = match run_sql(
        repo_path,
        "SELECT count(*) FROM gfs.clone_source WHERE NOT whole_cached",
    )
    .await
    {
        Ok(out) => out.trim().parse::<i64>().unwrap_or(0),
        Err(_) => return Ok(0),
    };
    if pending == 0 {
        return Ok(0);
    }

    if !quiet {
        println!(
            "  {} fetching {} table(s) not yet copied, so the export is complete",
            dimmed("·"),
            pending
        );
    }
    run_sql(
        repo_path,
        "SELECT gfs.warm(relid) FROM gfs.clone_source WHERE NOT whole_cached",
    )
    .await
    .context(
        "failed to materialize the clone; export aborted rather than write an incomplete dump",
    )?;

    Ok(pending)
}
