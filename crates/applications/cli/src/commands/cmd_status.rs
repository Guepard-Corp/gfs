//! `gfs status` — show repository and compute status.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::model::status::{SourceStatus, StatusResponse};
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::ports::repository::Repository;
use gfs_domain::usecases::repository::status_repo_usecase::StatusRepoUseCase;

use crate::cli_utils::{get_repo_dir, relativize_to_repo};
use crate::commands::cmd_source;
use crate::commands::compute_support::compute_for_repo;
use crate::output::{
    BOX_V, bold, box_bottom, box_row, box_top, cyan, dimmed, fmt_box_row, fmt_box_row_colored,
    green, red, yellow,
};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Returns exit code: 0 = compute running (or no compute configured), 1 = compute not running.
pub async fn run(path: Option<PathBuf>, output: String) -> Result<i32> {
    let repo_path = path.clone().unwrap_or_else(get_repo_dir);

    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute = compute_for_repo(&repository, &repo_path).await?;
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    gfs_db_providers::register_all(registry.as_ref())
        .context("failed to register database providers")?;

    let use_case = StatusRepoUseCase::new(repository, compute, registry);
    let mut status = use_case
        .run(&repo_path)
        .await
        .context("not a GFS repository (run from a repo root or use --path <dir>)")?;

    // Only ask the clone where it stands if there is a running database to ask.
    let running = matches!(&status.compute, Some(c) if c.container_status == "running");
    let mut moments = None;
    if running {
        status.source = source_summary(&repo_path).await;
        // #131: the copy-coherence verdict. A LOCAL read (gfs.copy_watermark),
        // so it respects this command's no-probe rule; None on databases that
        // predate watermarking, and the output is then unchanged.
        if status.source.is_some() {
            moments = cmd_source::clone_moments(&repo_path).await;
        }
    }

    match output.as_str() {
        "json" => print_json(&status),
        _ => print_table(&status, &repo_path, moments.as_ref()),
    }

    // Exit code: 0 if no compute or compute is running, 1 otherwise.
    let exit_code = match &status.compute {
        Some(c) if c.container_status != "running" => 1,
        _ => 0,
    };

    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// Source (lazy clones only)
// ---------------------------------------------------------------------------

/// Read the clone's last known verdict about its source.
///
/// Deliberately does not probe. `git status` does not contact the remote either:
/// it reports what is already known and leaves the round trip to `git fetch`.
/// Probing here would make a routine `gfs status` hang whenever the source is
/// slow or gone, which is exactly when you most want to run it.
///
/// `None` means there is nothing to report -- the repository is not a lazy clone,
/// or its database did not answer. Neither is an error for `gfs status`, so the
/// section is simply omitted rather than failing the command.
async fn source_summary(repo_path: &Path) -> Option<SourceStatus> {
    let raw = cmd_source::run_sql(
        repo_path,
        "SELECT count(*), \
                count(*) FILTER (WHERE d.drifted), \
                count(*) FILTER (WHERE d.drifted \
                                   AND gfs.relation_diverged_sql(d.relid)), \
                COALESCE(to_char(max(d.checked_at), 'YYYY-MM-DD HH24:MI:SS'), '') \
           FROM gfs.drift_state d",
    )
    .await
    .ok()?;

    let r = cmd_source::rows(&raw).into_iter().next()?;
    if r.len() < 4 {
        return None;
    }
    let mut st = SourceStatus {
        tracked: r[0].parse().ok()?,
        behind: r[1].parse().ok()?,
        diverged: r[2].parse().ok()?,
        last_checked: r[3].clone(),
        frozen: None,
        frozen_at: None,
    };

    // #132: frozen (detached snapshot) state. One extra LOCAL read; a clone
    // that predates snapshot mode has no gfs.clone_mode and the fields stay
    // None, so lazy-clone output (JSON included) is byte-identical to #133's.
    if let Some((true, at, _lsn)) = cmd_source::frozen_info(repo_path).await {
        st.frozen = Some(true);
        if !at.is_empty() {
            st.frozen_at = Some(at);
        }
        // Live drift cannot exist on a frozen clone; repurpose `diverged` as
        // the count of tables the freeze KEPT for local writes (documented on
        // the field) so the frozen box can say so without a second struct.
        st.behind = 0;
        st.diverged = cmd_source::kept_count(repo_path).await;
    }
    Some(st)
}

fn print_source(s: &SourceStatus, moments: Option<&cmd_source::CloneMoments>) {
    println!();
    println!("{}", box_top(&bold("Source"), BOX_W));

    // #132: a frozen clone is a sealed snapshot; behind/diverged/checked no
    // longer mean anything (nothing is compared any more), so say what it IS.
    if s.frozen == Some(true) {
        let state = "frozen snapshot";
        let row = fmt_box_row_colored("State", &cyan(state), state, LABEL_W, BOX_W);
        println!("{}", box_row(&row, BOX_W));
        if let Some(at) = &s.frozen_at {
            let row = fmt_box_row("Frozen at", at, LABEL_W, BOX_W);
            println!("{}", box_row(&row, BOX_W));
        }
        let row = fmt_box_row("Tracked tables", &s.tracked.to_string(), LABEL_W, BOX_W);
        println!("{}", box_row(&row, BOX_W));
        if s.diverged > 0 {
            let d = s.diverged.to_string();
            let row = fmt_box_row_colored("Kept", &yellow(&d).to_string(), &d, LABEL_W, BOX_W);
            println!("{}", box_row(&row, BOX_W));
        }
        println!("{}", box_bottom(BOX_W));
        println!();
        println!(
            "  {}",
            dimmed("a point-in-time snapshot; the source is never consulted")
        );
        if s.diverged > 0 {
            // #131: when the watermarks can date them, say the sharper thing --
            // kept tables were not re-copied, so their source rows are older
            // than the freeze instant. Never reported as "torn": keeping your
            // writes is what makes this clone a branch.
            if moments.map(|m| m.diverged_stale).unwrap_or(0) > 0 {
                println!(
                    "  {}",
                    dimmed(
                        "kept tables preserve your local writes; their source rows predate the freeze"
                    )
                );
            } else {
                println!(
                    "  {}",
                    dimmed("kept tables preserve your local writes (they are your branch)")
                );
            }
        }
        return;
    }

    // Zeros here would read as "up to date"; nothing has been compared yet.
    if s.tracked == 0 {
        let msg = format!("{:<w$}", "(not checked yet)", w = BOX_W);
        println!("  {} {} {}", BOX_V, dimmed(&msg), BOX_V);
        println!("{}", box_bottom(BOX_W));
        println!();
        println!(
            "  {}",
            dimmed("`gfs fetch --check` to probe the source now")
        );
        return;
    }

    let row = fmt_box_row("Tracked tables", &s.tracked.to_string(), LABEL_W, BOX_W);
    println!("{}", box_row(&row, BOX_W));

    // "behind" counts every table the source has changed; the diverged ones are a
    // subset of those, and are the only ones `gfs pull` cannot resolve on its own.
    let behind = s.behind.to_string();
    let behind_colored = if s.behind > 0 {
        yellow(&behind).to_string()
    } else {
        behind.clone()
    };
    let row = fmt_box_row_colored("Behind", &behind_colored, &behind, LABEL_W, BOX_W);
    println!("{}", box_row(&row, BOX_W));

    if s.diverged > 0 {
        let d = s.diverged.to_string();
        let row = fmt_box_row_colored("Diverged", &red(&d).to_string(), &d, LABEL_W, BOX_W);
        println!("{}", box_row(&row, BOX_W));
    }

    // #131: does this clone mix source moments? A copied-at-different-moments
    // clone can look clean above (drift is a different fact), so it gets its
    // own row. Omitted while nothing has been copied or the verdict is absent.
    let torn = moments.map(|m| m.torn).unwrap_or(false);
    if let Some(m) = moments {
        if m.torn {
            let v = format!("spans \u{2265}{} (torn)", m.moment_count);
            let row = fmt_box_row_colored("Moments", &yellow(&v).to_string(), &v, LABEL_W, BOX_W);
            println!("{}", box_row(&row, BOX_W));
        } else if m.copied >= 2 && m.unmarked == 0 {
            let row = fmt_box_row("Moments", "single", LABEL_W, BOX_W);
            println!("{}", box_row(&row, BOX_W));
        } else if m.copied > 0 && m.unmarked > 0 {
            let row = fmt_box_row("Moments", "unknown", LABEL_W, BOX_W);
            println!("{}", box_row(&row, BOX_W));
        }
    }

    if !s.last_checked.is_empty() {
        let row = fmt_box_row("Checked", &s.last_checked, LABEL_W, BOX_W);
        println!("{}", box_row(&row, BOX_W));
    }
    println!("{}", box_bottom(BOX_W));

    if s.behind > 0 {
        println!();
        println!(
            "  {}",
            dimmed("`gfs fetch` for detail, `gfs pull` to make these tables local again")
        );
    }
    if torn {
        println!();
        println!(
            "  {}",
            dimmed("`gfs fetch` shows the span; `gfs freeze` makes this clone one moment again")
        );
    }
}

// ---------------------------------------------------------------------------
// Output formats
// ---------------------------------------------------------------------------

const LABEL_W: usize = 20;
const BOX_W: usize = 40;

fn print_table(s: &StatusResponse, repo_path: &Path, moments: Option<&cmd_source::CloneMoments>) {
    // Repository section
    println!("{}", box_top(&bold("Repository"), BOX_W));

    let branch_row = fmt_box_row_colored(
        "Branch",
        &cyan(&s.current_branch),
        &s.current_branch,
        LABEL_W,
        BOX_W,
    );
    println!("{}", box_row(&branch_row, BOX_W));

    if let Some(ref active) = s.active_workspace_data_dir {
        let rel = relativize_to_repo(repo_path, active);
        let row = fmt_box_row("Active workspace", &rel, LABEL_W, BOX_W);
        println!("{}", box_row(&row, BOX_W));
    }
    // `gfs status --help` promises a connection string. A container-backed
    // provider prints one in the Compute section below; an embedded one has no
    // Compute section, so it printed none at all.
    if s.compute.is_none()
        && let Some(ref conn) = s.connection_string
    {
        let row = fmt_box_row("Connection", conn, LABEL_W, BOX_W);
        println!("{}", box_row(&row, BOX_W));
    }
    println!("{}", box_bottom(BOX_W));

    println!();

    if let Some(ref c) = s.compute {
        let status_dot = status_indicator_colored(&c.container_status);
        let status_raw = format!(
            "{} {}",
            status_indicator(&c.container_status),
            c.container_status
        );

        println!("{}", box_top(&bold("Compute"), BOX_W));

        let row = fmt_box_row("Provider", &c.provider, LABEL_W, BOX_W);
        println!("{}", box_row(&row, BOX_W));

        let row = fmt_box_row("Version", &c.version, LABEL_W, BOX_W);
        println!("{}", box_row(&row, BOX_W));

        let status_colored = format!("{} {}", status_dot, c.container_status);
        let row = fmt_box_row_colored("Status", &status_colored, &status_raw, LABEL_W, BOX_W);
        println!("{}", box_row(&row, BOX_W));

        let truncated = truncate_id(&c.container_id);
        let row = fmt_box_row_colored(
            "Container ID",
            &dimmed(&truncated),
            &truncated,
            LABEL_W,
            BOX_W,
        );
        println!("{}", box_row(&row, BOX_W));

        if let Some(ref bind) = c.data_bind_host_path {
            let rel = relativize_to_repo(repo_path, bind);
            let row = fmt_box_row("Container data dir", &rel, LABEL_W, BOX_W);
            println!("{}", box_row(&row, BOX_W));
        }
        if !c.connection_string.is_empty() {
            let row = fmt_box_row("Connection", &c.connection_string, LABEL_W, BOX_W);
            println!("{}", box_row(&row, BOX_W));
        }
        println!("{}", box_bottom(BOX_W));
    } else {
        println!("{}", box_top(&bold("Compute"), BOX_W));
        let msg = format!("{:<w$}", "(no compute instance configured)", w = BOX_W);
        println!("  {} {} {}", BOX_V, dimmed(&msg), BOX_V);
        println!("{}", box_bottom(BOX_W));
    }

    if let Some(ref src) = s.source {
        print_source(src, moments);
    }

    if let Some(ref warning) = s.bind_mismatch_warning {
        println!();
        println!("  {}  {}", yellow("⚠"), yellow(warning));
    }
}

/// Single-character indicator for container status (for quick scanning).
fn status_indicator(status: &str) -> &'static str {
    match status {
        "running" => "●",
        "starting" | "restarting" => "◐",
        "stopped" | "stopping" | "not_provisioned" => "○",
        "paused" => "◌",
        "failed" | "unknown" => "✕",
        _ => "•",
    }
}

/// Status indicator with color applied (green=ok, yellow=transitioning, red=bad).
fn status_indicator_colored(status: &str) -> String {
    let dot = status_indicator(status);
    match status {
        "running" => green(dot).to_string(),
        "starting" | "restarting" => yellow(dot).to_string(),
        "stopped" | "stopping" | "not_provisioned" | "paused" => dimmed(dot).to_string(),
        "failed" | "unknown" => red(dot).to_string(),
        _ => dot.to_string(),
    }
}

/// Shorten container ID for display (first 12 chars, like docker ps).
fn truncate_id(id: &str) -> String {
    if id.len() <= 16 {
        id.to_string()
    } else {
        format!("{}…", &id[..12])
    }
}

fn print_json(s: &StatusResponse) {
    let out = serde_json::to_string_pretty(s).expect("status serialization");
    println!("{}", out);
}
