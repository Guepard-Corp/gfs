use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_compute_docker::DockerCompute;
use gfs_compute_kubernetes::KubernetesCompute;
use gfs_domain::ports::compute::{Compute, NoopCompute};
use gfs_domain::ports::repository::Repository;

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
            "kubernetes" | "k8s" | "k3s" => {
                Ok(Arc::new(KubernetesCompute::new(None).await.context(
                    "failed to connect to Kubernetes (check KUBECONFIG / k3s)",
                )?))
            }
            _ => Ok(Arc::new(DockerCompute::new().context(
                "failed to connect to Docker/Podman daemon (is your container runtime running?)",
            )?)),
        }
    } else {
        Ok(Arc::new(NoopCompute))
    }
}
