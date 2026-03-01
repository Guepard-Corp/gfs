use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;

use crate::model::config::{EnvironmentConfig, RuntimeConfig};
use crate::ports::compute::{Compute, ComputeError, StartOptions};
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

// ---------------------------------------------------------------------------
// Use case
// ---------------------------------------------------------------------------

/// Use case for initialising a repository and optionally provisioning a database.
///
/// `R` is generic over [`DatabaseProviderRegistry`] because that trait is not
/// dyn-compatible (its `register` method uses `impl Into<String>`).
pub struct InitRepositoryUseCase<R: DatabaseProviderRegistry> {
    repository: Arc<dyn Repository>,
    compute: Arc<dyn Compute>,
    registry: Arc<R>,
}

impl<R: DatabaseProviderRegistry> InitRepositoryUseCase<R> {
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

    /// Initialise the repository and optionally provision a database.
    ///
    /// When `database_provider` is set, `database_version` must also be set and non-empty.
    pub async fn run(
        &self,
        path: PathBuf,
        mount_point: Option<String>,
        database_provider: Option<String>,
        database_version: Option<String>,
    ) -> std::result::Result<(), InitRepoError> {
        self.repository.init(&path, mount_point).await?;

        if let Some(provider) = database_provider {
            let version = database_version
                .filter(|v| !v.is_empty())
                .ok_or(InitRepoError::DatabaseVersionRequired)?;
            self.deploy_database(&path, provider, version).await?;
        }

        Ok(())
    }

    async fn deploy_database(
        &self,
        repo_path: &std::path::Path,
        provider_name: String,
        database_version: String,
    ) -> std::result::Result<(), InitRepoError> {
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
        let base = definition
            .image
            .split(':')
            .next()
            .unwrap_or(&definition.image);
        definition.image = format!("{}:{}", base, database_version);

        let workspace_data_dir = self
            .repository
            .get_workspace_data_dir_for_head(repo_path)
            .await?;
        definition.host_data_dir = Some(workspace_data_dir);

        let id = self.compute.provision(&definition).await?;
        self.compute.start(&id, StartOptions::default()).await?;

        let database_version = provider.version_from_image(&definition);

        let environment = EnvironmentConfig {
            database_provider: provider_name,
            database_version,
        };
        self.repository
            .update_environment_config(repo_path, environment)
            .await?;

        let runtime = RuntimeConfig {
            runtime_provider: "docker".to_string(),
            runtime_version: "24".to_string(),
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

    use crate::model::config::{EnvironmentConfig, RuntimeConfig};
    use crate::ports::compute::{
        Compute, ComputeDefinition, InstanceId, InstanceState, InstanceStatus, StartOptions,
    };
    use crate::ports::database_provider::{
        ConnectionParams, DatabaseProvider, DatabaseProviderArg, DatabaseProviderRegistry,
        ProviderError, Result as RegistryResult, SIGTERM, SupportedFeature,
    };
    use crate::ports::repository::Repository;

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
        let usecase = InitRepositoryUseCase::new(
            Arc::new(MockRepository),
            Arc::new(MockCompute),
            Arc::new(MockRegistry),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(dir.path().to_path_buf(), None, None, None)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn init_with_database_provider() {
        let usecase = InitRepositoryUseCase::new(
            Arc::new(MockRepository),
            Arc::new(MockCompute),
            Arc::new(MockRegistry),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(
                dir.path().to_path_buf(),
                None,
                Some("postgres".into()),
                Some("17".into()),
            )
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn init_database_version_required() {
        let usecase = InitRepositoryUseCase::new(
            Arc::new(MockRepository),
            Arc::new(MockCompute),
            Arc::new(MockRegistry),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(
                dir.path().to_path_buf(),
                None,
                Some("postgres".into()),
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
            Arc::new(MockCompute),
            Arc::new(MockRegistry),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(
                dir.path().to_path_buf(),
                None,
                Some("mysql".into()),
                Some("8".into()),
            )
            .await;
        assert!(matches!(
            result,
            Err(InitRepoError::UnknownDatabaseProvider(_))
        ));
    }
}
