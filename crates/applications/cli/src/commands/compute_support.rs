use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use gfs_compute_docker::DockerCompute;
use gfs_compute_k8s::K8sCompute;
use gfs_domain::model::config::GfsConfig;
use gfs_domain::ports::compute::{
    Compute, ComputeDefinition, ComputeError, ExecOutput, InstanceConnectionInfo, InstanceId,
    InstanceStatus, LogEntry, LogsOptions, StartOptions,
};
use gfs_domain::ports::repository::Repository;

fn missing_runtime_error() -> ComputeError {
    ComputeError::Internal("repository has no configured compute runtime".to_string())
}

#[derive(Debug, Default)]
struct NoopCompute;

#[async_trait]
impl Compute for NoopCompute {
    async fn provision(
        &self,
        _definition: &ComputeDefinition,
    ) -> gfs_domain::ports::compute::Result<InstanceId> {
        Err(missing_runtime_error())
    }

    async fn start(
        &self,
        _id: &InstanceId,
        _options: StartOptions,
    ) -> gfs_domain::ports::compute::Result<InstanceStatus> {
        Err(missing_runtime_error())
    }

    async fn stop(&self, _id: &InstanceId) -> gfs_domain::ports::compute::Result<InstanceStatus> {
        Err(missing_runtime_error())
    }

    async fn restart(
        &self,
        _id: &InstanceId,
    ) -> gfs_domain::ports::compute::Result<InstanceStatus> {
        Err(missing_runtime_error())
    }

    async fn status(&self, _id: &InstanceId) -> gfs_domain::ports::compute::Result<InstanceStatus> {
        Err(missing_runtime_error())
    }

    async fn get_connection_info(
        &self,
        _id: &InstanceId,
        _compute_port: u16,
    ) -> gfs_domain::ports::compute::Result<InstanceConnectionInfo> {
        Err(missing_runtime_error())
    }

    async fn prepare_for_snapshot(
        &self,
        _id: &InstanceId,
        _commands: &[String],
    ) -> gfs_domain::ports::compute::Result<()> {
        Err(missing_runtime_error())
    }

    async fn logs(
        &self,
        _id: &InstanceId,
        _options: LogsOptions,
    ) -> gfs_domain::ports::compute::Result<Vec<LogEntry>> {
        Err(missing_runtime_error())
    }

    async fn pause(&self, _id: &InstanceId) -> gfs_domain::ports::compute::Result<InstanceStatus> {
        Err(missing_runtime_error())
    }

    async fn unpause(
        &self,
        _id: &InstanceId,
    ) -> gfs_domain::ports::compute::Result<InstanceStatus> {
        Err(missing_runtime_error())
    }

    async fn get_instance_data_mount_host_path(
        &self,
        _id: &InstanceId,
        _compute_data_path: &str,
    ) -> gfs_domain::ports::compute::Result<Option<std::path::PathBuf>> {
        Err(missing_runtime_error())
    }

    async fn remove_instance(&self, _id: &InstanceId) -> gfs_domain::ports::compute::Result<()> {
        Err(missing_runtime_error())
    }

    async fn get_task_connection_info(
        &self,
        _id: &InstanceId,
        _compute_port: u16,
    ) -> gfs_domain::ports::compute::Result<InstanceConnectionInfo> {
        Err(missing_runtime_error())
    }

    async fn run_task(
        &self,
        _definition: &ComputeDefinition,
        _command: &str,
        _linked_to: Option<&InstanceId>,
    ) -> gfs_domain::ports::compute::Result<ExecOutput> {
        Err(missing_runtime_error())
    }
}

// ---------------------------------------------------------------------------
// Runtime selection
// ---------------------------------------------------------------------------

/// Build a [`Compute`] adapter from the `runtime_provider` stored in `.gfs/config.toml`.
///
/// - `"k8s"` → [`K8sCompute`]
/// - `"docker"` / `"podman"` / anything else → [`DockerCompute`]
/// - missing / empty provider string → [`NoopCompute`]
async fn build_compute(provider: Option<&str>) -> Result<Arc<dyn Compute>> {
    match provider {
        Some(p) if p == "k8s" => {
            let c = K8sCompute::new()
                .await
                .context("failed to connect to Kubernetes cluster")?;
            Ok(Arc::new(c))
        }
        Some(_) => {
            let c = DockerCompute::new()
                .context("failed to connect to Docker/Podman daemon (is it running?)")?;
            Ok(Arc::new(c))
        }
        None => Ok(Arc::new(NoopCompute)),
    }
}

/// Select the right compute adapter for a repository by reading its runtime config.
/// Use this when you already have a `Repository` port handy.
pub async fn compute_for_repo(
    repository: &Arc<dyn Repository>,
    repo_path: &Path,
) -> Result<Arc<dyn Compute>> {
    let runtime = repository.get_runtime_config(repo_path).await?;
    let provider = runtime.as_ref().and_then(|r| {
        if r.container_name.trim().is_empty() {
            None
        } else {
            Some(r.runtime_provider.as_str())
        }
    });
    build_compute(provider).await
}

/// Select the right compute adapter by loading the GFS config from `repo_path` directly.
/// Use this in CLI commands that don't have a `Repository` object.
pub async fn compute_for_path(repo_path: &Path) -> Result<Arc<dyn Compute>> {
    let config = GfsConfig::load(repo_path).ok();
    let provider = config.as_ref().and_then(|c| {
        c.runtime.as_ref().and_then(|r| {
            if r.container_name.trim().is_empty() {
                None
            } else {
                Some(r.runtime_provider.as_str())
            }
        })
    });
    build_compute(provider).await
}
