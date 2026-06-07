//! k3s-only: reprovision Postgres after GFS checkout (PVC restore + stable NodePort).

use std::path::Path;
use std::sync::Arc;

use gfs_domain::model::config::{EnvironmentConfig, GfsConfig, RuntimeConfig};
use gfs_domain::ports::compute::{Compute, InstanceId};
use gfs_domain::ports::database_provider::DatabaseProviderRegistry;
use gfs_domain::ports::repository::Repository;
use gfs_domain::ports::storage::{CloneOptions, SnapshotId, StoragePort, VolumeId};
use gfs_storage_kubernetes::KubernetesStorage;

use crate::KubernetesCompute;

#[derive(Debug, thiserror::Error)]
pub enum K8sCheckoutReprovisionError {
    #[error("config: {0}")]
    Config(String),

    #[error("not configured: {0}")]
    NotConfigured(String),

    #[error("unknown provider: {0}")]
    UnknownProvider(String),

    #[error("compute: {0}")]
    Compute(String),

    #[error("repository: {0}")]
    Repository(String),

    #[error("storage: {0}")]
    Storage(String),
}

/// Stable ZFS-backed PVC name for Postgres data (matches `ensure_pvc` / init).
pub fn stable_data_pvc(instance: &str) -> String {
    format!("{}-data", instance.trim())
}

/// Inverse of [`stable_data_pvc`]: the owning instance of a `{instance}-data` PVC.
fn source_instance_from_pvc(pvc_name: &str) -> Option<&str> {
    pvc_name.strip_suffix("-data").filter(|s| !s.is_empty())
}

/// Keep the advertised credential truthful for the volume about to be mounted.
///
/// A checkout restores the instance's OWN snapshot — its credentials Secret
/// already matches the volume's auth state, so it is left untouched. A clone
/// seed restores the SOURCE instance's snapshot, whose baked-in auth state
/// answers to the source's deploy-time password — the target must adopt the
/// source's credentials Secret before the StatefulSet is recreated.
///
/// Never derive credentials from the (already torn down) StatefulSet's pod
/// env here: in the clone-seed flow it holds the clone's dead freshly
/// generated password, and after a pre-fix checkout it holds the provider
/// default — both would re-introduce the stale-credential bug.
async fn adopt_credentials_for_restored_volume(
    storage: &KubernetesStorage,
    compute: &KubernetesCompute,
    vs_name: &str,
    target_instance: &str,
) -> Result<(), K8sCheckoutReprovisionError> {
    let source_pvc = match storage.snapshot_source_pvc(vs_name).await {
        Ok(pvc) => pvc,
        Err(e) => {
            tracing::warn!(
                "could not read source PVC of snapshot '{vs_name}': {e}; \
                 skipping credentials adoption"
            );
            return Ok(());
        }
    };
    let Some(source_instance) = source_pvc.as_deref().and_then(source_instance_from_pvc) else {
        tracing::warn!(
            "snapshot '{vs_name}' has no recognizable source PVC; skipping credentials adoption"
        );
        return Ok(());
    };
    if source_instance == target_instance {
        // Checkout of the instance's own history: Secret already truthful.
        return Ok(());
    }
    compute
        .adopt_credentials_secret(source_instance, target_instance)
        .await
        .map_err(|e| K8sCheckoutReprovisionError::Compute(e.to_string()))
}

/// Restore the pinned instance's data volume from a commit's VolumeSnapshot, then start Postgres.
pub async fn restore_database_volume_from_snapshot<R: DatabaseProviderRegistry>(
    storage: &KubernetesStorage,
    compute: &KubernetesCompute,
    registry: Arc<R>,
    repository: Arc<dyn Repository>,
    repo_path: &Path,
    snapshot_hash: &str,
) -> Result<(), K8sCheckoutReprovisionError> {
    let cfg = GfsConfig::load(repo_path)
        .map_err(|e| K8sCheckoutReprovisionError::Config(e.to_string()))?;

    let stable_instance = cfg
        .runtime
        .as_ref()
        .map(|r| r.container_name.clone())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            K8sCheckoutReprovisionError::NotConfigured("runtime.container_name missing".into())
        })?;

    let data_pvc = stable_data_pvc(&stable_instance);
    let vs_name = format!("gfs-snap-{}", &snapshot_hash[..32.min(snapshot_hash.len())]);

    let legacy_pvcs: Vec<String> = cfg
        .mount_point
        .as_ref()
        .map(|mp| mp.trim().to_string())
        .filter(|mp| !mp.is_empty() && mp.as_str() != data_pvc.as_str())
        .into_iter()
        .collect();

    let instance_id = InstanceId(stable_instance.clone());
    compute
        .remove_instance_with_pvcs(&instance_id, &legacy_pvcs)
        .await
        .map_err(|e| K8sCheckoutReprovisionError::Compute(e.to_string()))?;

    storage
        .delete_pvc(&data_pvc)
        .await
        .map_err(|e| K8sCheckoutReprovisionError::Storage(e.to_string()))?;
    for legacy in &legacy_pvcs {
        let _ = storage.delete_pvc(legacy).await;
    }

    storage
        .wait_snapshot_ready(&vs_name)
        .await
        .map_err(|e| K8sCheckoutReprovisionError::Storage(e.to_string()))?;

    adopt_credentials_for_restored_volume(storage, compute, &vs_name, &stable_instance).await?;

    StoragePort::clone(
        storage,
        &VolumeId("unused".into()),
        VolumeId(data_pvc.clone()),
        CloneOptions {
            from_snapshot: Some(SnapshotId(vs_name)),
        },
    )
    .await
    .map_err(|e| K8sCheckoutReprovisionError::Storage(e.to_string()))?;

    // PVC may stay Pending until a pod consumes it (WaitForFirstConsumer).
    reprovision_after_pvc_restore(compute, registry, repository, repo_path, data_pvc).await
}

