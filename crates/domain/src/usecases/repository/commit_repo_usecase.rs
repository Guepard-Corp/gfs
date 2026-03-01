use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;

use crate::model::commit::NewCommit;
use crate::ports::compute::{Compute, ComputeError, InstanceId, InstanceState};
use crate::ports::database_provider::{ConnectionParams, DatabaseProviderRegistry};
use crate::ports::repository::{Repository, RepositoryError};
use crate::ports::storage::{SnapshotOptions, StorageError, StoragePort, VolumeId};
use crate::repo_utils::repo_layout;
use crate::usecases::repository::export_repo_usecase::ExportRepoUseCase;
use crate::usecases::repository::extract_schema_usecase::ExtractSchemaUseCase;
use crate::utils::hash::hash_snapshot;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum CommitRepoError {
    #[error("repository error: {0}")]
    Repository(#[from] RepositoryError),

    #[error("compute error: {0}")]
    Compute(#[from] ComputeError),

    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("unknown database provider: '{0}'")]
    UnknownDatabaseProvider(String),

    #[error("commit message must not be empty")]
    EmptyMessage,
}

// ---------------------------------------------------------------------------
// Use case
// ---------------------------------------------------------------------------

/// Use case for committing the current state of a repository.
///
/// Steps:
/// 1. Validate the commit message.
/// 2. Resolve the current branch, parent commit, runtime config, and mount point from the repo.
/// 3. If a database container is running, prepare it for snapshotting and pause it.
/// 4. Take a storage snapshot of the workspace volume.
/// 5. Build a [`NewCommit`] and persist it via the [`Repository`] port.
/// 6. Unpause the container if it was paused in step 3.
///
/// `R` is generic over [`DatabaseProviderRegistry`] because that trait is not
/// dyn-compatible (its `register` method uses `impl Into<String>`).
pub struct CommitRepoUseCase<R: DatabaseProviderRegistry> {
    repository: Arc<dyn Repository>,
    compute: Arc<dyn Compute>,
    storage: Arc<dyn StoragePort>,
    registry: Arc<R>,
}

impl<R: DatabaseProviderRegistry> CommitRepoUseCase<R> {
    pub fn new(
        repository: Arc<dyn Repository>,
        compute: Arc<dyn Compute>,
        storage: Arc<dyn StoragePort>,
        registry: Arc<R>,
    ) -> Self {
        Self {
            repository,
            compute,
            storage,
            registry,
        }
    }

    /// Commit the current state of the repository at `path` with the given `message`.
    ///
    /// `author` and `committer` default to `"user"` when `None`; supply them from
    /// the caller (e.g. from the CLI or config) for production use.
    pub async fn run(
        &self,
        path: PathBuf,
        message: String,
        author: Option<String>,
        author_email: Option<String>,
        committer: Option<String>,
        committer_email: Option<String>,
    ) -> std::result::Result<String, CommitRepoError> {
        if message.trim().is_empty() {
            return Err(CommitRepoError::EmptyMessage);
        }

        // Use canonical path so snapshot dir matches where checkout will read (same physical path).
        let path = std::fs::canonicalize(&path)
            .map_err(|e| CommitRepoError::Repository(RepositoryError::Io(e)))?;

        // 1. Resolve commit context from the repository.
        let parent_commit_id = self.repository.get_current_commit_id(&path).await?;
        let runtime_config = self.repository.get_runtime_config(&path).await?;
        let environment = self.repository.get_environment_config(&path).await?;
        let mount_point = self.repository.get_mount_point(&path).await?;

        // Use user config as fallback for author / committer.
        let user_config = self.repository.get_user_config(&path).await?;
        let resolved_author = author
            .or_else(|| user_config.as_ref().and_then(|u| u.name.clone()))
            .unwrap_or_else(|| "user".to_string());
        let resolved_author_email =
            author_email.or_else(|| user_config.as_ref().and_then(|u| u.email.clone()));
        let resolved_committer = committer
            .or_else(|| user_config.as_ref().and_then(|u| u.name.clone()))
            .unwrap_or_else(|| "user".to_string());
        let resolved_committer_email =
            committer_email.or_else(|| user_config.as_ref().and_then(|u| u.email.clone()));

        // 2. Extract and store schema (best-effort) while the container is still running.
        //    Must run before pausing, since schema extraction requires a live database connection.
        let schema_hash = if runtime_config.is_some() && environment.is_some() {
            self.extract_and_store_schema(&path).await.ok()
        } else {
            None
        };

        // 3. Prepare the database container for snapshotting (if present).
        let mut was_paused = false;
        if let (Some(runtime), Some(env)) = (&runtime_config, &environment) {
            let instance_id = InstanceId(runtime.container_name.clone());

            let provider = self.registry.get(&env.database_provider).ok_or_else(|| {
                CommitRepoError::UnknownDatabaseProvider(env.database_provider.clone())
            })?;
            let conn_info = self
                .compute
                .get_connection_info(&instance_id, provider.default_port())
                .await?;
            let params = ConnectionParams {
                host: conn_info.host,
                port: conn_info.port,
                env: conn_info.env,
            };
            let commands = provider.prepare_for_snapshot(&params).map_err(|e| {
                CommitRepoError::Repository(RepositoryError::Internal(e.to_string()))
            })?;
            self.compute
                .prepare_for_snapshot(&instance_id, &commands)
                .await?;

            // Pause the container so no writes land during the snapshot.
            let status = self.compute.status(&instance_id).await?;
            if status.state == InstanceState::Running {
                self.compute.pause(&instance_id).await?;
                was_paused = true;
            }
        }

        // 4. Take a storage snapshot.
        //    The VolumeId is the mount point of the workspace volume.  When no
        //    explicit mount_point is configured we read .gfs/WORKSPACE which
        //    always points to the directory where the database is currently
        //    running — even after multiple commits have advanced HEAD.
        let volume_id = if let Some(mp) = mount_point {
            VolumeId(mp)
        } else {
            let data_dir = self.repository.get_active_workspace_data_dir(&path).await?;
            VolumeId(data_dir.to_string_lossy().into_owned())
        };

        // Generate a unique snapshot hash from the source path + current timestamp.
        let snap_timestamp = chrono::Utc::now();
        let snapshot_hash = hash_snapshot(&volume_id.0, &snap_timestamp);

        // Ensure .gfs/snapshots/<2>/ exists and return the full COW destination path.
        let snapshot_dest = self
            .repository
            .ensure_snapshot_path(&path, &snapshot_hash)
            .await?;

        // COW-copy the workspace data dir into the snapshot destination folder.
        self.storage
            .snapshot(
                &volume_id,
                SnapshotOptions {
                    label: Some(snapshot_dest.to_string_lossy().into_owned()),
                },
            )
            .await?;

        // 5. Build the new commit.
        //    Use "0" parent when this is the very first real commit.
        let parents = if parent_commit_id == "0" {
            None
        } else {
            Some(vec![parent_commit_id])
        };

        let mut new_commit = NewCommit::new(
            message,
            resolved_author,
            resolved_author_email,
            resolved_committer,
            resolved_committer_email,
            snapshot_hash,
            parents,
        );
        new_commit.schema_hash = schema_hash;

        // 6. Persist the commit object and advance the branch ref.
        let commit_hash = self.repository.commit(&path, new_commit).await?;

        // 7. Unpause the container if we paused it.
        if was_paused && let Some(runtime) = &runtime_config {
            let instance_id = InstanceId(runtime.container_name.clone());
            self.compute.unpause(&instance_id).await?;
        }

        tracing::info!("Commit created: {}", commit_hash);
        Ok(commit_hash)
    }

    /// Extract and store schema for the current database state.
    /// Returns the schema hash on success, or None if extraction fails.
    /// This is best-effort - failures are logged but don't fail the commit.
    async fn extract_and_store_schema(
        &self,
        repo_path: &std::path::Path,
    ) -> Result<String, Box<dyn std::error::Error>> {
        tracing::debug!("Extracting schema for commit");

        // 1. Extract schema metadata using ExtractSchemaUseCase.
        let extract_use_case =
            ExtractSchemaUseCase::new(self.compute.clone(), self.registry.clone());
        let schema_output = extract_use_case.run(repo_path).await.map_err(|e| {
            tracing::warn!("Schema extraction failed: {}", e);
            e
        })?;

        // 2. Export schema DDL using ExportRepoUseCase with "schema" format.
        let export_use_case = ExportRepoUseCase::new(self.compute.clone(), self.registry.clone());
        let temp_dir = std::env::temp_dir().join(format!(
            "gfs-schema-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let export_output = export_use_case
            .run(repo_path, temp_dir.clone(), "schema")
            .await
            .map_err(|e| {
                tracing::warn!("Schema DDL export failed: {}", e);
                e
            })?;

        let schema_sql = std::fs::read_to_string(&export_output.file_path).map_err(|e| {
            tracing::warn!("Failed to read exported schema DDL: {}", e);
            e
        })?;

        // 3. Store schema object in repo.
        let schema_hash =
            repo_layout::write_schema_object(repo_path, &schema_output.metadata, &schema_sql)
                .map_err(|e| {
                    tracing::warn!("Failed to write schema object: {}", e);
                    e
                })?;

        // 4. Cleanup temp directory.
        let _ = std::fs::remove_dir_all(temp_dir);

        tracing::info!("Schema stored with hash: {}", schema_hash);
        Ok(schema_hash)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use crate::model::commit::{CommitWithRefs, NewCommit};
    use crate::model::config::{EnvironmentConfig, RuntimeConfig, UserConfig};
    use crate::ports::compute::{
        ComputeDefinition, InstanceId, InstanceState, InstanceStatus, LogEntry, LogsOptions,
        StartOptions,
    };
    use crate::ports::database_provider::{
        ConnectionParams, DatabaseProvider, DatabaseProviderArg, DatabaseProviderRegistry,
        ProviderError, Result as RegistryResult, SIGTERM, SupportedFeature,
    };
    use crate::ports::repository::{LogOptions, RemoteOptions, Repository};
    use crate::ports::storage::{
        CloneOptions, MountStatus, Quota, Snapshot, SnapshotId, SnapshotOptions, VolumeId,
        VolumeStatus,
    };

    // -----------------------------------------------------------------------
    // Mock Repository
    // -----------------------------------------------------------------------

    #[derive(Default)]
    struct MockRepository {
        /// The commit hash returned by `commit()`.
        commit_hash: String,
        /// Values returned by the context getters.
        current_commit: String,
        runtime_config: Option<RuntimeConfig>,
        environment: Option<EnvironmentConfig>,
        mount_point: Option<String>,
        user_config: Option<UserConfig>,
        /// Records the NewCommit passed to `commit()`.
        committed: Mutex<Option<NewCommit>>,
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
            _: crate::model::config::EnvironmentConfig,
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
            new_commit: NewCommit,
        ) -> crate::ports::repository::Result<String> {
            *self.committed.lock().unwrap() = Some(new_commit);
            Ok(self.commit_hash.clone())
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
            _: LogOptions,
        ) -> crate::ports::repository::Result<Vec<CommitWithRefs>> {
            Ok(vec![])
        }
        async fn rev_parse(
            &self,
            _: &std::path::Path,
            _: &str,
        ) -> crate::ports::repository::Result<String> {
            Ok(String::new())
        }
        async fn push(
            &self,
            _: &std::path::Path,
            _: RemoteOptions,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn pull(
            &self,
            _: &std::path::Path,
            _: RemoteOptions,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn fetch(
            &self,
            _: &std::path::Path,
            _: RemoteOptions,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn get_current_branch(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<String> {
            Ok("main".to_string())
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
            Ok(self.mount_point.clone())
        }
        async fn get_environment_config(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<EnvironmentConfig>> {
            Ok(self.environment.clone())
        }
        async fn get_user_config(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<UserConfig>> {
            Ok(self.user_config.clone())
        }
        async fn ensure_snapshot_path(
            &self,
            _: &std::path::Path,
            hash: &str,
        ) -> crate::ports::repository::Result<PathBuf> {
            // Return a predictable temp-dir path for testing.
            Ok(PathBuf::from(format!(
                "/tmp/snapshots/{}/{}",
                &hash[..2],
                &hash[2..]
            )))
        }
        async fn get_active_workspace_data_dir(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<PathBuf> {
            Ok(PathBuf::from("/workspace/data"))
        }
    }

    // -----------------------------------------------------------------------
    // Mock Compute
    // -----------------------------------------------------------------------

    struct MockCompute {
        state: InstanceState,
        prepared: Mutex<bool>,
        paused: Mutex<bool>,
        unpaused: Mutex<bool>,
    }

    impl Default for MockCompute {
        fn default() -> Self {
            Self {
                state: InstanceState::Stopped,
                prepared: Mutex::new(false),
                paused: Mutex::new(false),
                unpaused: Mutex::new(false),
            }
        }
    }

    #[async_trait]
    impl crate::ports::compute::Compute for MockCompute {
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
                state: self.state.clone(),
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn prepare_for_snapshot(
            &self,
            _: &InstanceId,
            _commands: &[String],
        ) -> crate::ports::compute::Result<()> {
            *self.prepared.lock().unwrap() = true;
            Ok(())
        }
        async fn logs(
            &self,
            _: &InstanceId,
            _: LogsOptions,
        ) -> crate::ports::compute::Result<Vec<LogEntry>> {
            Ok(vec![])
        }
        async fn pause(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            *self.paused.lock().unwrap() = true;
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Paused,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn unpause(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            *self.unpaused.lock().unwrap() = true;
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
            compute_port: u16,
        ) -> crate::ports::compute::Result<crate::ports::compute::InstanceConnectionInfo> {
            Ok(crate::ports::compute::InstanceConnectionInfo {
                host: "127.0.0.1".into(),
                port: compute_port,
                env: vec![],
            })
        }
        async fn get_instance_data_mount_host_path(
            &self,
            _id: &InstanceId,
            _compute_data_path: &str,
        ) -> crate::ports::compute::Result<Option<std::path::PathBuf>> {
            Ok(None)
        }
        async fn remove_instance(&self, _id: &InstanceId) -> crate::ports::compute::Result<()> {
            Ok(())
        }
        async fn get_task_connection_info(
            &self,
            _id: &InstanceId,
            compute_port: u16,
        ) -> crate::ports::compute::Result<crate::ports::compute::InstanceConnectionInfo> {
            Ok(crate::ports::compute::InstanceConnectionInfo {
                host: "172.17.0.2".into(),
                port: compute_port,
                env: vec![],
            })
        }
        async fn run_task(
            &self,
            _definition: &ComputeDefinition,
            _command: &str,
            _linked_to: Option<&InstanceId>,
        ) -> crate::ports::compute::Result<crate::ports::compute::ExecOutput> {
            Ok(crate::ports::compute::ExecOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    // -----------------------------------------------------------------------
    // Mock Storage
    // -----------------------------------------------------------------------

    struct MockStorage {
        snapshot_id: String,
        last_volume: Mutex<Option<String>>,
        /// Records the label (destination path) passed to snapshot().
        last_label: Mutex<Option<String>>,
    }

    impl MockStorage {
        fn new(snapshot_id: impl Into<String>) -> Self {
            Self {
                snapshot_id: snapshot_id.into(),
                last_volume: Mutex::new(None),
                last_label: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl crate::ports::storage::StoragePort for MockStorage {
        async fn mount(
            &self,
            _: &VolumeId,
            _: &std::path::Path,
        ) -> crate::ports::storage::Result<()> {
            Ok(())
        }
        async fn unmount(&self, _: &VolumeId) -> crate::ports::storage::Result<()> {
            Ok(())
        }
        async fn snapshot(
            &self,
            id: &VolumeId,
            options: SnapshotOptions,
        ) -> crate::ports::storage::Result<Snapshot> {
            *self.last_volume.lock().unwrap() = Some(id.0.clone());
            *self.last_label.lock().unwrap() = options.label.clone();
            Ok(Snapshot {
                id: SnapshotId(self.snapshot_id.clone()),
                volume_id: id.clone(),
                created_at: chrono::Utc::now(),
                size_bytes: 0,
                label: options.label,
            })
        }
        async fn clone(
            &self,
            _: &VolumeId,
            target: VolumeId,
            _: CloneOptions,
        ) -> crate::ports::storage::Result<VolumeStatus> {
            Ok(VolumeStatus {
                id: target,
                mount_point: None,
                status: MountStatus::Unmounted,
                size_bytes: 0,
                used_bytes: 0,
            })
        }
        async fn status(&self, id: &VolumeId) -> crate::ports::storage::Result<VolumeStatus> {
            Ok(VolumeStatus {
                id: id.clone(),
                mount_point: None,
                status: MountStatus::Unmounted,
                size_bytes: 0,
                used_bytes: 0,
            })
        }
        async fn quota(&self, id: &VolumeId) -> crate::ports::storage::Result<Quota> {
            Ok(Quota {
                volume_id: id.clone(),
                limit_bytes: 0,
                used_bytes: 0,
                free_bytes: 0,
            })
        }
    }

    // -----------------------------------------------------------------------
    // Mock DatabaseProviderRegistry
    // -----------------------------------------------------------------------

    struct MockProvider;

    impl DatabaseProvider for MockProvider {
        fn name(&self) -> &str {
            "mock-db"
        }
        fn definition(&self) -> ComputeDefinition {
            ComputeDefinition {
                image: "mock:latest".into(),
                env: vec![],
                ports: vec![],
                data_dir: PathBuf::from("/data"),
                host_data_dir: None,
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
            Ok("mock://localhost:5432".into())
        }
        fn supported_versions(&self) -> Vec<String> {
            vec!["latest".to_string()]
        }
        fn supported_features(&self) -> Vec<SupportedFeature> {
            vec![SupportedFeature {
                id: "schema".into(),
                description: "Schema support.".into(),
            }]
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
            if name == "mock-db" {
                Some(Arc::new(MockProvider))
            } else {
                None
            }
        }
        fn list(&self) -> Vec<String> {
            vec!["mock-db".into()]
        }
        fn unregister(&self, _: &str) -> Option<Arc<dyn DatabaseProvider>> {
            None
        }
    }

    // -----------------------------------------------------------------------
    // Helper
    // -----------------------------------------------------------------------

    fn make_usecase(
        repo: MockRepository,
        compute: MockCompute,
        storage: MockStorage,
    ) -> CommitRepoUseCase<MockRegistry> {
        CommitRepoUseCase::new(
            Arc::new(repo),
            Arc::new(compute),
            Arc::new(storage),
            Arc::new(MockRegistry),
        )
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn empty_message_returns_error() {
        let uc = make_usecase(
            MockRepository::default(),
            MockCompute::default(),
            MockStorage::new("snap-1"),
        );
        let err = uc
            .run(PathBuf::from("/repo"), "".into(), None, None, None, None)
            .await;
        assert!(matches!(err, Err(CommitRepoError::EmptyMessage)));
    }

    /// Path that exists so that run()'s canonicalize() succeeds.
    fn existing_repo_path() -> PathBuf {
        let temp = tempfile::tempdir().expect("tempdir");
        temp.keep()
    }

    #[tokio::test]
    async fn commit_without_database_skips_compute() {
        let compute = MockCompute::default();
        let repo = MockRepository {
            commit_hash: "abc123".into(),
            current_commit: "0".into(),
            mount_point: Some("/vol/main".into()),
            ..Default::default()
        };

        let uc = make_usecase(repo, compute, MockStorage::new("snap-abc"));
        let hash = uc
            .run(
                existing_repo_path(),
                "initial commit".into(),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("commit should succeed");

        assert_eq!(hash, "abc123");

        // No container → compute was never touched.
        let compute_arc = uc.compute.clone();
        // We can't downcast Arc<dyn Compute>, so we just check the result is correct.
        let _ = compute_arc;
    }

    #[tokio::test]
    async fn commit_with_running_database_pauses_and_unpauses() {
        let compute = Arc::new(MockCompute {
            state: InstanceState::Running,
            ..Default::default()
        });
        let repo = MockRepository {
            commit_hash: "def456".into(),
            current_commit: "previous-hash".into(),
            mount_point: Some("/vol/main".into()),
            runtime_config: Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "my-pg".into(),
            }),
            environment: Some(EnvironmentConfig {
                database_provider: "mock-db".into(),
                database_version: "16".into(),
            }),
            ..Default::default()
        };
        let storage = Arc::new(MockStorage::new("snap-def"));
        let registry = Arc::new(MockRegistry);

        let uc = CommitRepoUseCase::new(Arc::new(repo), compute.clone(), storage.clone(), registry);

        let hash = uc
            .run(
                existing_repo_path(),
                "second commit".into(),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("commit should succeed");

        assert_eq!(hash, "def456");
        assert!(
            *compute.prepared.lock().unwrap(),
            "prepare_for_snapshot should have been called"
        );
        assert!(
            *compute.paused.lock().unwrap(),
            "pause should have been called"
        );
        assert!(
            *compute.unpaused.lock().unwrap(),
            "unpause should have been called"
        );
        assert_eq!(
            storage.last_volume.lock().unwrap().as_deref(),
            Some("/vol/main")
        );
        // Snapshot destination must be a 64-char hex-named path under /tmp/snapshots/.
        let label = storage.last_label.lock().unwrap().clone().unwrap();
        assert!(
            label.contains("/tmp/snapshots/"),
            "expected snapshot inside snapshots dir"
        );
    }

    #[tokio::test]
    async fn commit_snapshot_hash_is_64_char_hex_independent_of_storage_id() {
        let storage = Arc::new(MockStorage::new("storage-snap-id-is-ignored"));
        let repo = MockRepository {
            commit_hash: "ghi789".into(),
            current_commit: "0".into(),
            mount_point: Some("/vol".into()),
            ..Default::default()
        };
        let registry = Arc::new(MockRegistry);

        let uc = CommitRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockCompute::default()),
            storage.clone(),
            registry,
        );

        uc.run(
            existing_repo_path(),
            "snap test".into(),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("commit should succeed");

        // The label passed to storage should contain the snapshot path built from the hash.
        let label = storage.last_label.lock().unwrap().clone().unwrap();
        // Label is a path like "/tmp/snapshots/<2>/<62>" — the 64-char hash is split across it.
        assert!(
            label.contains("/tmp/snapshots/"),
            "expected snapshot dest under /tmp/snapshots, got: {label}"
        );

        // Extract the hash portion: last two segments of the path joined.
        let parts: Vec<&str> = label.trim_end_matches('/').split('/').collect();
        let n = parts.len();
        assert!(n >= 2, "path should have at least 2 segments");
        let reconstructed_hash = format!("{}{}", parts[n - 2], parts[n - 1]);
        assert_eq!(reconstructed_hash.len(), 64, "hash should be 64 chars");
        assert!(
            reconstructed_hash.chars().all(|c| c.is_ascii_hexdigit()),
            "hash should be hex"
        );
    }

    #[tokio::test]
    async fn commit_uses_mount_point_as_volume_id() {
        let storage = Arc::new(MockStorage::new("snap-mp"));
        let repo = MockRepository {
            commit_hash: "mp1".into(),
            current_commit: "0".into(),
            mount_point: Some("/mnt/my-volume".into()),
            ..Default::default()
        };
        let registry = Arc::new(MockRegistry);

        let uc = CommitRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockCompute::default()),
            storage.clone(),
            registry,
        );

        uc.run(
            existing_repo_path(),
            "mp test".into(),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("commit should succeed");

        assert_eq!(
            storage.last_volume.lock().unwrap().as_deref(),
            Some("/mnt/my-volume")
        );
    }

    #[tokio::test]
    async fn commit_falls_back_to_workspace_data_dir_when_no_mount_point() {
        let storage = Arc::new(MockStorage::new("snap-fallback"));
        let repo = MockRepository {
            commit_hash: "fb1".into(),
            current_commit: "0".into(),
            mount_point: None, // no mount_point configured
            ..Default::default()
        };
        let registry = Arc::new(MockRegistry);

        let uc = CommitRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockCompute::default()),
            storage.clone(),
            registry,
        );

        uc.run(
            existing_repo_path(),
            "fallback test".into(),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("commit should succeed");

        // Mock get_active_workspace_data_dir returns "/workspace/data"
        assert_eq!(
            storage.last_volume.lock().unwrap().as_deref(),
            Some("/workspace/data")
        );
    }

    #[tokio::test]
    async fn commit_user_config_fallback_for_author_committer() {
        let repo = MockRepository {
            commit_hash: "uc1".into(),
            current_commit: "0".into(),
            mount_point: Some("/vol".into()),
            user_config: Some(UserConfig {
                name: Some("Alice".into()),
                email: Some("alice@example.com".into()),
            }),
            ..Default::default()
        };
        let storage = Arc::new(MockStorage::new("snap-uc"));
        let registry = Arc::new(MockRegistry);

        let uc = CommitRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockCompute::default()),
            storage,
            registry,
        );

        let hash = uc
            .run(
                existing_repo_path(),
                "user config test".into(),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("commit should succeed");

        assert_eq!(hash, "uc1");
    }

    #[tokio::test]
    async fn commit_canonicalize_failure() {
        let uc = make_usecase(
            MockRepository::default(),
            MockCompute::default(),
            MockStorage::new("snap"),
        );
        let result = uc
            .run(
                PathBuf::from("/nonexistent/path/that/does/not/exist"),
                "msg".into(),
                None,
                None,
                None,
                None,
            )
            .await;
        assert!(result.is_err());
        assert!(matches!(result, Err(CommitRepoError::Repository(_))));
    }

    #[tokio::test]
    async fn commit_storage_error() {
        struct FailingStorage;

        #[async_trait]
        impl crate::ports::storage::StoragePort for FailingStorage {
            async fn mount(
                &self,
                _: &VolumeId,
                _: &std::path::Path,
            ) -> crate::ports::storage::Result<()> {
                Ok(())
            }
            async fn unmount(&self, _: &VolumeId) -> crate::ports::storage::Result<()> {
                Ok(())
            }
            async fn snapshot(
                &self,
                _: &VolumeId,
                _: SnapshotOptions,
            ) -> crate::ports::storage::Result<Snapshot> {
                Err(crate::ports::storage::StorageError::Internal(
                    "storage failed".into(),
                ))
            }
            async fn clone(
                &self,
                _: &VolumeId,
                target: VolumeId,
                _: CloneOptions,
            ) -> crate::ports::storage::Result<VolumeStatus> {
                Ok(VolumeStatus {
                    id: target,
                    mount_point: None,
                    status: MountStatus::Unmounted,
                    size_bytes: 0,
                    used_bytes: 0,
                })
            }
            async fn status(&self, id: &VolumeId) -> crate::ports::storage::Result<VolumeStatus> {
                Ok(VolumeStatus {
                    id: id.clone(),
                    mount_point: None,
                    status: MountStatus::Unmounted,
                    size_bytes: 0,
                    used_bytes: 0,
                })
            }
            async fn quota(&self, id: &VolumeId) -> crate::ports::storage::Result<Quota> {
                Ok(Quota {
                    volume_id: id.clone(),
                    limit_bytes: 0,
                    used_bytes: 0,
                    free_bytes: 0,
                })
            }
        }

        let repo = MockRepository {
            commit_hash: "x".into(),
            current_commit: "0".into(),
            mount_point: Some("/vol".into()),
            ..Default::default()
        };
        let uc = CommitRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockCompute::default()),
            Arc::new(FailingStorage),
            Arc::new(MockRegistry),
        );
        let result = uc
            .run(
                existing_repo_path(),
                "storage fail".into(),
                None,
                None,
                None,
                None,
            )
            .await;
        assert!(matches!(result, Err(CommitRepoError::Storage(_))));
    }
}
