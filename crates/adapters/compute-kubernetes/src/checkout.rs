//! k3s-only: put a branch's data volume in front of Postgres after a GFS checkout.
//!
//! A checkout here does NOT destroy anything. Each branch owns its own PVC, so
//! switching branches means recreating the StatefulSet against a different
//! `claimName` — the outgoing branch's volume is left exactly as it was,
//! including work that has not been committed yet.
//!
//! This replaced a delete-and-re-clone: the single PVC `{instance}-data` was
//! deleted and cloned back from the target commit's VolumeSnapshot. That threw
//! away everything since the last commit, gave a branch no live state to return
//! to, and made every checkout depend on a PVC delete draining — which a
//! `snapshot.storage.kubernetes.io/pvc-as-source-protection` finalizer can block
//! indefinitely, leaving the repository with no database and no way forward.
//!
//! Two paths now:
//!
//! - **Rebind** — the branch already has a volume. No snapshot, no clone, no
//!   delete; just point the StatefulSet at it. This is the common case and the
//!   one that preserves uncommitted work.
//! - **Seed** — the branch has no volume yet, so clone one from the commit's
//!   VolumeSnapshot. Still deletes nothing.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use gfs_domain::model::config::{EnvironmentConfig, GfsConfig, RepoCredentials, RuntimeConfig};
use gfs_domain::ports::compute::{Compute, ComputeDefinition, EnvVar, InstanceId};
use gfs_domain::ports::database_provider::{ContainerProvider, DatabaseProviderRegistry};
use gfs_domain::ports::repository::Repository;
use gfs_domain::ports::storage::{CloneOptions, SnapshotId, VolumeId};
use gfs_domain::repo_utils::branch_volumes::{self, BranchVolumes};
use gfs_storage_kubernetes::KubernetesStorage;

use crate::branch_volume::{branch_data_pvc, mount_existing_pvc, pvc_belongs_to_instance};
use crate::{INSTANCE_LABEL_KEY, KubernetesCompute};

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
///
/// Still the name a fresh repository starts on. It is no longer the name of
/// *the* data volume, because each branch gets its own — see
/// [`crate::branch_volume::branch_data_pvc`]. Callers outside this crate that
/// want the volume currently in front of the database must read
/// `config.mount_point`, which is authoritative, rather than deriving it here.
pub fn stable_data_pvc(instance: &str) -> String {
    crate::branch_volume::stable_data_pvc_name(instance)
}

/// Best-effort owning instance of a data PVC, by name.
///
/// Only correct for the `{instance}-data` shape. A per-branch volume cannot be
/// parsed back — `{instance}-{slug}-{hash}-data` is ambiguous because instance
/// names contain dashes too — so callers must prefer the instance LABEL and use
/// this only as the fallback for volumes created before labels were stamped.
fn source_instance_from_pvc(pvc_name: &str) -> Option<&str> {
    pvc_name.strip_suffix("-data").filter(|s| !s.is_empty())
}

/// The instance that owns `pvc`: its instance label, falling back to its name.
async fn owning_instance_of_pvc(storage: &KubernetesStorage, pvc: &str) -> Option<String> {
    match storage.pvc_label(pvc, INSTANCE_LABEL_KEY).await {
        Ok(Some(instance)) if !instance.trim().is_empty() => return Some(instance),
        Ok(_) => {}
        Err(e) => tracing::warn!("could not read the instance label of PVC '{pvc}': {e}"),
    }
    source_instance_from_pvc(pvc).map(str::to_string)
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
    let Some(source_pvc) = source_pvc
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    else {
        tracing::warn!(
            "snapshot '{vs_name}' has no recognizable source PVC; skipping credentials adoption"
        );
        return Ok(());
    };

    // Ours, whichever branch it belongs to: checkout of the instance's own
    // history, so the Secret is already truthful. This test used to be
    // `source_instance_from_pvc(pvc) == target_instance`, which silently stopped
    // holding once an instance could own more than one volume — stripping
    // `-data` off `{instance}-feat-ab12cd34-data` yields something that is not an
    // instance name, so the instance's OWN snapshot read as a foreign one and
    // adoption was attempted against a Secret that does not exist.
    if pvc_belongs_to_instance(source_pvc, target_instance) {
        return Ok(());
    }

    let Some(source_instance) = owning_instance_of_pvc(storage, source_pvc).await else {
        tracing::warn!(
            "could not attribute source PVC '{source_pvc}' of snapshot '{vs_name}' to an \
             instance; skipping credentials adoption"
        );
        return Ok(());
    };
    if source_instance == target_instance {
        return Ok(());
    }
    compute
        .adopt_credentials_secret(&source_instance, target_instance)
        .await
        .map_err(|e| K8sCheckoutReprovisionError::Compute(e.to_string()))
}

