//! `gfs status` — show repository and compute status (RFC 006).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_compute_docker::DockerCompute;
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::model::status::StatusResponse;
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::ports::repository::Repository;
use gfs_domain::usecases::repository::status_repo_usecase::StatusRepoUseCase;

use crate::cli_utils::{get_repo_dir, relativize_to_repo};
use crate::output::{bold, cyan, dimmed, green, red, yellow};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(path: Option<PathBuf>, output: String) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);

    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute = Arc::new(DockerCompute::new().context(
        "failed to connect to Docker/Podman daemon (is your container runtime running?)",
    )?);
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

    Ok(())
}

// ---------------------------------------------------------------------------
// Output formats
// ---------------------------------------------------------------------------

const LABEL_WIDTH: usize = 20;

fn print_table(s: &StatusResponse, repo_path: &Path) {
    // Repository section
    println!("  {}", bold("Repository"));
    println!("  {}", "─".repeat(40));
    println!(
        "  {:<width$} {}",
        "Branch",
        cyan(&s.current_branch),
        width = LABEL_WIDTH
    );
    if let Some(ref active) = s.active_workspace_data_dir {
        println!(
            "  {:<width$} {}",
            "Active workspace",
            relativize_to_repo(repo_path, active),
            width = LABEL_WIDTH
        );
    }
    println!();

    if let Some(ref c) = s.compute {
        let status_dot = status_indicator_colored(&c.container_status);
        println!("  {}", bold("Compute"));
        println!("  {}", "─".repeat(40));
        println!(
            "  {:<width$} {}",
            "Provider",
            c.provider,
            width = LABEL_WIDTH
        );
        println!("  {:<width$} {}", "Version", c.version, width = LABEL_WIDTH);
        println!(
            "  {:<width$} {} {}",
            "Status",
            status_dot,
            c.container_status,
            width = LABEL_WIDTH
        );
        println!(
            "  {:<width$} {}",
            "Container ID",
            dimmed(truncate_id(&c.container_id)),
            width = LABEL_WIDTH
        );
        if let Some(ref bind) = c.data_bind_host_path {
            println!(
                "  {:<width$} {}",
                "Container data dir",
                relativize_to_repo(repo_path, bind),
                width = LABEL_WIDTH
            );
        }
        if !c.connection_string.is_empty() {
            println!(
                "  {:<width$} {}",
                "Connection",
                c.connection_string,
                width = LABEL_WIDTH
            );
        }
    } else {
        println!("  {}", bold("Compute"));
        println!("  {}", "─".repeat(40));
        println!("  (no compute instance configured)");
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
