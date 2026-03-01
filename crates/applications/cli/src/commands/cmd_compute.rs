use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_compute_docker::DockerCompute;
use gfs_domain::model::config::{GfsConfig, RuntimeConfig};
use gfs_domain::ports::compute::{Compute, InstanceId, InstanceState, InstanceStatus, LogsOptions};
use gfs_domain::ports::database_provider::{
    DatabaseProviderRegistry, InMemoryDatabaseProviderRegistry,
};
use gfs_domain::repo_utils::repo_layout;

use crate::ComputeAction;
use crate::cli_utils::{get_repo_dir, relativize_to_repo};
use crate::output::{dimmed, green, red, yellow};

// ---------------------------------------------------------------------------
// Entry point called from main
// ---------------------------------------------------------------------------

/// Resolve the compute instance id: from action's --id if set, otherwise from
/// repo config at path (or cwd) as runtime.container_name.
fn resolve_id(path: Option<PathBuf>, action: &ComputeAction) -> Result<String> {
    let id_from_action = match action {
        ComputeAction::Status { id } => id.as_deref(),
        ComputeAction::Start { id } => id.as_deref(),
        ComputeAction::Stop { id } => id.as_deref(),
        ComputeAction::Restart { id } => id.as_deref(),
        ComputeAction::Pause { id } => id.as_deref(),
        ComputeAction::Unpause { id } => id.as_deref(),
        ComputeAction::Logs { id, .. } => id.as_deref(),
    };
    if let Some(id) = id_from_action {
        return Ok(id.to_string());
    }
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let config = GfsConfig::load(&repo_path)
        .context("not a gfs repository (use --path <repo> or run from a repo)")?;
    let container_name = config
        .runtime
        .as_ref()
        .map(|r| r.container_name.as_str())
        .filter(|s| !s.is_empty())
        .context("no container_name in repo config (set runtime.container_name or pass --id)")?;
    Ok(container_name.to_string())
}