/// Rebind the workspace PVC and recreate the StatefulSet/Service with the same instance name and NodePort.
pub async fn reprovision_after_pvc_restore<R: DatabaseProviderRegistry>(
    compute: &KubernetesCompute,
    registry: Arc<R>,
    repository: Arc<dyn Repository>,
    repo_path: &Path,
    data_pvc: String,
) -> Result<(), K8sCheckoutReprovisionError> {
    let cfg = GfsConfig::load(repo_path)
        .map_err(|e| K8sCheckoutReprovisionError::Config(e.to_string()))?;

    let stable_instance = cfg
        .runtime
        .as_ref()
        .map(|r| r.container_name.clone())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            K8sCheckoutReprovisionError::NotConfigured("runtime.container_name missing".into())
        })?;

    let expected_pvc = stable_data_pvc(&stable_instance);
    if data_pvc != expected_pvc {
        return Err(K8sCheckoutReprovisionError::NotConfigured(format!(
            "checkout PVC must be {expected_pvc}, got {data_pvc}"
        )));
    }

    let provider_name = cfg
        .environment
        .as_ref()
        .map(|e| e.database_provider.clone())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            K8sCheckoutReprovisionError::NotConfigured("database provider missing".into())
        })?;

    let database_port = cfg.environment.as_ref().and_then(|e| e.database_port);
    let database_version = cfg
        .environment
        .as_ref()
        .map(|e| e.database_version.clone())
        .unwrap_or_else(|| "17".to_string());

    let provider = registry
        .get(&provider_name)
        .ok_or_else(|| K8sCheckoutReprovisionError::UnknownProvider(provider_name.clone()))?;

    let mut def = provider.definition();
    let base = def.image.split(':').next().unwrap_or(&def.image);
    def.image = format!("{base}:{database_version}");
    // PVC already exists from VolumeSnapshot restore; mount default `{instance}-data`.
    def.host_data_dir = None;

    let instance_id = InstanceId(stable_instance.clone());
    // SS/svc already torn down in restore_database_volume_from_snapshot; keep cloned PVC.

    compute
        .provision_pinned(&def, &stable_instance, database_port)
        .await
        .map_err(|e| K8sCheckoutReprovisionError::Compute(e.to_string()))?;

    compute
        .start(&instance_id, Default::default())
        .await
        .map_err(|e| K8sCheckoutReprovisionError::Compute(e.to_string()))?;

    let runtime =
        compute
            .describe_runtime()
            .await
            .unwrap_or(gfs_domain::ports::compute::RuntimeDescriptor {
                provider: "kubernetes".into(),
                version: "unknown".into(),
            });

    repository
        .update_runtime_config(
            repo_path,
            RuntimeConfig {
                runtime_provider: runtime.provider,
                runtime_version: runtime.version,
                container_name: stable_instance,
            },
        )
        .await
        .map_err(|e| K8sCheckoutReprovisionError::Repository(e.to_string()))?;

    let conn = compute
        .get_connection_info(&instance_id, provider.default_port())
        .await
        .map_err(|e| K8sCheckoutReprovisionError::Compute(e.to_string()))?;

    if let Ok(mut updated) = GfsConfig::load(repo_path) {
        updated.mount_point = Some(data_pvc);
        if let Some(env) = updated.environment.as_mut() {
            env.database_port = Some(conn.port);
        } else {
            updated.environment = Some(EnvironmentConfig {
                database_provider: provider_name,
                database_version,
                database_port: Some(conn.port),
            });
        }
        let _ = updated.save(repo_path);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_instance_round_trips_stable_data_pvc() {
        let instance = "gfs-pg-1780839025190";
        assert_eq!(
            source_instance_from_pvc(&stable_data_pvc(instance)),
            Some(instance)
        );
    }

    #[test]
    fn source_instance_rejects_non_data_pvcs() {
        assert_eq!(source_instance_from_pvc("gfs-pg-1"), None);
        assert_eq!(source_instance_from_pvc("-data"), None);
        assert_eq!(source_instance_from_pvc(""), None);
    }
}
