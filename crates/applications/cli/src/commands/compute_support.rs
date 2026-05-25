use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use gfs_compute_docker::DockerCompute;
use gfs_compute_kubernetes::KubernetesCompute;
use gfs_console_remote::block_direct_kubernetes_env;
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

pub async fn compute_for_repo(
    repository: &Arc<dyn Repository>,
    repo_path: &Path,
) -> Result<Arc<dyn Compute>> {
    let runtime = repository.get_runtime_config(repo_path).await?;
    let has_runtime = runtime
        .as_ref()
        .is_some_and(|runtime| !runtime.container_name.trim().is_empty());

    if has_runtime {
        let provider = runtime
            .as_ref()
            .map(|r| r.runtime_provider.trim().to_ascii_lowercase())
            .unwrap_or_else(|| "docker".to_string());

        match provider.as_str() {
            "guepard" | "console" | "remote" => {
                block_direct_kubernetes_env()?;
                anyhow::bail!(
                    "guepard remote repository: VCS runs via console API (gfs commit/log/checkout), not local compute"
                );
            }
            "kubernetes" | "k8s" | "k3s" => Ok(Arc::new(
                KubernetesCompute::new(None)
                    .await
                    .context("failed to connect to Kubernetes (check KUBECONFIG / k3s)")?,
            )),
            _ => Ok(Arc::new(DockerCompute::new().context(
                "failed to connect to Docker/Podman daemon (is your container runtime running?)",
            )?)),
        }
    } else {
        Ok(Arc::new(NoopCompute))
    }
}