pub async fn run(path: Option<PathBuf>, action: ComputeAction) -> Result<()> {
    let compute = DockerCompute::new()
        .map_err(|e| anyhow::anyhow!("failed to connect to Docker daemon: {e}"))?;
    let id = resolve_id(path.clone(), &action)?;
    dispatch(&compute, &id, action, path).await
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

async fn dispatch(
    compute: &DockerCompute,
    id: &str,
    action: ComputeAction,
    path: Option<PathBuf>,
) -> Result<()> {
    let instance_id = InstanceId(id.to_string());

    match action {
        ComputeAction::Status { .. } => {
            let status = compute
                .status(&instance_id)
                .await
                .map_err(anyhow::Error::from)?;
            print_status(&status);
            if let Some(ref dir) = container_data_dir(compute, &instance_id, path.clone()).await {
                let repo_path = path.clone().unwrap_or_else(get_repo_dir);
                let rel = relativize_to_repo(&repo_path, dir);
                println!("Container data dir : {rel}");
            }
        }

        ComputeAction::Start { .. } => {
            let repo_path = path.clone().unwrap_or_else(get_repo_dir);
            let (instance_id, status) =
                start_restart_or_recreate(compute, &instance_id, &repo_path, false).await?;
            print_status(&status);
            if let Some(ref dir) = container_data_dir(compute, &instance_id, path).await {
                let rel = relativize_to_repo(&repo_path, dir);
                println!("Container data dir : {rel}");
            }
        }

        ComputeAction::Stop { .. } => {
            let status = compute
                .stop(&instance_id)
                .await
                .map_err(anyhow::Error::from)?;
            print_status(&status);
        }

        ComputeAction::Restart { .. } => {
            let repo_path = path.clone().unwrap_or_else(get_repo_dir);
            let (instance_id, status) =
                start_restart_or_recreate(compute, &instance_id, &repo_path, true).await?;
            print_status(&status);
            if let Some(ref dir) = container_data_dir(compute, &instance_id, path).await {
                let rel = relativize_to_repo(&repo_path, dir);
                println!("Container data dir : {rel}");
            }
        }

        ComputeAction::Pause { .. } => {
            let status = compute
                .pause(&instance_id)
                .await
                .map_err(anyhow::Error::from)?;
            print_status(&status);
        }

        ComputeAction::Unpause { .. } => {
            let status = compute
                .unpause(&instance_id)
                .await
                .map_err(anyhow::Error::from)?;
            print_status(&status);
        }

        ComputeAction::Logs {
            tail,
            since,
            stdout,
            stderr,
            ..
        } => {
            let since_dt = since
                .as_deref()
                .map(|s| {
                    chrono::DateTime::parse_from_rfc3339(s)
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .map_err(|e| anyhow::anyhow!("invalid --since timestamp: {e}"))
                })
                .transpose()?;

            let options = LogsOptions {
                tail,
                since: since_dt,
                stdout: stdout.unwrap_or(true),
                stderr: stderr.unwrap_or(true),
            };

            let entries = compute
                .logs(&instance_id, options)
                .await
                .map_err(anyhow::Error::from)?;

            for entry in &entries {
                println!(
                    "[{}] [{}] {}",
                    entry.timestamp.format("%Y-%m-%dT%H:%M:%SZ"),
                    match entry.stream {
                        gfs_domain::ports::compute::LogStream::Stdout => "stdout",
                        gfs_domain::ports::compute::LogStream::Stderr => "stderr",
                    },
                    entry.message.trim_end()
                );
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Display helper
// ---------------------------------------------------------------------------

fn print_status(s: &InstanceStatus) {
    println!("id          : {}", dimmed(&s.id.0));
    println!("state       : {}", format_state_colored(&s.state));
    if let Some(pid) = s.pid {
        println!("pid         : {pid}");
    }
    if let Some(started_at) = s.started_at {
        println!("started_at  : {}", started_at.format("%Y-%m-%dT%H:%M:%SZ"));
    }
    if let Some(code) = s.exit_code {
        println!("exit_code   : {code}");
    }
}

fn format_state(state: &InstanceState) -> &'static str {
    match state {
        InstanceState::Starting => "starting",
        InstanceState::Running => "running",
        InstanceState::Paused => "paused",
        InstanceState::Stopping => "stopping",
        InstanceState::Stopped => "stopped",
        InstanceState::Restarting => "restarting",
        InstanceState::Failed => "failed",
        InstanceState::Unknown => "unknown",
    }
}

fn format_state_colored(state: &InstanceState) -> String {
    let s = format_state(state);
    match state {
        InstanceState::Running => green(s).to_string(),
        InstanceState::Starting | InstanceState::Restarting => yellow(s).to_string(),
        InstanceState::Stopped | InstanceState::Stopping | InstanceState::Paused => {
            dimmed(s).to_string()
        }
        InstanceState::Failed | InstanceState::Unknown => red(s).to_string(),
    }
}

/// If the container exists and its data bind does not match the active workspace, recreate it
/// (stop, remove, provision with current active workspace, start, update config). Otherwise start or restart the existing container.
/// When `restart_if_same` is true (e.g. for `gfs compute restart`), calls restart instead of start when bind matches.
async fn start_restart_or_recreate(
    compute: &DockerCompute,
    instance_id: &InstanceId,
    repo_path: &std::path::Path,
    restart_if_same: bool,
) -> Result<(InstanceId, InstanceStatus)> {
    let active = match repo_layout::get_active_workspace_data_dir(repo_path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => return just_start_or_restart(compute, instance_id, restart_if_same).await,
    };

    let config = match GfsConfig::load(repo_path) {
        Ok(c) => c,
        Err(_) => return just_start_or_restart(compute, instance_id, restart_if_same).await,
    };
    let provider_name = match &config.environment {
        Some(e) if !e.database_provider.is_empty() => e.database_provider.as_str(),
        _ => return just_start_or_restart(compute, instance_id, restart_if_same).await,
    };

    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    gfs_compute_docker::containers::register_all(registry.as_ref())
        .context("register providers")?;
    let provider = registry
        .get(provider_name)
        .context("unknown database provider")?;
    let compute_data_path = provider
        .definition()
        .data_dir
        .to_string_lossy()
        .into_owned();

    let current_bind = match compute
        .get_instance_data_mount_host_path(instance_id, &compute_data_path)
        .await
    {
        Ok(Some(p)) => p.to_string_lossy().into_owned(),
        _ => return just_start_or_restart(compute, instance_id, restart_if_same).await,
    };

    if !paths_differ(&active, &current_bind) {
        return just_start_or_restart(compute, instance_id, restart_if_same).await;
    }

    compute.stop(instance_id).await?;
    compute.remove_instance(instance_id).await?;

    let mut definition = provider.definition();
    if let Some(ref env) = config.environment
        && !env.database_version.is_empty()
    {
        let base = definition
            .image
            .split(':')
            .next()
            .unwrap_or(&definition.image);
        definition.image = format!("{}:{}", base, env.database_version);
    }
    definition.host_data_dir = Some(std::path::PathBuf::from(&active));
    let new_id = compute.provision(&definition).await?;
    let status = compute.start(&new_id, Default::default()).await?;

    repo_layout::update_runtime_config(
        repo_path,
        RuntimeConfig {
            runtime_provider: "docker".to_string(),
            runtime_version: "24".to_string(),
            container_name: new_id.0.clone(),
        },
    )
    .context("update runtime config with new container name")?;

    Ok((new_id, status))
}

async fn just_start_or_restart(
    compute: &DockerCompute,
    instance_id: &InstanceId,
    restart: bool,
) -> Result<(InstanceId, InstanceStatus)> {
    let status = if restart {
        compute.restart(instance_id).await?
    } else {
        compute.start(instance_id, Default::default()).await?
    };
    Ok((instance_id.clone(), status))
}

fn paths_differ(a: &str, b: &str) -> bool {
    let a = std::path::Path::new(a);
    let b = std::path::Path::new(b);
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a != b,
        _ => a != b,
    }
}

/// Resolve the container's data bind host path from repo config (database provider) and Docker inspect.
async fn container_data_dir(
    compute: &DockerCompute,
    instance_id: &InstanceId,
    path: Option<PathBuf>,
) -> Option<String> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let config = GfsConfig::load(&repo_path).ok()?;
    let provider_name = config.environment.as_ref()?.database_provider.as_str();
    if provider_name.is_empty() {
        return None;
    }
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    gfs_compute_docker::containers::register_all(registry.as_ref()).ok()?;
    let provider = registry.get(provider_name)?;
    let compute_data_path = provider
        .definition()
        .data_dir
        .to_string_lossy()
        .into_owned();
    let host_path = compute
        .get_instance_data_mount_host_path(instance_id, &compute_data_path)
        .await
        .ok()?
        .map(|p| p.to_string_lossy().into_owned())?;
    Some(host_path)
}
