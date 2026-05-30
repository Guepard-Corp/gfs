//! Use case for switching the active branch or commit (checkout).
//!
//! Orchestrates [`Repository`], [`Compute`], and [`DatabaseProviderRegistry`]:
//! stops the repo's compute instance (if any), runs checkout, then starts or
//! recreates the instance with a mount on the new branch/commit data dir.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use crate::model::config::RuntimeConfig;
use crate::ports::compute::{
    Compute, ComputeCapabilities, ComputeDefinition, ComputeError, InstanceId, RuntimeDescriptor,
};
use crate::ports::database_provider::DatabaseProviderRegistry;
use crate::ports::repository::{Repository, RepositoryError};
use crate::repo_utils::repo_layout;
#[cfg(unix)]
use crate::utils::current_user;
use crate::utils::data_dir;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum CheckoutRepoError {
    #[error("{0}")]
    Repository(#[from] RepositoryError),

    #[error("compute: {0}")]
    Compute(#[from] ComputeError),
}

// ---------------------------------------------------------------------------
// Use case
// ---------------------------------------------------------------------------

/// Use case for checking out a branch or commit.
///
/// When the repo has a compute container configured, stops it before checkout
/// and starts (or recreates with the new workspace mount) after checkout.
/// Resolves the revision, runs checkout, and returns the full commit hash.
pub struct CheckoutRepoUseCase<R: DatabaseProviderRegistry> {
    repository: Arc<dyn Repository>,
    compute: Arc<dyn Compute>,
    registry: Arc<R>,
}

impl<R: DatabaseProviderRegistry> CheckoutRepoUseCase<R> {
    pub fn new(
        repository: Arc<dyn Repository>,
        compute: Arc<dyn Compute>,
        registry: Arc<R>,
    ) -> Self {
        Self {
            repository,
            compute,
            registry,
        }
    }

    /// Check out `revision` (branch name or full 64-char commit hash) at `path`.
    /// When `create_branch` is `Some(name)`, creates a new branch at `revision`
    /// (or current HEAD if `revision` is empty) then checks out that branch.
    /// Returns the full commit hash on success for display (e.g. short hash).
    pub async fn run(
        &self,
        path: PathBuf,
        revision: String,
        create_branch: Option<String>,
    ) -> std::result::Result<String, CheckoutRepoError> {
        let revision = revision.trim().to_string();

        let container_id = self
            .repository
            .get_runtime_config(&path)
            .await
            .ok()
            .flatten()
            .and_then(|r| {
                let name = r.container_name.trim();
                if name.is_empty() {
                    None
                } else {
                    Some(InstanceId(name.to_string()))
                }
            });

        if let Some(ref id) = container_id {
            match self.compute.stop(id).await {
                Ok(_) | Err(ComputeError::NotFound(_)) => {}
                Err(e) => return Err(CheckoutRepoError::Compute(e)),
            }
        }

        let commit_hash = self.do_checkout(&path, &revision, create_branch).await?;

        if let Some(ref id) = container_id {
            self.ensure_compute_started_after_checkout(&path, id)
                .await?;
        }

        Ok(commit_hash)
    }

    async fn do_checkout(
        &self,
        path: &Path,
        revision: &str,
        create_branch: Option<String>,
    ) -> std::result::Result<String, CheckoutRepoError> {
        if let Some(ref branch_name) = create_branch {
            let branch_name = branch_name.trim().to_string();
            if branch_name.is_empty() {
                return Err(CheckoutRepoError::Repository(
                    RepositoryError::RevisionNotFound("(empty branch name)".to_string()),
                ));
            }
            let start_rev = if revision.is_empty() {
                "HEAD".to_string()
            } else {
                revision.to_string()
            };
            let commit_hash = self.repository.rev_parse(path, &start_rev).await?;
            if commit_hash == "0" {
                return Err(CheckoutRepoError::Repository(RepositoryError::Internal(
                    "cannot create branch: start revision has no commits".to_string(),
                )));
            }
            self.repository
                .create_branch(path, &branch_name, &commit_hash)
                .await?;
            self.repository.checkout(path, &branch_name).await?;
            let out_hash = self.repository.get_current_commit_id(path).await?;
            return Ok(out_hash);
        }

        if revision.is_empty() {
            return Err(CheckoutRepoError::Repository(
                RepositoryError::RevisionNotFound("(empty)".to_string()),
            ));
        }

        self.repository.checkout(path, revision).await?;
        let commit_hash = self.repository.get_current_commit_id(path).await?;
        Ok(commit_hash)
    }

    /// Start the instance or recreate it with the current workspace data dir if the bind differs.
    async fn ensure_compute_started_after_checkout(
        &self,
        path: &Path,
        instance_id: &InstanceId,
    ) -> std::result::Result<(), CheckoutRepoError> {
        let active = self.repository.get_active_workspace_data_dir(path).await?;
        let active_str = active.to_string_lossy().into_owned();
        tracing::info!(
            "ensure_compute_started_after_checkout: active_workspace={:?}",
            active
        );

        let environment = match self.repository.get_environment_config(path).await? {
            Some(e) if !e.database_provider.is_empty() => e,
            _ => return Ok(()),
        };

        let provider = match self.registry.get(environment.database_provider.as_str()) {
            Some(p) => p,
            None => return Ok(()),
        };

        let params = crate::model::config::GfsConfig::load_compute_params(path);
        let mut definition = provider.definition_with_overrides(&params);
        if !environment.database_version.is_empty() {
            let base = definition
                .image
                .split(':')
                .next()
                .unwrap_or(definition.image.as_str());
            definition.image = format!("{}:{}", base, environment.database_version);
        }
        data_dir::prepare_for_database_provider(provider.name(), &active).map_err(|e| {
            ComputeError::Internal(format!(
                "failed to prepare data dir '{}': {e}",
                active.display()
            ))
        })?;
        definition.host_data_dir = Some(active.clone());
        #[cfg(unix)]
        {
            match current_user::current_user_uid_gid() {
                Some(uid_gid) => definition.user = Some(uid_gid),
                None => tracing::warn!(
                    "could not determine host uid:gid; container will run as its default user — \
                     workspace files may be unreadable by the host user during snapshot"
                ),
            }
        }
        let compute_data_path = definition.data_dir.to_string_lossy().into_owned();
        let repair_target = definition
            .user
            .clone()
            .or_else(|| provider.data_dir_owner().map(str::to_string));
        let startup_probes = provider.container_startup_probes();

        let current_bind = self
            .compute
            .get_instance_data_mount_host_path(instance_id, &compute_data_path)
            .await
            .ok()
            .flatten()
            .map(|p| p.to_string_lossy().into_owned());

        tracing::info!(
            "ensure_compute_started_after_checkout: current_bind={:?}, paths_differ={}",
            current_bind,
            paths_differ(&active_str, current_bind.as_deref().unwrap_or(""))
        );

        let repair_marker = repo_layout::repair_marker_path(std::path::Path::new(&active_str));
        let repair_needed = repair_marker.as_ref().map(|m| m.exists()).unwrap_or(false);

        if !paths_differ(&active_str, current_bind.as_deref().unwrap_or("")) {
            tracing::info!("ensure_compute_started_after_checkout: starting existing container");
            if repair_needed {
                self.pre_start_repair_data_dir(
                    &definition,
                    &compute_data_path,
                    repair_target.as_deref(),
                    repair_marker.as_deref(),
                )
                .await;
            }
            match self.compute.start(instance_id, Default::default()).await {
                Ok(_) => {
                    self.repair_data_dir_permissions_in_container(
                        instance_id,
                        &compute_data_path,
                        repair_target.as_deref(),
                    )
                    .await;
                    self.assert_container_healthy(instance_id, startup_probes)
                        .await?;
                    return Ok(());
                }
                Err(ComputeError::NotFound(_)) => {
                    tracing::info!(
                        "ensure_compute_started_after_checkout: container not found, recreating"
                    );
                }
                Err(e) => return Err(CheckoutRepoError::Compute(e)),
            }
        }

        tracing::info!(
            "ensure_compute_started_after_checkout: removing old container and creating new one"
        );
        match self.compute.remove_instance(instance_id).await {
            Ok(()) | Err(ComputeError::NotFound(_)) => {}
            Err(e) => return Err(CheckoutRepoError::Compute(e)),
        }
        let new_id = self.compute.provision(&definition).await?;
        // Always repair before first start of a new container — the workspace was just
        // populated from snapshot and ownership may not match the DB process user.
        self.pre_start_repair_data_dir(
            &definition,
            &compute_data_path,
            repair_target.as_deref(),
            repair_marker.as_deref(),
        )
        .await;
        let _ = self.compute.start(&new_id, Default::default()).await?;
        // Belt-and-suspenders: also repair inside the running container.
        self.repair_data_dir_permissions_in_container(
            &new_id,
            &compute_data_path,
            repair_target.as_deref(),
        )
        .await;
        self.assert_container_healthy(&new_id, startup_probes)
            .await?;
        let runtime = self
            .compute
            .describe_runtime()
            .await
            .unwrap_or_else(|_| RuntimeDescriptor {
                provider: "docker".to_string(),
                version: "24".to_string(),
            });
        self.repository
            .update_runtime_config(
                path,
                RuntimeConfig {
                    runtime_provider: runtime.provider,
                    runtime_version: runtime.version,
                    container_name: new_id.0.clone(),
                },
            )
            .await?;
        Ok(())
    }

    async fn assert_container_healthy(
        &self,
        instance_id: &InstanceId,
        startup_probes: &[&'static str],
    ) -> std::result::Result<(), CheckoutRepoError> {
        if startup_probes.is_empty() {
            return Ok(());
        }

        let caps = self
            .compute
            .capabilities()
            .await
            .unwrap_or(ComputeCapabilities {
                supports_stream_snapshot: false,
                supports_exec_as_root: false,
            });
        // Startup probes are connectivity checks (pg_isready, mysqladmin ping) that do
        // not require root. When exec-as-root is unavailable, run as the container's
        // default user so health is still verified — do not skip entirely.
        let exec_user = if caps.supports_exec_as_root {
            Some("0:0")
        } else {
            tracing::warn!(
                instance = %instance_id,
                "compute runtime cannot exec as root; running startup probes as default container user"
            );
            None
        };

        // Postgres cold-start on Windows Docker Desktop / WSL2 routinely
        // exceeds 3 s — bind-mount + initdb on the 9P layer is slow. Probe
        // for up to ~60 s so first-time checkouts after a snapshot land
        // before failing. Tests use a tiny budget to stay fast.
        #[cfg(not(test))]
        const PROBE_ATTEMPTS: u32 = 120;
        #[cfg(not(test))]
        const PROBE_SLEEP_MS: u64 = 500;
        #[cfg(test)]
        const PROBE_ATTEMPTS: u32 = 15;
        #[cfg(test)]
        const PROBE_SLEEP_MS: u64 = 10;

        for probe in startup_probes {
            let cmd = probe.trim();
            if cmd.is_empty() {
                continue;
            }
            let mut last_out = None;
            let mut ok = false;
            for attempt in 0..PROBE_ATTEMPTS {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_millis(PROBE_SLEEP_MS)).await;
                }
                let out = self
                    .compute
                    .exec(instance_id, cmd, exec_user)
                    .await
                    .map_err(CheckoutRepoError::Compute)?;
                if out.exit_code == 0 {
                    ok = true;
                    break;
                }
                last_out = Some(out);
            }
            if !ok {
                let out = last_out.unwrap();
                return Err(CheckoutRepoError::Compute(ComputeError::Internal(format!(
                    "database startup probe failed after {} attempts (exit {}): {}\nstdout: {}\nstderr: {}",
                    PROBE_ATTEMPTS,
                    out.exit_code,
                    cmd,
                    out.stdout.trim(),
                    out.stderr.trim()
                ))));
            }
        }
        Ok(())
    }

    /// Best-effort pre-start permission repair via an ephemeral container.
    ///
    /// Spins up a throwaway container (same image + data-dir bind, running as root)
    /// to `chown` + `chmod` the data directory _before_ the real container starts.
    /// This is necessary when a prior `stream_snapshot` commit left files owned by the
    /// host user rather than the database process user (e.g. `postgres:postgres`).
    ///
    /// Runs as root (`0:0`) so the operation succeeds regardless of current ownership.
    /// Best-effort: logs and continues on failure so checkout is not blocked.
    async fn pre_start_repair_data_dir(
        &self,
        definition: &ComputeDefinition,
        container_data_path: &str,
        chown_target: Option<&str>,
        marker: Option<&std::path::Path>,
    ) {
        let Some(target) = chown_target.filter(|s| !s.trim().is_empty()) else {
            return;
        };
        let escaped = container_data_path.replace('\'', "'\"'\"'");
        let cmd = format!("chown -R {target} '{escaped}' && chmod -R 0700 '{escaped}'");

        let mut repair_def = definition.clone();
        repair_def.user = Some("0:0".to_string());

        tracing::info!(
            data_dir = container_data_path,
            chown_target = target,
            "pre_start_repair_data_dir: running pre-start chown via ephemeral container"
        );

        match self.compute.run_task(&repair_def, &cmd, None).await {
            Ok(_) => {
                if let Some(m) = marker {
                    let _ = std::fs::remove_file(m);
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    data_dir = container_data_path,
                    "pre_start_repair_data_dir: repair task failed; container may fail to start"
                );
            }
        }
    }

    /// Best-effort: ensure the DB process user can read/write its data dir.
    ///
    /// This is critical when snapshots were created via `stream_snapshot` because tar extraction
    /// intentionally does not preserve container ownership/mode bits.
    async fn repair_data_dir_permissions_in_container(
        &self,
        instance_id: &InstanceId,
        container_data_path: &str,
        chown_target: Option<&str>,
    ) {
        let Some(target) = chown_target.filter(|s| !s.trim().is_empty()) else {
            return;
        };
        let escaped = container_data_path.replace('\'', "'\"'\"'");
        let cmd = format!("chown -R {target} '{escaped}' && chmod -R 0700 '{escaped}'");
        let caps = self.compute.capabilities().await.ok();
        let can_root = caps.map(|c| c.supports_exec_as_root).unwrap_or(false);
        if !can_root {
            tracing::warn!(
                instance = %instance_id,
                data_dir = container_data_path,
                "compute runtime cannot exec as root; cannot repair workspace permissions inside container"
            );
            return;
        }

        if let Err(e) = self.compute.exec(instance_id, &cmd, Some("0:0")).await {
            tracing::warn!(
                error = %e,
                instance = %instance_id,
                data_dir = container_data_path,
                "failed to repair workspace permissions inside container; continuing"
            );
        }
    }
}

fn paths_differ(active: &str, current_bind: &str) -> bool {
    let a = std::path::Path::new(active);
    let b = std::path::Path::new(current_bind);
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a != b,
        _ => active != current_bind,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::Arc;

    use async_trait::async_trait;

    use crate::model::config::{EnvironmentConfig, RuntimeConfig};
    use crate::ports::compute::{
        Compute, ComputeDefinition, InstanceId, InstanceState, InstanceStatus, StartOptions,
    };
    use crate::ports::database_provider::{
        ConnectionParams, DatabaseProvider, DatabaseProviderArg, DatabaseProviderRegistry,
        ProviderError, Result as RegistryResult, SIGTERM, SupportedFeature,
    };
    use crate::ports::repository::Repository;

    struct MockRepository {
        current_commit: String,
        runtime_config: Option<RuntimeConfig>,
    }

    #[async_trait]
    impl Repository for MockRepository {
        async fn init(
            &self,
            _: &std::path::Path,
            _: Option<String>,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn get_workspace_data_dir_for_head(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<PathBuf> {
            Ok(PathBuf::from("/workspace/data"))
        }
        async fn update_environment_config(
            &self,
            _: &std::path::Path,
            _: EnvironmentConfig,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn update_runtime_config(
            &self,
            _: &std::path::Path,
            _: RuntimeConfig,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn clone_repo(
            &self,
            _: &str,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn commit(
            &self,
            _: &std::path::Path,
            _: crate::model::commit::NewCommit,
        ) -> crate::ports::repository::Result<String> {
            Ok(String::new())
        }
        async fn checkout(
            &self,
            _: &std::path::Path,
            _: &str,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn create_branch(
            &self,
            _: &std::path::Path,
            _: &str,
            _: &str,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn log(
            &self,
            _: &std::path::Path,
            _: crate::ports::repository::LogOptions,
        ) -> crate::ports::repository::Result<Vec<crate::model::commit::CommitWithRefs>> {
            Ok(vec![])
        }
        async fn rev_parse(
            &self,
            _: &std::path::Path,
            rev: &str,
        ) -> crate::ports::repository::Result<String> {
            if rev == "0" {
                return Err(crate::ports::repository::RepositoryError::Internal(
                    "cannot create branch: start revision has no commits".into(),
                ));
            }
            Ok(self.current_commit.clone())
        }
        async fn push(
            &self,
            _: &std::path::Path,
            _: crate::ports::repository::RemoteOptions,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn pull(
            &self,
            _: &std::path::Path,
            _: crate::ports::repository::RemoteOptions,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn fetch(
            &self,
            _: &std::path::Path,
            _: crate::ports::repository::RemoteOptions,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn get_current_branch(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<String> {
            Ok("main".into())
        }
        async fn get_current_commit_id(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<String> {
            Ok(self.current_commit.clone())
        }
        async fn get_runtime_config(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<RuntimeConfig>> {
            Ok(self.runtime_config.clone())
        }
        async fn get_mount_point(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<String>> {
            Ok(None)
        }
        async fn get_environment_config(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<EnvironmentConfig>> {
            Ok(None)
        }
        async fn get_user_config(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<crate::model::config::UserConfig>> {
            Ok(None)
        }
        async fn ensure_snapshot_path(
            &self,
            _: &std::path::Path,
            _: &str,
        ) -> crate::ports::repository::Result<PathBuf> {
            Ok(PathBuf::from("/tmp/snap"))
        }
        async fn get_active_workspace_data_dir(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<PathBuf> {
            Ok(PathBuf::from("/workspace/data"))
        }
    }

    struct MockCompute;

    #[async_trait]
    impl Compute for MockCompute {
        async fn provision(
            &self,
            _: &ComputeDefinition,
        ) -> crate::ports::compute::Result<InstanceId> {
            Ok(InstanceId("mock".into()))
        }
        async fn start(
            &self,
            id: &InstanceId,
            _: StartOptions,
        ) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn stop(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Stopped,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn restart(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn status(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn prepare_for_snapshot(
            &self,
            _: &InstanceId,
            _: &[String],
        ) -> crate::ports::compute::Result<()> {
            Ok(())
        }
        async fn logs(
            &self,
            _: &InstanceId,
            _: crate::ports::compute::LogsOptions,
        ) -> crate::ports::compute::Result<Vec<crate::ports::compute::LogEntry>> {
            Ok(vec![])
        }
        async fn pause(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Paused,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn unpause(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn get_connection_info(
            &self,
            _id: &InstanceId,
            port: u16,
        ) -> crate::ports::compute::Result<crate::ports::compute::InstanceConnectionInfo> {
            Ok(crate::ports::compute::InstanceConnectionInfo {
                host: "127.0.0.1".into(),
                port,
                env: vec![],
            })
        }
        async fn get_instance_data_mount_host_path(
            &self,
            _id: &InstanceId,
            _: &str,
        ) -> crate::ports::compute::Result<Option<PathBuf>> {
            Ok(None)
        }
        async fn remove_instance(&self, _id: &InstanceId) -> crate::ports::compute::Result<()> {
            Ok(())
        }
        async fn get_task_connection_info(
            &self,
            _id: &InstanceId,
            port: u16,
        ) -> crate::ports::compute::Result<crate::ports::compute::InstanceConnectionInfo> {
            Ok(crate::ports::compute::InstanceConnectionInfo {
                host: "172.17.0.2".into(),
                port,
                env: vec![],
            })
        }
        async fn run_task(
            &self,
            _: &ComputeDefinition,
            _: &str,
            _: Option<&InstanceId>,
        ) -> crate::ports::compute::Result<crate::ports::compute::ExecOutput> {
            Ok(crate::ports::compute::ExecOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    struct MockProvider;

    impl DatabaseProvider for MockProvider {
        fn name(&self) -> &str {
            "postgres"
        }
        fn definition(&self) -> ComputeDefinition {
            ComputeDefinition {
                labels: Default::default(),
                image: "postgres:17".into(),
                env: vec![],
                ports: vec![],
                data_dir: PathBuf::from("/data"),
                host_data_dir: None,
                user: None,
                logs_dir: None,
                conf_dir: None,
                args: vec![],
            }
        }
        fn default_port(&self) -> u16 {
            5432
        }
        fn default_args(&self) -> Vec<DatabaseProviderArg> {
            vec![]
        }
        fn default_signal(&self) -> u32 {
            SIGTERM
        }
        fn connection_string(
            &self,
            _: &ConnectionParams,
        ) -> std::result::Result<String, ProviderError> {
            Ok("postgres://localhost:5432".into())
        }
        fn supported_versions(&self) -> Vec<String> {
            vec!["17".into()]
        }
        fn supported_features(&self) -> Vec<SupportedFeature> {
            vec![]
        }
        fn prepare_for_snapshot(&self, _: &ConnectionParams) -> RegistryResult<Vec<String>> {
            Ok(vec![])
        }
        fn query_client_command(
            &self,
            _: &ConnectionParams,
            _: Option<&str>,
        ) -> std::result::Result<std::process::Command, ProviderError> {
            Ok(std::process::Command::new("true"))
        }
    }

    struct MockRegistry;

    impl DatabaseProviderRegistry for MockRegistry {
        fn register(&self, _: Arc<dyn DatabaseProvider>) -> RegistryResult<()> {
            Ok(())
        }
        fn get(&self, name: &str) -> Option<Arc<dyn DatabaseProvider>> {
            if name.eq_ignore_ascii_case("postgres") {
                Some(Arc::new(MockProvider))
            } else {
                None
            }
        }
        fn list(&self) -> Vec<String> {
            vec!["postgres".into()]
        }
        fn unregister(&self, _: &str) -> Option<Arc<dyn DatabaseProvider>> {
            None
        }
    }

    #[tokio::test]
    async fn checkout_revision() {
        let repo = MockRepository {
            current_commit: "abc123".into(),
            runtime_config: None,
        };
        let usecase = CheckoutRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockCompute),
            Arc::new(MockRegistry),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(dir.path().to_path_buf(), "main".into(), None)
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "abc123");
    }

    #[tokio::test]
    async fn checkout_empty_revision() {
        let repo = MockRepository {
            current_commit: "abc123".into(),
            runtime_config: None,
        };
        let usecase = CheckoutRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockCompute),
            Arc::new(MockRegistry),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase.run(dir.path().to_path_buf(), "".into(), None).await;
        assert!(matches!(result, Err(CheckoutRepoError::Repository(_))));
    }

    #[tokio::test]
    async fn checkout_create_branch_empty_name() {
        let repo = MockRepository {
            current_commit: "abc123".into(),
            runtime_config: None,
        };
        let usecase = CheckoutRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockCompute),
            Arc::new(MockRegistry),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(dir.path().to_path_buf(), "main".into(), Some("".into()))
            .await;
        assert!(matches!(result, Err(CheckoutRepoError::Repository(_))));
    }

    #[tokio::test]
    async fn checkout_with_container_stops_and_start() {
        let repo = MockRepository {
            current_commit: "abc123".into(),
            runtime_config: Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "container-1".into(),
            }),
        };
        let usecase = CheckoutRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockCompute),
            Arc::new(MockRegistry),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(dir.path().to_path_buf(), "main".into(), None)
            .await;
        assert!(result.is_ok());
    }

    /// Simulates a container that was manually removed from Docker:
    /// `stop`, `remove_instance`, and `get_instance_data_mount_host_path` all
    /// return `NotFound`; `provision` and `start` succeed (new container).
    struct MockComputeRemoved;

    #[async_trait]
    impl Compute for MockComputeRemoved {
        async fn provision(
            &self,
            _: &ComputeDefinition,
        ) -> crate::ports::compute::Result<InstanceId> {
            Ok(InstanceId("new-container".into()))
        }
        async fn start(
            &self,
            id: &InstanceId,
            _: StartOptions,
        ) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn stop(&self, _: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Err(ComputeError::NotFound("container-1".into()))
        }
        async fn restart(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn status(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn prepare_for_snapshot(
            &self,
            _: &InstanceId,
            _: &[String],
        ) -> crate::ports::compute::Result<()> {
            Ok(())
        }
        async fn logs(
            &self,
            _: &InstanceId,
            _: crate::ports::compute::LogsOptions,
        ) -> crate::ports::compute::Result<Vec<crate::ports::compute::LogEntry>> {
            Ok(vec![])
        }
        async fn pause(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Paused,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn unpause(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn get_connection_info(
            &self,
            _id: &InstanceId,
            port: u16,
        ) -> crate::ports::compute::Result<crate::ports::compute::InstanceConnectionInfo> {
            Ok(crate::ports::compute::InstanceConnectionInfo {
                host: "127.0.0.1".into(),
                port,
                env: vec![],
            })
        }
        async fn get_instance_data_mount_host_path(
            &self,
            _id: &InstanceId,
            _: &str,
        ) -> crate::ports::compute::Result<Option<PathBuf>> {
            Err(ComputeError::NotFound("container-1".into()))
        }
        async fn remove_instance(&self, _id: &InstanceId) -> crate::ports::compute::Result<()> {
            Err(ComputeError::NotFound("container-1".into()))
        }
        async fn get_task_connection_info(
            &self,
            _id: &InstanceId,
            port: u16,
        ) -> crate::ports::compute::Result<crate::ports::compute::InstanceConnectionInfo> {
            Ok(crate::ports::compute::InstanceConnectionInfo {
                host: "172.17.0.2".into(),
                port,
                env: vec![],
            })
        }
        async fn run_task(
            &self,
            _: &ComputeDefinition,
            _: &str,
            _: Option<&InstanceId>,
        ) -> crate::ports::compute::Result<crate::ports::compute::ExecOutput> {
            Ok(crate::ports::compute::ExecOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    /// Repository variant that exposes a database environment config so the
    /// `ensure_compute_started_after_checkout` recreate path is exercised.
    struct MockRepositoryWithEnv {
        current_commit: String,
    }

    #[async_trait]
    impl Repository for MockRepositoryWithEnv {
        async fn init(
            &self,
            _: &std::path::Path,
            _: Option<String>,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn get_workspace_data_dir_for_head(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<PathBuf> {
            Ok(PathBuf::from("/workspace/data"))
        }
        async fn update_environment_config(
            &self,
            _: &std::path::Path,
            _: EnvironmentConfig,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn update_runtime_config(
            &self,
            _: &std::path::Path,
            _: RuntimeConfig,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn clone_repo(
            &self,
            _: &str,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn commit(
            &self,
            _: &std::path::Path,
            _: crate::model::commit::NewCommit,
        ) -> crate::ports::repository::Result<String> {
            Ok(String::new())
        }
        async fn checkout(
            &self,
            _: &std::path::Path,
            _: &str,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn create_branch(
            &self,
            _: &std::path::Path,
            _: &str,
            _: &str,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn log(
            &self,
            _: &std::path::Path,
            _: crate::ports::repository::LogOptions,
        ) -> crate::ports::repository::Result<Vec<crate::model::commit::CommitWithRefs>> {
            Ok(vec![])
        }
        async fn rev_parse(
            &self,
            _: &std::path::Path,
            _: &str,
        ) -> crate::ports::repository::Result<String> {
            Ok(self.current_commit.clone())
        }
        async fn push(
            &self,
            _: &std::path::Path,
            _: crate::ports::repository::RemoteOptions,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn pull(
            &self,
            _: &std::path::Path,
            _: crate::ports::repository::RemoteOptions,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn fetch(
            &self,
            _: &std::path::Path,
            _: crate::ports::repository::RemoteOptions,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn get_current_branch(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<String> {
            Ok("main".into())
        }
        async fn get_current_commit_id(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<String> {
            Ok(self.current_commit.clone())
        }
        async fn get_runtime_config(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<RuntimeConfig>> {
            Ok(Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "container-1".into(),
            }))
        }
        async fn get_mount_point(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<String>> {
            Ok(None)
        }
        async fn get_environment_config(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<EnvironmentConfig>> {
            Ok(Some(EnvironmentConfig {
                database_provider: "postgres".into(),
                database_version: "17".into(),
                database_port: None,
            }))
        }
        async fn get_user_config(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<crate::model::config::UserConfig>> {
            Ok(None)
        }
        async fn ensure_snapshot_path(
            &self,
            _: &std::path::Path,
            _: &str,
        ) -> crate::ports::repository::Result<PathBuf> {
            Ok(PathBuf::from("/tmp/snap"))
        }
        async fn get_active_workspace_data_dir(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<PathBuf> {
            Ok(PathBuf::from("/workspace/data"))
        }
    }

    /// When the container has been manually removed from Docker (stop returns NotFound)
    /// and there is no database environment configured, checkout must still succeed.
    #[tokio::test]
    async fn checkout_succeeds_when_container_removed_no_env() {
        let repo = MockRepository {
            current_commit: "abc123".into(),
            runtime_config: Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "container-1".into(),
            }),
        };
        let usecase = CheckoutRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockComputeRemoved),
            Arc::new(MockRegistry),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(dir.path().to_path_buf(), "main".into(), None)
            .await;
        assert!(
            result.is_ok(),
            "checkout should succeed even when container was removed: {result:?}"
        );
    }

    /// When the container has been manually removed from Docker and a database
    /// environment is configured, checkout must succeed and GFS must recreate
    /// the compute (provision + start) using the current workspace data dir.
    #[tokio::test]
    async fn checkout_recreates_compute_when_container_removed() {
        let repo = MockRepositoryWithEnv {
            current_commit: "abc123".into(),
        };
        let usecase = CheckoutRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockComputeRemoved),
            Arc::new(MockRegistry),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(dir.path().to_path_buf(), "main".into(), None)
            .await;
        assert!(
            result.is_ok(),
            "checkout should recreate compute when container was removed: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Probe retry tests
    // -----------------------------------------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Compute mock whose `exec` fails `exec_fail_count` times before succeeding.
    struct MockComputeWithProbeFailures {
        exec_fail_remaining: AtomicUsize,
    }

    #[async_trait]
    impl Compute for MockComputeWithProbeFailures {
        async fn capabilities(&self) -> crate::ports::compute::Result<ComputeCapabilities> {
            Ok(ComputeCapabilities {
                supports_stream_snapshot: false,
                supports_exec_as_root: true,
            })
        }
        async fn exec(
            &self,
            _id: &InstanceId,
            _command: &str,
            _user: Option<&str>,
        ) -> crate::ports::compute::Result<crate::ports::compute::ExecOutput> {
            let remaining = self.exec_fail_remaining.load(Ordering::SeqCst);
            if remaining > 0 {
                self.exec_fail_remaining.fetch_sub(1, Ordering::SeqCst);
                return Ok(crate::ports::compute::ExecOutput {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "not ready yet".into(),
                });
            }
            Ok(crate::ports::compute::ExecOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        async fn provision(
            &self,
            _: &ComputeDefinition,
        ) -> crate::ports::compute::Result<InstanceId> {
            Ok(InstanceId("mock-probe".into()))
        }
        async fn start(
            &self,
            id: &InstanceId,
            _: StartOptions,
        ) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn stop(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Stopped,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn restart(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn status(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn prepare_for_snapshot(
            &self,
            _: &InstanceId,
            _: &[String],
        ) -> crate::ports::compute::Result<()> {
            Ok(())
        }
        async fn logs(
            &self,
            _: &InstanceId,
            _: crate::ports::compute::LogsOptions,
        ) -> crate::ports::compute::Result<Vec<crate::ports::compute::LogEntry>> {
            Ok(vec![])
        }
        async fn pause(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Paused,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn unpause(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn get_connection_info(
            &self,
            _id: &InstanceId,
            port: u16,
        ) -> crate::ports::compute::Result<crate::ports::compute::InstanceConnectionInfo> {
            Ok(crate::ports::compute::InstanceConnectionInfo {
                host: "127.0.0.1".into(),
                port,
                env: vec![],
            })
        }
        async fn get_instance_data_mount_host_path(
            &self,
            _id: &InstanceId,
            _: &str,
        ) -> crate::ports::compute::Result<Option<PathBuf>> {
            Ok(None)
        }
        async fn remove_instance(&self, _id: &InstanceId) -> crate::ports::compute::Result<()> {
            Ok(())
        }
        async fn get_task_connection_info(
            &self,
            _id: &InstanceId,
            port: u16,
        ) -> crate::ports::compute::Result<crate::ports::compute::InstanceConnectionInfo> {
            Ok(crate::ports::compute::InstanceConnectionInfo {
                host: "172.17.0.2".into(),
                port,
                env: vec![],
            })
        }
        async fn run_task(
            &self,
            _: &ComputeDefinition,
            _: &str,
            _: Option<&InstanceId>,
        ) -> crate::ports::compute::Result<crate::ports::compute::ExecOutput> {
            Ok(crate::ports::compute::ExecOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    /// Provider that exposes one startup probe so `assert_container_healthy` is exercised.
    struct MockProviderWithProbe;

    impl DatabaseProvider for MockProviderWithProbe {
        fn name(&self) -> &str {
            "postgres"
        }
        fn definition(&self) -> ComputeDefinition {
            ComputeDefinition {
                labels: Default::default(),
                image: "postgres:17".into(),
                env: vec![],
                ports: vec![],
                data_dir: PathBuf::from("/data"),
                host_data_dir: None,
                user: None,
                logs_dir: None,
                conf_dir: None,
                args: vec![],
            }
        }
        fn default_port(&self) -> u16 {
            5432
        }
        fn default_args(&self) -> Vec<DatabaseProviderArg> {
            vec![]
        }
        fn default_signal(&self) -> u32 {
            SIGTERM
        }
        fn connection_string(
            &self,
            _: &ConnectionParams,
        ) -> std::result::Result<String, ProviderError> {
            Ok("postgres://localhost:5432".into())
        }
        fn supported_versions(&self) -> Vec<String> {
            vec!["17".into()]
        }
        fn supported_features(&self) -> Vec<SupportedFeature> {
            vec![]
        }
        fn prepare_for_snapshot(&self, _: &ConnectionParams) -> RegistryResult<Vec<String>> {
            Ok(vec![])
        }
        fn query_client_command(
            &self,
            _: &ConnectionParams,
            _: Option<&str>,
        ) -> std::result::Result<std::process::Command, ProviderError> {
            Ok(std::process::Command::new("true"))
        }
        fn container_startup_probes(&self) -> &'static [&'static str] {
            &["pg_isready -U postgres"]
        }
    }

    struct MockRegistryWithProbeProvider;

    impl DatabaseProviderRegistry for MockRegistryWithProbeProvider {
        fn register(&self, _: Arc<dyn DatabaseProvider>) -> RegistryResult<()> {
            Ok(())
        }
        fn get(&self, name: &str) -> Option<Arc<dyn DatabaseProvider>> {
            if name.eq_ignore_ascii_case("postgres") {
                Some(Arc::new(MockProviderWithProbe))
            } else {
                None
            }
        }
        fn list(&self) -> Vec<String> {
            vec!["postgres".into()]
        }
        fn unregister(&self, _: &str) -> Option<Arc<dyn DatabaseProvider>> {
            None
        }
    }

    /// Probe fails 3 times then succeeds on the 4th attempt — checkout must return Ok.
    #[tokio::test]
    async fn checkout_probe_succeeds_after_retries() {
        let repo = MockRepositoryWithEnv {
            current_commit: "abc123".into(),
        };
        let compute = MockComputeWithProbeFailures {
            exec_fail_remaining: AtomicUsize::new(3),
        };
        let usecase = CheckoutRepoUseCase::new(
            Arc::new(repo),
            Arc::new(compute),
            Arc::new(MockRegistryWithProbeProvider),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(dir.path().to_path_buf(), "main".into(), None)
            .await;
        assert!(
            result.is_ok(),
            "probe should succeed after 3 retries: {result:?}"
        );
    }

    /// Probe fails more times than the retry budget (20 > 15) — checkout must return Err.
    #[tokio::test]
    async fn checkout_probe_fails_after_exhausting_retries() {
        let repo = MockRepositoryWithEnv {
            current_commit: "abc123".into(),
        };
        let compute = MockComputeWithProbeFailures {
            exec_fail_remaining: AtomicUsize::new(20),
        };
        let usecase = CheckoutRepoUseCase::new(
            Arc::new(repo),
            Arc::new(compute),
            Arc::new(MockRegistryWithProbeProvider),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(dir.path().to_path_buf(), "main".into(), None)
            .await;
        assert!(
            matches!(result, Err(CheckoutRepoError::Compute(_))),
            "probe should fail after exhausting retries: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Probes as default container user when exec-as-root unavailable
    // -----------------------------------------------------------------------

    /// A compute mock with `supports_exec_as_root = false` that succeeds immediately.
    struct MockComputeNoRootSuccess;

    #[async_trait]
    impl Compute for MockComputeNoRootSuccess {
        async fn capabilities(&self) -> crate::ports::compute::Result<ComputeCapabilities> {
            Ok(ComputeCapabilities {
                supports_stream_snapshot: false,
                supports_exec_as_root: false,
            })
        }
        async fn exec(
            &self,
            _id: &InstanceId,
            _command: &str,
            _user: Option<&str>,
        ) -> crate::ports::compute::Result<crate::ports::compute::ExecOutput> {
            Ok(crate::ports::compute::ExecOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        async fn provision(
            &self,
            _: &ComputeDefinition,
        ) -> crate::ports::compute::Result<InstanceId> {
            Ok(InstanceId("mock-no-root".into()))
        }
        async fn start(
            &self,
            id: &InstanceId,
            _: StartOptions,
        ) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn stop(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Stopped,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn restart(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn status(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn prepare_for_snapshot(
            &self,
            _: &InstanceId,
            _: &[String],
        ) -> crate::ports::compute::Result<()> {
            Ok(())
        }
        async fn logs(
            &self,
            _: &InstanceId,
            _: crate::ports::compute::LogsOptions,
        ) -> crate::ports::compute::Result<Vec<crate::ports::compute::LogEntry>> {
            Ok(vec![])
        }
        async fn pause(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Paused,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn unpause(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn get_connection_info(
            &self,
            _id: &InstanceId,
            port: u16,
        ) -> crate::ports::compute::Result<crate::ports::compute::InstanceConnectionInfo> {
            Ok(crate::ports::compute::InstanceConnectionInfo {
                host: "127.0.0.1".into(),
                port,
                env: vec![],
            })
        }
        async fn get_instance_data_mount_host_path(
            &self,
            _id: &InstanceId,
            _: &str,
        ) -> crate::ports::compute::Result<Option<PathBuf>> {
            Ok(None)
        }
        async fn remove_instance(&self, _id: &InstanceId) -> crate::ports::compute::Result<()> {
            Ok(())
        }
        async fn get_task_connection_info(
            &self,
            _id: &InstanceId,
            port: u16,
        ) -> crate::ports::compute::Result<crate::ports::compute::InstanceConnectionInfo> {
            Ok(crate::ports::compute::InstanceConnectionInfo {
                host: "172.17.0.2".into(),
                port,
                env: vec![],
            })
        }
        async fn run_task(
            &self,
            _: &ComputeDefinition,
            _: &str,
            _: Option<&InstanceId>,
        ) -> crate::ports::compute::Result<crate::ports::compute::ExecOutput> {
            Ok(crate::ports::compute::ExecOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    /// A compute mock with `supports_exec_as_root = false` that always fails exec.
    struct MockComputeNoRootFail;

    #[async_trait]
    impl Compute for MockComputeNoRootFail {
        async fn capabilities(&self) -> crate::ports::compute::Result<ComputeCapabilities> {
            Ok(ComputeCapabilities {
                supports_stream_snapshot: false,
                supports_exec_as_root: false,
            })
        }
        async fn exec(
            &self,
            _id: &InstanceId,
            _command: &str,
            _user: Option<&str>,
        ) -> crate::ports::compute::Result<crate::ports::compute::ExecOutput> {
            Ok(crate::ports::compute::ExecOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: "db not ready".into(),
            })
        }
        async fn provision(
            &self,
            _: &ComputeDefinition,
        ) -> crate::ports::compute::Result<InstanceId> {
            Ok(InstanceId("mock-no-root-fail".into()))
        }
        async fn start(
            &self,
            id: &InstanceId,
            _: StartOptions,
        ) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn stop(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Stopped,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn restart(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn status(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn prepare_for_snapshot(
            &self,
            _: &InstanceId,
            _: &[String],
        ) -> crate::ports::compute::Result<()> {
            Ok(())
        }
        async fn logs(
            &self,
            _: &InstanceId,
            _: crate::ports::compute::LogsOptions,
        ) -> crate::ports::compute::Result<Vec<crate::ports::compute::LogEntry>> {
            Ok(vec![])
        }
        async fn pause(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Paused,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn unpause(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn get_connection_info(
            &self,
            _id: &InstanceId,
            port: u16,
        ) -> crate::ports::compute::Result<crate::ports::compute::InstanceConnectionInfo> {
            Ok(crate::ports::compute::InstanceConnectionInfo {
                host: "127.0.0.1".into(),
                port,
                env: vec![],
            })
        }
        async fn get_instance_data_mount_host_path(
            &self,
            _id: &InstanceId,
            _: &str,
        ) -> crate::ports::compute::Result<Option<PathBuf>> {
            Ok(None)
        }
        async fn remove_instance(&self, _id: &InstanceId) -> crate::ports::compute::Result<()> {
            Ok(())
        }
        async fn get_task_connection_info(
            &self,
            _id: &InstanceId,
            port: u16,
        ) -> crate::ports::compute::Result<crate::ports::compute::InstanceConnectionInfo> {
            Ok(crate::ports::compute::InstanceConnectionInfo {
                host: "172.17.0.2".into(),
                port,
                env: vec![],
            })
        }
        async fn run_task(
            &self,
            _: &ComputeDefinition,
            _: &str,
            _: Option<&InstanceId>,
        ) -> crate::ports::compute::Result<crate::ports::compute::ExecOutput> {
            Ok(crate::ports::compute::ExecOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    /// When `supports_exec_as_root = false`, probes still run (as default container
    /// user) and checkout succeeds when the probe returns exit_code 0.
    #[tokio::test]
    async fn checkout_probe_runs_as_default_user_when_no_root_exec() {
        let repo = MockRepositoryWithEnv {
            current_commit: "abc123".into(),
        };
        let usecase = CheckoutRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockComputeNoRootSuccess),
            Arc::new(MockRegistryWithProbeProvider),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(dir.path().to_path_buf(), "main".into(), None)
            .await;
        assert!(
            result.is_ok(),
            "probes should succeed as default user when exec-as-root is unavailable: {result:?}"
        );
    }

    /// When `supports_exec_as_root = false`, a failing probe must still cause
    /// checkout to return `Err` — health is not silently skipped.
    #[tokio::test]
    async fn checkout_probe_fails_as_default_user() {
        let repo = MockRepositoryWithEnv {
            current_commit: "abc123".into(),
        };
        let usecase = CheckoutRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockComputeNoRootFail),
            Arc::new(MockRegistryWithProbeProvider),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(dir.path().to_path_buf(), "main".into(), None)
            .await;
        assert!(
            matches!(result, Err(CheckoutRepoError::Compute(_))),
            "failing probe as default user should surface an error: {result:?}"
        );
    }
}