/// The branch whose data should be in front of the database, as a map key.
///
/// `get_current_branch` returns the commit hash on a detached HEAD, which is the
/// right key anyway: a detached checkout is pinned to one commit, so its volume
/// is keyed by that commit rather than by a branch that does not exist.
async fn current_branch_key(
    repository: &Arc<dyn Repository>,
    repo_path: &Path,
) -> Result<String, K8sCheckoutReprovisionError> {
    let branch = repository
        .get_current_branch(repo_path)
        .await
        .map_err(|e| K8sCheckoutReprovisionError::Repository(e.to_string()))?;
    let branch = branch.trim().to_string();
    if branch.is_empty() {
        return Err(K8sCheckoutReprovisionError::NotConfigured(
            "HEAD names neither a branch nor a commit".into(),
        ));
    }
    Ok(branch)
}

/// What a checkout must do to put a branch's volume in front of the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumePlan {
    /// The branch's volume is already there — point the database at it. No
    /// snapshot is read, nothing is cloned, and nothing is deleted, so this path
    /// preserves whatever the branch has not committed yet.
    Rebind(String),
    /// The branch has no volume yet — clone one from the commit's snapshot.
    Seed(String),
}

impl VolumePlan {
    /// The PVC the plan ends up mounting.
    pub fn pvc(&self) -> &str {
        match self {
            Self::Rebind(pvc) | Self::Seed(pvc) => pvc,
        }
    }
}

/// The volume a branch should be using: what the repository remembers, or a
/// fresh derived name when it remembers nothing.
///
/// A recorded volume keeps its name even when it has gone missing, so a branch
/// does not acquire a new name every time something goes wrong and leave the old
/// one behind. It also means an adopted pre-branch-aware volume keeps the name it
/// already has — a PVC cannot be renamed, so "deriving" one would silently mean
/// cloning and abandoning the original.
pub fn volume_for_branch(instance: &str, branch: &str, recorded: Option<&str>) -> String {
    recorded
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| branch_data_pvc(instance, branch))
}

/// Decide between rebinding a branch's volume and seeding it a new one.
///
/// Kept separate from the cluster calls because this is the whole behaviour
/// change and it is worth being able to test: everything else in the two paths
/// is mechanical.
pub fn plan_volume(volume: String, exists: bool) -> VolumePlan {
    if exists {
        VolumePlan::Rebind(volume)
    } else {
        VolumePlan::Seed(volume)
    }
}

/// Record that `branch`'s data now lives in `pvc`.
///
/// Non-fatal: the database is already up by the time this runs, and refusing the
/// checkout over a bookkeeping write would be worse than the consequence of
/// losing it. A missing record makes the next visit to this branch clone a fresh
/// volume from the branch tip instead of rebinding this one — the old behaviour,
/// so uncommitted work is at risk, which is why it is logged loudly.
fn record_branch_volume(repo_path: &Path, branch: &str, pvc: &str) {
    if let Err(e) = branch_volumes::update(repo_path, |volumes| volumes.set(branch, pvc)) {
        tracing::warn!(
            "could not record that branch '{branch}' uses volume '{pvc}': {e}; \
             a later checkout of this branch will clone a new volume from its last commit \
             instead of returning to this one"
        );
    }
}

