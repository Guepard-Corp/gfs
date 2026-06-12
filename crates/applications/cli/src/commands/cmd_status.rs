//! `gfs status` — show repository and compute status (RFC 006).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::model::status::StatusResponse;
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::ports::repository::Repository;
use gfs_domain::usecases::repository::status_repo_usecase::StatusRepoUseCase;

use crate::cli_utils::{get_repo_dir, relativize_to_repo};
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

    if super::remote_support::is_remote_repo(&repo_path)? {
        let json_output = output == "json";
        return super::cmd_remote_status::run(path, json_output).await;
    }

    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute = compute_for_repo(&repository, &repo_path).await?;
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    gfs_compute_docker::containers::register_all(registry.as_ref())
        .context("failed to register database providers")?;

    let use_case = StatusRepoUseCase::new(repository, compute, registry);
    let status = use_case
        .run(&repo_path)
        .await
        .context("not a GFS repository (run from a repo root or use --path <dir>)")?;

    match output.as_str() {
        "json" => print_json(&status),
        _ => print_table(&status, &repo_path),
    }

    // Exit code: 0 if no compute or compute is running, 1 otherwise.
    let exit_code = match &status.compute {
        Some(c) if c.container_status != "running" => 1,
        _ => 0,
    };

    Ok(exit_code)
}

// ---------------------------------------------------------------------------
// Output formats
// ---------------------------------------------------------------------------

const LABEL_W: usize = 20;
const BOX_W: usize = 40;

fn print_table(s: &StatusResponse, repo_path: &Path) {
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
