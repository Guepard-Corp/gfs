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
fn rows(raw: &str) -> Vec<Vec<String>> {
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split('|').map(|c| c.trim().to_string()).collect())
        .collect()
}

/// `gfs fetch` — report what has changed on the source. Never modifies the clone.
///
/// Reads the cached verdict by default so it is fast and works while the source is
/// unreachable; `--check` forces a fresh probe.
pub async fn fetch(path: Option<PathBuf>, check: bool, json_output: bool) -> Result<()> {
    let repo = path.unwrap_or_else(get_repo_dir);

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

    let all = rows(&raw);
    let changed: Vec<&Vec<String>> = all.iter().filter(|r| r[1] == "true").collect();
    let conflicts = changed.iter().filter(|r| r[3] == "true").count();
    let last_checked = all.first().map(|r| r[4].clone()).unwrap_or_default();

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "tables_tracked": all.len(),
                "tables_changed": changed.len(),
                "conflicts": conflicts,
                "last_checked": last_checked,
                "changed": changed.iter().map(|r| json!({
                    "table": r[0], "reason": r[2], "conflict": r[3] == "true"
                })).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }

    println!();
    if all.is_empty() {
        println!("  {} no tables tracked yet", dimmed("·"));
        return Ok(());
    }

    if changed.is_empty() {
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
                println!("      {}", dimmed("you have local writes AND the source changed"));
            } else {
                println!("    {} {}", yellow("changed "), cyan(&r[0]));
                if !r[2].is_empty() {
                    println!("      {}", dimmed(&r[2]));
                }
            }
        }
        println!();
        println!("  {}", dimmed("reads of these tables go to the source, so they are"));
        println!("  {}", dimmed("correct but slower. run `gfs pull` to make them local again."));
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
        run_sql(&repo, &format!("UPDATE gfs.sync_policy SET autoschema = {on};")).await?;
        if json_output {
            println!("{}", serde_json::to_string_pretty(&json!({ "autoschema": on }))?);
        } else {
            println!();
            println!("  {} auto schema repair {}", green("✓"), bold(if on { "on" } else { "off" }));
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
                dimmed("a column dropped on the source is never applied automatically: that could destroy local data")
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
            println!("{}", serde_json::to_string_pretty(&json!({ "autopull": on }))?);
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
    for r in actions.iter().filter(|r| r[0] == "schema" || r[0] == "sequence" || r[0] == "enum") {
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
        println!("    {}", dimmed("the next read of each fetches from the source"));
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
    .context("failed to materialize the clone; export aborted rather than write an incomplete dump")?;

    Ok(pending)
}
