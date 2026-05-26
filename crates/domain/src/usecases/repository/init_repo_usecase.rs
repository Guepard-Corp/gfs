use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;

#[cfg(unix)]
use crate::utils::current_user;
use crate::utils::data_dir;

use crate::model::config::{EnvironmentConfig, RuntimeConfig};
use crate::ports::compute::{Compute, ComputeError, RuntimeDescriptor, StartOptions};
use crate::ports::database_provider::DatabaseProviderRegistry;
use crate::ports::repository::{Repository, RepositoryError};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum InitRepoError {
    #[error("repository error: {0}")]
    Repository(#[from] RepositoryError),

    #[error("compute error: {0}")]
    Compute(#[from] ComputeError),

    #[error("unknown database provider: '{0}'")]
    UnknownDatabaseProvider(String),

    #[error("database_version is required when database_provider is set")]
    DatabaseVersionRequired,
}

/// Optional initial database credentials applied to the provisioned container's env.
#[derive(Debug, Default, Clone)]
pub struct DatabaseCredentials {
    pub user: Option<String>,
    pub password: Option<String>,
    pub name: Option<String>,
}

// ---------------------------------------------------------------------------
// Use case
// ---------------------------------------------------------------------------

/// Use case for initialising a repository and optionally provisioning a database.
///
/// `R` is generic over [`DatabaseProviderRegistry`] because that trait is not
/// dyn-compatible (its `register` method uses `impl Into<String>`).
pub struct InitRepositoryUseCase<R: DatabaseProviderRegistry> {
    repository: Arc<dyn Repository>,
    compute: Option<Arc<dyn Compute>>,
    registry: Arc<R>,
}

impl<R: DatabaseProviderRegistry> InitRepositoryUseCase<R> {
    pub fn new(
        repository: Arc<dyn Repository>,
        compute: Option<Arc<dyn Compute>>,
        registry: Arc<R>,
    ) -> Self {
        Self {
            repository,
            compute,
            registry,
        }
    }

    /// Initialise the repository and optionally provision a database.
    ///
    /// When `database_provider` is set, either `database_version` must be set and
    /// non-empty, or `image` must override the full container image (which pins
    /// its own version, e.g. `pgvector/pgvector:pg16`).
    pub async fn run(
        &self,
        path: PathBuf,
        mount_point: Option<String>,
        database_provider: Option<String>,
        database_version: Option<String>,
        database_port: Option<u16>,
        credentials: DatabaseCredentials,
        image: Option<String>,
    ) -> std::result::Result<(), InitRepoError> {
        self.repository.init(&path, mount_point).await?;

        if let Some(provider) = database_provider {
            self.deploy_database(
                &path,
                provider,
                database_version,
                database_port,
                credentials,
                image,
            )
            .await?;
        }

        Ok(())
    }

    async fn deploy_database(
        &self,
        repo_path: &std::path::Path,
        provider_name: String,
        database_version: Option<String>,
        database_port: Option<u16>,
        credentials: DatabaseCredentials,
        image: Option<String>,
    ) -> std::result::Result<(), InitRepoError> {
        let compute = self.compute.as_ref().ok_or_else(|| {
            InitRepoError::Compute(ComputeError::Internal(
                "database provisioning requires a compute runtime".into(),
            ))
        })?;

        let list = self.registry.list();
        let matched_name = list
            .iter()
            .find(|n| n.eq_ignore_ascii_case(&provider_name))
            .cloned();

        let provider = matched_name
            .and_then(|name| self.registry.get(&name))
            .ok_or_else(|| {
                InitRepoError::UnknownDatabaseProvider(format!(
                    "'{}'; available: {}",
                    provider_name,
                    list.join(", ")
                ))
            })?;

        let mut definition = provider.definition();
        match image {
            // Explicit image override pins its own version (e.g. an image that
            // bundles an extension the default image lacks, like pgvector).
            Some(img) => definition.image = img,
            None => {
                let version = database_version
                    .filter(|v| !v.is_empty())
                    .ok_or(InitRepoError::DatabaseVersionRequired)?;
                let base = definition
                    .image
                    .split(':')
                    .next()
                    .unwrap_or(&definition.image);
                definition.image = format!("{}:{}", base, version);
            }
        }

        if let Some(port) = database_port {
            for mapping in &mut definition.ports {
                if mapping.compute_port == provider.default_port() {
                    mapping.host_port = Some(port);
                }
            }
        }

        // Apply user-provided credentials if supported by the provider's env vars
        if let Some(user) = credentials.user {
            for env in &mut definition.env {
                if env.name.contains("USER") {
                    env.default = Some(user.clone());
                }
            }
        }
        if let Some(password) = credentials.password {
            for env in &mut definition.env {
                if env.name.contains("PASSWORD") {
                    env.default = Some(password.clone());
                }
            }
        }
        if let Some(db) = credentials.name {
            for env in &mut definition.env {
                if env.name.contains("DB") || env.name.contains("DATABASE") {
                    env.default = Some(db.clone());
                }
            }
        }

        let workspace_data_dir = self
            .repository
            .get_workspace_data_dir_for_head(repo_path)
            .await?;
        data_dir::prepare_for_database_provider(provider.name(), &workspace_data_dir).map_err(
            |e| {
                ComputeError::Internal(format!(
                    "failed to prepare data dir '{}': {e}",
                    workspace_data_dir.display()
                ))
            },
        )?;
        definition.host_data_dir = Some(workspace_data_dir);

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

        let id = compute.provision(&definition).await?;
        compute.start(&id, StartOptions::default()).await?;
        let runtime = compute
            .describe_runtime()
            .await
            .unwrap_or_else(|_| RuntimeDescriptor {
                provider: "docker".to_string(),
                version: "24".to_string(),
            });

        let database_version = provider.version_from_image(&definition);

        let environment = EnvironmentConfig {
            database_provider: provider_name,
            database_version,
            database_port,
        };
        self.repository
            .update_environment_config(repo_path, environment)
            .await?;

        let runtime = RuntimeConfig {
            runtime_provider: runtime.provider,
            runtime_version: runtime.version,
            container_name: id.0.clone(),
        };
        self.repository
            .update_runtime_config(repo_path, runtime)
            .await?;

        tracing::info!("Database deployed; instance id: {}", id);
        Ok(())
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

    use crate::adapters::gfs_repository::GfsRepository;
    use crate::model::config::{EnvironmentConfig, RuntimeConfig};
    use crate::ports::compute::{
        Compute, ComputeDefinition, InstanceId, InstanceState, InstanceStatus, StartOptions,
    };
    use crate::ports::database_provider::{
        ConnectionParams, DatabaseProvider, DatabaseProviderArg, DatabaseProviderRegistry,
        ProviderError, Result as RegistryResult, SIGTERM, SupportedFeature,
    };
    use crate::ports::repository::{Repository, RepositoryError};

    struct MockRepository;

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
            _: &str,
        ) -> crate::ports::repository::Result<String> {
            Ok(String::new())
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
            Ok("0".into())
        }
        async fn get_runtime_config(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<RuntimeConfig>> {
            Ok(None)
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
    async fn init_without_database_provider() {
        let usecase =
            InitRepositoryUseCase::new(Arc::new(MockRepository), None, Arc::new(MockRegistry));
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(
                dir.path().to_path_buf(),
                None,
                None,
                None,
                None,
                DatabaseCredentials::default(),
                None,
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn init_with_database_provider() {
        let usecase = InitRepositoryUseCase::new(
            Arc::new(MockRepository),
            Some(Arc::new(MockCompute)),
            Arc::new(MockRegistry),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(
                dir.path().to_path_buf(),
                None,
                Some("postgres".into()),
                Some("17".into()),
                None,
                DatabaseCredentials::default(),
                None,
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn init_database_version_required() {
        let usecase = InitRepositoryUseCase::new(
            Arc::new(MockRepository),
            Some(Arc::new(MockCompute)),
            Arc::new(MockRegistry),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(
                dir.path().to_path_buf(),
                None,
                Some("postgres".into()),
                None,
                None,
                DatabaseCredentials::default(),
                None,
            )
            .await;
        assert!(matches!(
            result,
            Err(InitRepoError::DatabaseVersionRequired)
        ));
    }

    #[tokio::test]
    async fn init_unknown_database_provider() {
        let usecase = InitRepositoryUseCase::new(
            Arc::new(MockRepository),
            Some(Arc::new(MockCompute)),
            Arc::new(MockRegistry),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(
                dir.path().to_path_buf(),
                None,
                Some("mysql".into()),
                Some("8".into()),
                None,
                DatabaseCredentials::default(),
                None,
            )
            .await;
        assert!(matches!(
            result,
            Err(InitRepoError::UnknownDatabaseProvider(_))
        ));
    }

    #[tokio::test]
    async fn init_fails_when_repository_already_initialized() {
        let usecase = InitRepositoryUseCase::new(
            Arc::new(GfsRepository::new()),
            Some(Arc::new(MockCompute)),
            Arc::new(MockRegistry),
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();

        // First init succeeds
        let first = usecase
            .run(
                path.clone(),
                None,
                None,
                None,
                None,
                DatabaseCredentials::default(),
                None,
            )
            .await;
        assert!(first.is_ok(), "first init should succeed: {:?}", first);

        // Second init fails with AlreadyInitialized
        let second = usecase
            .run(path, None, None, None, None, DatabaseCredentials::default(), None)
            .await;
        assert!(
            matches!(
                second,
                Err(InitRepoError::Repository(
                    RepositoryError::AlreadyInitialized(_)
                ))
            ),
            "second init should fail with AlreadyInitialized: {:?}",
            second
        );
    }
}
