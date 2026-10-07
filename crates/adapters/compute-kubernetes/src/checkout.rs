//! k3s-only: reprovision Postgres after GFS checkout (PVC restore + stable NodePort).

use std::path::Path;
use std::sync::Arc;

use gfs_domain::model::config::{EnvironmentConfig, GfsConfig, RepoCredentials, RuntimeConfig};
use gfs_domain::ports::compute::{Compute, ComputeDefinition, EnvVar, InstanceId};
use gfs_domain::ports::database_provider::{ContainerProvider, DatabaseProviderRegistry};
use gfs_domain::ports::repository::Repository;
use gfs_domain::ports::storage::{CloneOptions, SnapshotId, StoragePort, VolumeId};
use gfs_storage_kubernetes::{KubernetesStorage, volumesnapshot_name_for_hash};

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

/// The cluster operations a restore performs before it can clone, behind a
/// trait so their ORDER can be asserted with a recording fake and no cluster.
///
/// [`ClusterSteps`] is the only production implementation; it delegates to
/// `KubernetesStorage` and `KubernetesCompute`. The seam exists because the
/// order is the whole fix (see [`verify_snapshot_then_destroy_instance`]) and
/// a fake is the only way to observe the order without a Kubernetes cluster.
/// Errors carry the adapter's message so the caller chooses the variant.
trait RestoreSteps {
    /// Errors if the VolumeSnapshot is missing, never became ready, or is being
    /// deleted. Must change nothing.
    async fn wait_snapshot_ready(&self, vs_name: &str) -> Result<(), String>;

    /// Deletes the StatefulSet and Service and ISSUES the delete of the data
    /// PVC and of `legacy_pvcs`. Destructive: the live volume is gone once the
    /// delete it issues completes, and the StorageClass reclaims on Delete.
    async fn teardown_instance_keep_snapshots(
        &self,
        id: &InstanceId,
        legacy_pvcs: &[String],
    ) -> Result<(), String>;

    /// Deletes a PVC and waits for it to drain. Destructive.
    async fn delete_pvc(&self, pvc: &str) -> Result<(), String>;
}

/// The real adapters, borrowed for one restore.
struct ClusterSteps<'a> {
    storage: &'a KubernetesStorage,
    compute: &'a KubernetesCompute,
}