/// Put the current branch's data volume in front of Postgres.
///
/// Rebinds the branch's existing volume when it has one, and otherwise clones it
/// a new volume from the commit's VolumeSnapshot. Deletes no volume in either
/// case, so the branch being left keeps its live state.
///
/// `snapshot_hash` is the snapshot of the commit now at HEAD; it is only read on
/// the seeding path.
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

    // HEAD has already moved by the time we are called, so this is the branch
    // being switched TO.
    let branch = current_branch_key(&repository, repo_path).await?;

    let recorded = BranchVolumes::load(repo_path)
        .map_err(|e| K8sCheckoutReprovisionError::Config(e.to_string()))?
        .get(&branch)
        .map(str::to_string);

    let target_pvc = volume_for_branch(&stable_instance, &branch, recorded.as_deref());

    // Refuse rather than guess: an API failure read as "absent" would send us
    // down the seeding path and clone a second volume beside the live one.
    let exists = storage
        .pvc_exists(&target_pvc)
        .await
        .map_err(|e| K8sCheckoutReprovisionError::Storage(e.to_string()))?;

    let plan = plan_volume(target_pvc.clone(), exists);
    let instance_id = InstanceId(stable_instance.clone());

    if matches!(plan, VolumePlan::Rebind(_)) {
        // REBIND. Nothing is cloned and nothing is deleted, so this path cannot
        // lose data and cannot be blocked by a PVC that will not drain.
        tracing::info!("checkout: rebinding branch '{branch}' to its volume '{target_pvc}'");
        compute
            .teardown_compute_keep_volumes(&instance_id)
            .await
            .map_err(|e| K8sCheckoutReprovisionError::Compute(e.to_string()))?;
    } else {
        // SEED. The branch has no volume yet, so clone one from the commit.
        let vs_name = format!("gfs-snap-{}", &snapshot_hash[..32.min(snapshot_hash.len())]);

        // Confirm the snapshot is usable BEFORE touching the running instance,
        // so an unusable one is a refusal that changes nothing.
        storage
            .wait_snapshot_ready(&vs_name)
            .await
            .map_err(|e| K8sCheckoutReprovisionError::Storage(e.to_string()))?;

        tracing::info!(
            "checkout: seeding branch '{branch}' volume '{target_pvc}' from snapshot '{vs_name}'"
        );

        // Compute only. The volume the outgoing branch is using stays bound to
        // its own PVC and is not touched.
        compute
            .teardown_compute_keep_volumes(&instance_id)
            .await
            .map_err(|e| K8sCheckoutReprovisionError::Compute(e.to_string()))?;

        adopt_credentials_for_restored_volume(storage, compute, &vs_name, &stable_instance).await?;

        // Stamp the owning instance on the new volume. Without it a destroy
        // cannot enumerate the instance's volumes and each branch leaks a ZFS
        // clone that the repository has no record of.
        let labels = BTreeMap::from([(INSTANCE_LABEL_KEY.to_string(), stable_instance.clone())]);
        storage
            .clone_labelled(
                VolumeId(target_pvc.clone()),
                CloneOptions {
                    from_snapshot: Some(SnapshotId(vs_name)),
                },
                &labels,
            )
            .await
            .map_err(|e| K8sCheckoutReprovisionError::Storage(e.to_string()))?;
    }

    record_branch_volume(repo_path, &branch, &target_pvc);

    // PVC may stay Pending until a pod consumes it (WaitForFirstConsumer).
    reprovision_after_pvc_restore(compute, registry, repository, repo_path, target_pvc).await
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

    // The volume must belong to THIS instance. This used to require it to be
    // exactly `{instance}-data`, which stopped being expressible once each branch
    // owns a volume — but the protection worth keeping is not the name, it is
    // that we never put another repository's data in front of this database. So
    // the check is widened to ownership, not removed.
    if !pvc_belongs_to_instance(&data_pvc, &stable_instance) {
        return Err(K8sCheckoutReprovisionError::NotConfigured(format!(
            "refusing to mount '{data_pvc}': it is not a volume of instance '{stable_instance}'"
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
    let mut def = checkout_definition(container, &cfg, &creds);
    // Name the volume explicitly rather than letting the adapter derive
    // `{instance}-data`: the branch's volume is usually NOT that name. The `pvc:`
    // form also tells `provision_with_instance` the volume already exists, so it
    // skips creating one — which is what we want, since the PVC was either
    // cloned just now or has been there since the last visit to this branch.
    //
    // This overrides the `host_data_dir = None` that `checkout_definition` sets:
    // that default derives `{instance}-data`, which is only correct while an
    // instance owns exactly one volume.
    def.host_data_dir = Some(mount_existing_pvc(&data_pvc));

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
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use gfs_domain::model::config::{ComputeConfig, EnvironmentConfig};
    use gfs_domain::ports::compute::{ComputeDefinition, ComputeResources};
    use gfs_domain::ports::database_provider::{
        ConnectionParams, DatabaseProvider, DatabaseProviderArg, ProviderError, SupportedFeature,
    };

    use super::*;

    #[test]
    fn a_branch_with_no_recorded_volume_is_seeded_under_a_derived_name() {
        let plan = plan_volume(volume_for_branch("gfs-pg-1", "feat/thing", None), false);
        assert_eq!(
            plan,
            VolumePlan::Seed(branch_data_pvc("gfs-pg-1", "feat/thing"))
        );
    }

    #[test]
    fn a_branch_whose_volume_is_present_is_rebound_not_recloned() {
        // The behaviour change this task exists for: returning to a branch must
        // not clone from its last commit, because that is what discarded
        // everything the branch had not committed.
        let plan = plan_volume(
            volume_for_branch("gfs-pg-1", "main", Some("gfs-pg-1-data")),
            true,
        );
        assert_eq!(plan, VolumePlan::Rebind("gfs-pg-1-data".to_string()));
    }

    #[test]
    fn a_recorded_volume_that_vanished_is_refilled_under_the_same_name() {
        // Not given a fresh derived name: that would leave the old name behind
        // and let a branch accumulate volumes across failures.
        let plan = plan_volume(
            volume_for_branch("gfs-pg-1", "main", Some("gfs-pg-1-data")),
            false,
        );
        assert_eq!(plan, VolumePlan::Seed("gfs-pg-1-data".to_string()));
    }

    #[test]
    fn an_adopted_legacy_volume_keeps_its_name_rather_than_being_renamed() {
        // A repository that predates per-branch volumes has its data in
        // `{instance}-data`. Once adopted, checkout must keep using that name —
        // a PVC cannot be renamed, so "deriving" a new one would silently mean
        // cloning and abandoning the original.
        let plan = plan_volume(
            volume_for_branch("gfs-pg-1", "main", Some("gfs-pg-1-data")),
            true,
        );
        assert_eq!(plan.pvc(), "gfs-pg-1-data");
        assert_ne!(plan.pvc(), branch_data_pvc("gfs-pg-1", "main"));
    }

    #[test]
    fn a_blank_record_falls_back_to_the_derived_name() {
        for recorded in [Some(""), Some("   "), None] {
            let plan = plan_volume(volume_for_branch("gfs-pg-1", "main", recorded), false);
            assert_eq!(
                plan.pvc(),
                branch_data_pvc("gfs-pg-1", "main"),
                "recorded={recorded:?} must not produce an empty PVC name"
            );
        }
    }

    #[test]
    fn every_planned_volume_passes_the_mount_guard() {
        // The guard and the planner must agree, or checkout plans a volume that
        // reprovision then refuses — leaving the database down.
        for branch in ["main", "feat/thing", "détaché", "0123456789abcdef"] {
            let plan = plan_volume(volume_for_branch("gfs-pg-1", branch, None), false);
            assert!(
                pvc_belongs_to_instance(plan.pvc(), "gfs-pg-1"),
                "planner produced {} which the guard rejects",
                plan.pvc()
            );
        }
    }

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
            }),
            deleted_branch_retention_days: None,
        }
    }

    fn has_arg_pair(args: &[String], name: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == name && w[1] == value)
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
}