impl RestoreSteps for ClusterSteps<'_> {
    async fn wait_snapshot_ready(&self, vs_name: &str) -> Result<(), String> {
        self.storage
            .wait_snapshot_ready(vs_name)
            .await
            .map_err(|e| e.to_string())
    }

    async fn teardown_instance_keep_snapshots(
        &self,
        id: &InstanceId,
        legacy_pvcs: &[String],
    ) -> Result<(), String> {
        self.compute
            .teardown_instance_keep_snapshots(id, legacy_pvcs)
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_pvc(&self, pvc: &str) -> Result<(), String> {
        self.storage
            .delete_pvc(pvc)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Confirm the snapshot, THEN destroy. The order is the fix.
///
/// Everything after the first step is irreversible: the teardown issues the
/// delete of the data PVC, and the StorageClass reclaim policy is Delete, so
/// the volume is destroyed rather than released. This wait used to sit after
/// the teardown and the PVC delete, so a snapshot that was missing or never
/// became ready cost the user their StatefulSet and their PVC before anyone
/// checked. Now an unusable snapshot is a refusal that changes nothing.
async fn verify_snapshot_then_destroy_instance<S: RestoreSteps>(
    steps: &S,
    vs_name: &str,
    instance_id: &InstanceId,
    data_pvc: &str,
    legacy_pvcs: &[String],
) -> Result<(), K8sCheckoutReprovisionError> {
    steps
        .wait_snapshot_ready(vs_name)
        .await
        .map_err(K8sCheckoutReprovisionError::Storage)?;

    // RESTORE teardown: must PRESERVE the VolumeSnapshots — we delete the data PVC
    // below and then clone it back FROM `vs_name`. Using the destroy teardown
    // (`remove_instance_with_pvcs`) here would reclaim that snapshot (SEV1 data loss).
    steps
        .teardown_instance_keep_snapshots(instance_id, legacy_pvcs)
        .await
        .map_err(K8sCheckoutReprovisionError::Compute)?;

    // Past this point the instance is gone. A PVC that will not delete leaves the
    // repository with no database and no way forward — every retry repeats the
    // teardown and fails here again. Observed: a lingering
    // `snapshot.storage.kubernetes.io/pvc-as-source-protection` finalizer, held
    // while VolumeSnapshots reference the PVC as their source, wedged a
    // repository permanently. So say what happened and how to get out of it,
    // rather than reporting a bare "still exists".
    if let Err(e) = steps.delete_pvc(data_pvc).await {
        return Err(K8sCheckoutReprovisionError::Storage(format!(
            "{e}\n  The instance has already been torn down, so this repository now has no \
             database.\n  A PVC usually refuses to delete because a VolumeSnapshot still \
             names it as source — check:\n    kubectl get pvc -n gfs {data_pvc} \
             -o jsonpath='{{.metadata.finalizers}}'\n    kubectl get volumesnapshot -n gfs \
             -o custom-columns=N:.metadata.name,READY:.status.readyToUse,\
             SRC:.spec.source.persistentVolumeClaimName\n  Deleting the snapshots that name it \
             releases the finalizer and the PVC drains."
        )));
    }
    for legacy in legacy_pvcs {
        let _ = steps.delete_pvc(legacy).await;
    }
    Ok(())
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
    // Named by the function that created the snapshot at commit time, so the
    // check below and the clone after it cannot disagree about which one.
    let vs_name = volumesnapshot_name_for_hash(snapshot_hash);

    let legacy_pvcs: Vec<String> = cfg
        .mount_point
        .as_ref()
        .map(|mp| mp.trim().to_string())
        .filter(|mp| !mp.is_empty() && mp.as_str() != data_pvc.as_str())
        .into_iter()
        .collect();

    let instance_id = InstanceId(stable_instance.clone());

    verify_snapshot_then_destroy_instance(
        &ClusterSteps { storage, compute },
        &vs_name,
        &instance_id,
        &data_pvc,
        &legacy_pvcs,
    )
    .await?;

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

/// Re-apply the repo's configured database name and user onto a provider-default
/// env set. A k8s checkout rebuilds the pod from `provider.definition()` (which
/// defaults to `POSTGRES_DB=postgres` / `POSTGRES_USER=postgres`); the credentials
/// Secret carries only the *password*, so without this both the DB name and the
/// user silently revert to `postgres` after every checkout — which makes `gfs
/// status`/`gfs query` advertise the wrong user (connections then fail with
/// `role "postgres" does not exist`) and target the wrong database.
fn apply_repo_credentials_to_env(env: &mut [EnvVar], creds: &RepoCredentials) {
    if let Some(db) = creds
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        for e in env.iter_mut() {
            if e.name.contains("DB") || e.name.contains("DATABASE") {
                e.default = Some(db.to_string());
            }
        }
    }
    if let Some(user) = creds
        .user
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        for e in env.iter_mut() {
            if e.name.contains("USER") {
                e.default = Some(user.to_string());
            }
        }
    }
}

/// The database version a repository is pinned to, or the historical default.
///
/// Shared so the definition and the config write-back at the end of a
/// reprovision cannot disagree about which version was just deployed.
fn configured_database_version(cfg: &GfsConfig) -> String {
    cfg.environment
        .as_ref()
        .map(|e| e.database_version.clone())
        .unwrap_or_else(|| "17".to_string())
}

/// The [`ComputeDefinition`] a checkout rebuilds the pod from.
///
/// Pure: the config and credentials are loaded by the caller and passed in, so
/// what a checkout would deploy can be asserted without a cluster, a repository
/// on disk, or a provider registry. Same reason [`apply_repo_credentials_to_env`]
/// is a free function.
///
/// Built through `definition_with_overrides`, never the bare `definition()`.
/// `[compute.params]` is persisted in `.gfs/config.toml` precisely so a rebuild
/// can re-apply it, and this path used to skip it — so a branch switch reverted
/// a tuned database to the provider's defaults, silently.
fn checkout_definition(
    container: &dyn ContainerProvider,
    cfg: &GfsConfig,
    creds: &RepoCredentials,
) -> ComputeDefinition {
    let mut def = container.definition_with_overrides(&cfg.compute_params());
    let base = def.image.split(':').next().unwrap_or(&def.image);
    def.image = format!("{base}:{}", configured_database_version(cfg));
    // Re-apply the repo's configured database name AND user (see
    // apply_repo_credentials_to_env): a checkout rebuilds the pod from the provider
    // default (POSTGRES_DB=postgres, POSTGRES_USER=postgres), and the credentials
    // Secret carries only the password — so without this a repo created with a custom
    // --database-name/--database-user reverts to `postgres` after every checkout,
    // breaking `gfs query` with `role "postgres" does not exist`.
    apply_repo_credentials_to_env(&mut def.env, creds);
    // Re-apply the recorded resource spec rather than re-deriving one. The
    // repository is the authoritative copy; a checkout that rebuilt
    // the pod from the provider default would drop the limits exactly the way
    // it used to drop the tuning parameters.
    def.resources = cfg.compute_resources();
    // Same reasoning for the discovery labels. `gfs.role` and `gfs.remote` are
    // known only to whoever created the repository, so unlike the provider and
    // version they cannot be recomputed here -- the repository's record is the
    // only copy. Merged under the provider's own labels so a provider that
    // starts emitting one keeps it.
    let recorded = cfg.compute_labels();
    if !recorded.is_empty() {
        let mut labels = recorded;
        labels.extend(std::mem::take(&mut def.labels));
        def.labels = labels;
    }
    // PVC already exists from VolumeSnapshot restore; mount default `{instance}-data`.
    def.host_data_dir = None;
    def
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
    let database_version = configured_database_version(&cfg);

    let provider = registry
        .get(&provider_name)
        .ok_or_else(|| K8sCheckoutReprovisionError::UnknownProvider(provider_name.clone()))?;

    // Rebuilding a pod is definitionally a container operation, so this path
    // requires the container half rather than assuming every provider has one.
    let container = provider.require_container().map_err(|e| {
        K8sCheckoutReprovisionError::UnknownProvider(format!("{provider_name}: {e}"))
    })?;

    let creds = RepoCredentials::load(repo_path);
    let def = checkout_definition(container, &cfg, &creds);

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
        .get_connection_info(&instance_id, container.default_port())
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
                display_name: None,
            });
        }
        let _ = updated.save(repo_path);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use gfs_domain::model::config::{ComputeConfig, EnvironmentConfig};
    use gfs_domain::ports::compute::{ComputeDefinition, ComputeResources};
    use gfs_domain::ports::database_provider::{
        ConnectionParams, DatabaseProvider, DatabaseProviderArg, ProviderError, SupportedFeature,
    };

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

    fn env(pairs: &[(&str, &str)]) -> Vec<EnvVar> {
        pairs
            .iter()
            .map(|(n, v)| EnvVar {
                name: n.to_string(),
                default: Some(v.to_string()),
            })
            .collect()
    }

    fn creds(user: Option<&str>, name: Option<&str>) -> RepoCredentials {
        RepoCredentials {
            user: user.map(str::to_string),
            password: Some("pw".to_string()),
            name: name.map(str::to_string),
        }
    }

    #[test]
    fn reapply_credentials_restores_custom_user_and_db() {
        // Provider defaults, as a fresh checkout reprovision rebuilds them.
        let mut e = env(&[
            ("POSTGRES_USER", "postgres"),
            ("POSTGRES_DB", "postgres"),
            ("POSTGRES_PASSWORD", "postgres"),
        ]);
        apply_repo_credentials_to_env(&mut e, &creds(Some("k8suser"), Some("shopdb")));
        let get = |n: &str| e.iter().find(|v| v.name == n).unwrap().default.as_deref();
        // Regression guard: the custom user used to revert to `postgres`, making
        // `gfs query` fail with `role "postgres" does not exist`.
        assert_eq!(get("POSTGRES_USER"), Some("k8suser"));
        assert_eq!(get("POSTGRES_DB"), Some("shopdb"));
        // Password env is left untouched (routed through the credentials Secret).
        assert_eq!(get("POSTGRES_PASSWORD"), Some("postgres"));
    }

    #[test]
    fn reapply_credentials_skips_empty_or_missing() {
        let mut e = env(&[("POSTGRES_USER", "postgres"), ("POSTGRES_DB", "postgres")]);
        // None and whitespace-only are treated as "not configured" → defaults kept.
        apply_repo_credentials_to_env(&mut e, &creds(None, Some("   ")));
        let get = |n: &str| e.iter().find(|v| v.name == n).unwrap().default.as_deref();
        assert_eq!(get("POSTGRES_USER"), Some("postgres"));
        assert_eq!(get("POSTGRES_DB"), Some("postgres"));
    }

    /// Stands in for a real provider so the definition a checkout builds can be
    /// asserted without a cluster. Deliberately a stub and not
    /// `gfs-compute-docker`'s PostgreSQL provider: an adapter must not depend on
    /// another adapter. `render_param_overrides` is byte-identical to the real
    /// one (`compute-docker/src/containers/postgresql.rs:286`).
    struct StubProvider;

    impl DatabaseProvider for StubProvider {
        fn name(&self) -> &str {
            "postgres"
        }

        fn connection_string(
            &self,
            _: &ConnectionParams,
        ) -> std::result::Result<String, ProviderError> {
            Ok("postgres://localhost".into())
        }

        fn supported_versions(&self) -> Vec<String> {
            vec!["17".into()]
        }

        fn supported_features(&self) -> Vec<SupportedFeature> {
            vec![]
        }

        fn query_client_command(
            &self,
            _: &ConnectionParams,
            _: Option<&str>,
        ) -> std::result::Result<std::process::Command, ProviderError> {
            Ok(std::process::Command::new("true"))
        }

        fn container(&self) -> Option<&dyn ContainerProvider> {
            Some(self)
        }
    }

    impl ContainerProvider for StubProvider {
        fn prepare_for_snapshot(
            &self,
            _: &ConnectionParams,
        ) -> gfs_domain::ports::database_provider::Result<Vec<String>> {
            Ok(vec![])
        }

        fn definition(&self) -> ComputeDefinition {
            ComputeDefinition {
                resources: None,
                image: "postgres:17".into(),
                env: vec![EnvVar {
                    name: "POSTGRES_USER".into(),
                    default: Some("postgres".into()),
                }],
                ports: vec![],
                data_dir: PathBuf::from("/var/lib/postgresql/data"),
                host_data_dir: None,
                user: None,
                logs_dir: None,
                conf_dir: None,
                args: vec!["-c".into(), "listen_addresses=*".into()],
                labels: Default::default(),
            }
        }

        fn default_port(&self) -> u16 {
            5432
        }

        fn default_args(&self) -> Vec<DatabaseProviderArg> {
            vec![]
        }

        fn render_param_overrides(
            &self,
            params: &BTreeMap<String, String>,
        ) -> Vec<DatabaseProviderArg> {
            params
                .iter()
                .map(|(k, v)| DatabaseProviderArg {
                    name: "-c".into(),
                    value: format!("{k}={v}"),
                })
                .collect()
        }
    }

    fn cfg_with_params(params: &[(&str, &str)]) -> GfsConfig {
        GfsConfig {
            mount_point: None,
            version: String::new(),
            description: String::new(),
            user: None,
            environment: Some(EnvironmentConfig {
                database_provider: "postgres".into(),
                database_version: "17".into(),
                database_port: None,
                display_name: None,
            }),
            runtime: None,
            storage: None,
            compute: Some(ComputeConfig {
                params: params
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                resources: None,
                labels: Default::default(),
            }),
            deleted_branch_retention_days: None,
        }
    }

    fn has_arg_pair(args: &[String], name: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == name && w[1] == value)
    }

    /// The discovery labels are stamped once, at init. A checkout rebuilds the
    /// pod from the provider's bare definition, so without re-applying them the
    /// database loses `gfs.managed`, `gfs.role` and the rest on its first branch
    /// switch — the same shape as the tuning-parameter drop, one field over.
    #[test]
    fn checkout_definition_carries_the_repositorys_discovery_labels() {
        let mut cfg = cfg_with_params(&[]);
        cfg.compute.as_mut().unwrap().labels = std::collections::BTreeMap::from([
            ("gfs.managed".to_string(), "true".to_string()),
            ("gfs.role".to_string(), "clone".to_string()),
            ("gfs.remote".to_string(), "src.example:5432".to_string()),
        ]);

        let def = checkout_definition(&StubProvider, &cfg, &creds(None, None));

        assert_eq!(
            def.labels.get("gfs.managed").map(String::as_str),
            Some("true")
        );
        // `clone`, not the `source` default: a value only the caller knew, which
        // reconstruction from config could never have recovered.
        assert_eq!(
            def.labels.get("gfs.role").map(String::as_str),
            Some("clone")
        );
        assert_eq!(
            def.labels.get("gfs.remote").map(String::as_str),
            Some("src.example:5432")
        );
    }

    #[test]
    fn checkout_definition_carries_the_repositorys_tuning_parameters() {
        // A checkout rebuilds the pod from scratch, and `[compute.params]` is
        // persisted in .gfs/config.toml precisely so every rebuild can re-apply
        // it. Every other provisioning site reads it back through
        // `definition_with_overrides` — init_repo_usecase.rs:141,
        // checkout_repo_usecase.rs:300, cmd_compute.rs:456, mcp/tools.rs:1156.
        // This path did not, so a branch switch quietly reverted a tuned
        // database to the provider's defaults with nothing reporting it.
        let cfg = cfg_with_params(&[("max_connections", "200")]);

        let def = checkout_definition(&StubProvider, &cfg, &creds(None, None));

        assert!(
            has_arg_pair(&def.args, "-c", "max_connections=200"),
            "the repository's tuning parameters were dropped; args were {:?}",
            def.args
        );
        // The override is appended after the defaults, not instead of them —
        // for engines where the last occurrence wins, that ordering is what
        // makes it an override rather than the only setting.
        assert!(
            has_arg_pair(&def.args, "-c", "listen_addresses=*"),
            "the provider's own defaults were lost; args were {:?}",
            def.args
        );
    }

    #[test]
    fn checkout_reapplies_the_recorded_resource_spec() {
        // A rebuild must re-read the repository's record
        // rather than re-derive from the provider default — the same failure
        // mode that lost the tuning parameters, one field over.
        let mut cfg = cfg_with_params(&[]);
        cfg.compute.as_mut().unwrap().resources = Some(ComputeResources {
            cpu_millicores: 625,
            memory_mb: 1024,
        });

        let def = checkout_definition(&StubProvider, &cfg, &creds(None, None));

        let applied = def.resources.expect("the recorded spec was dropped");
        assert_eq!(applied.cpu_millicores, 625);
        assert_eq!(applied.memory_mb, 1024);
    }

    #[test]
    fn checkout_attaches_no_spec_when_none_was_recorded() {
        // A database provisioned before enforcement existed keeps rebuilding
        // unconstrained, rather than acquiring an invented limit.
        let def = checkout_definition(&StubProvider, &cfg_with_params(&[]), &creds(None, None));
        assert!(def.resources.is_none());
    }

    #[test]
    fn checkout_definition_adds_nothing_when_no_parameters_are_configured() {
        // Guards the other direction: a repository that never tuned anything
        // must still deploy exactly what the provider specifies.
        let cfg = cfg_with_params(&[]);

        let def = checkout_definition(&StubProvider, &cfg, &creds(None, None));

        assert_eq!(def.args, StubProvider.definition().args);
    }

    /// Records every cluster operation in the order it is asked for, so the
    /// ORDER of the restore's destructive steps can be asserted without a
    /// cluster. The real adapters cannot be driven here — they need a kube
    /// client — and the point under test is that nothing destructive reaches
    /// them before the snapshot has been confirmed.
    struct RecordingSteps {
        snapshot_ready: bool,
        pvc_deletes: bool,
        calls: RefCell<Vec<String>>,
    }

    impl RecordingSteps {
        fn new(snapshot_ready: bool) -> Self {
            Self {
                snapshot_ready,
                pvc_deletes: true,
                calls: RefCell::new(Vec::new()),
            }
        }

        fn record(&self, call: String) {
            self.calls.borrow_mut().push(call);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl RestoreSteps for RecordingSteps {
        async fn wait_snapshot_ready(&self, vs_name: &str) -> Result<(), String> {
            self.record(format!("wait_snapshot_ready({vs_name})"));
            if self.snapshot_ready {
                Ok(())
            } else {
                Err(format!(
                    "get volumesnapshot failed: \"{vs_name}\" not found"
                ))
            }
        }

        async fn teardown_instance_keep_snapshots(
            &self,
            id: &InstanceId,
            _legacy_pvcs: &[String],
        ) -> Result<(), String> {
            self.record(format!("teardown_instance_keep_snapshots({})", id.0));
            Ok(())
        }

        async fn delete_pvc(&self, pvc: &str) -> Result<(), String> {
            self.record(format!("delete_pvc({pvc})"));
            if self.pvc_deletes {
                Ok(())
            } else {
                Err(format!("pvc '{pvc}' still exists after delete"))
            }
        }
    }

    const VS: &str = "gfs-snap-aade0f36aade0f36aade0f36aade0f36";

    async fn run_prelude(steps: &RecordingSteps) -> Result<(), K8sCheckoutReprovisionError> {
        verify_snapshot_then_destroy_instance(
            steps,
            VS,
            &InstanceId("gfs-pg-1".into()),
            "gfs-pg-1-data",
            &["gfs-pg-legacy".to_string()],
        )
        .await
    }

    fn position(calls: &[String], prefix: &str) -> Option<usize> {
        calls.iter().position(|c| c.starts_with(prefix))
    }

    #[tokio::test]
    async fn the_snapshot_is_confirmed_before_anything_is_destroyed() {
        // The order IS the fix. It used to be teardown -> delete PVC -> wait,
        // so a snapshot that was missing or never became ready was discovered
        // only after the live volume was gone — and the StorageClass reclaim
        // policy is Delete, so gone meant destroyed, not released.
        let steps = RecordingSteps::new(true);
        run_prelude(&steps)
            .await
            .expect("a ready snapshot restores");
        let calls = steps.calls();

        let wait = position(&calls, "wait_snapshot_ready").expect("the snapshot was never checked");
        let teardown = position(&calls, "teardown_instance_keep_snapshots")
            .expect("the instance was never torn down");
        let first_delete = position(&calls, "delete_pvc").expect("no PVC was deleted");
        assert!(
            wait < teardown,
            "the snapshot was checked only AFTER the instance was torn down; calls were {calls:?}"
        );
        assert!(
            wait < first_delete,
            "the snapshot was checked only AFTER a PVC delete; calls were {calls:?}"
        );
        assert_eq!(
            calls,
            [
                format!("wait_snapshot_ready({VS})"),
                "teardown_instance_keep_snapshots(gfs-pg-1)".to_string(),
                "delete_pvc(gfs-pg-1-data)".to_string(),
                "delete_pvc(gfs-pg-legacy)".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn an_unusable_snapshot_destroys_nothing() {
        let steps = RecordingSteps::new(false);
        let err = run_prelude(&steps)
            .await
            .expect_err("an unusable snapshot must abort the restore");
        assert!(
            matches!(err, K8sCheckoutReprovisionError::Storage(_)),
            "{err}"
        );
        let calls = steps.calls();

        assert!(
            position(&calls, "teardown_instance_keep_snapshots").is_none(),
            "the instance was torn down although the snapshot was unusable; calls were {calls:?}"
        );
        assert!(
            position(&calls, "delete_pvc").is_none(),
            "a PVC was deleted although the snapshot was unusable; calls were {calls:?}"
        );
        assert_eq!(calls, [format!("wait_snapshot_ready({VS})")]);
    }

    #[tokio::test]
    async fn a_pvc_that_will_not_delete_says_the_instance_is_gone_and_how_to_free_it() {
        // Past the teardown there is no database and every retry fails the
        // same way, so the error must say so and point at the finalizer that
        // usually holds the PVC, rather than report a bare "still exists".
        let mut steps = RecordingSteps::new(true);
        steps.pvc_deletes = false;
        let err = run_prelude(&steps)
            .await
            .expect_err("a PVC that will not drain fails the restore");
        let text = err.to_string();
        for needed in [
            "pvc 'gfs-pg-1-data' still exists after delete",
            "already been torn down",
            "kubectl get pvc -n gfs gfs-pg-1-data -o jsonpath='{.metadata.finalizers}'",
        ] {
            assert!(text.contains(needed), "error lacks {needed:?}: {text}");
        }
    }
}
