//! Use case for reporting data-plane status (RFC 006).
//!
//! Aggregates repository (current branch), config (provider, version), and compute
//! runtime (container status, id, connection string) into a read-only status response.
//! Connection string is built via [`DatabaseProvider::connection_string`] using
//! params from [`Compute::get_connection_info`].

use std::path::Path;

use crate::model::status::{ComputeStatus, StatusResponse};
use crate::ports::compute::{Compute, InstanceId};
use crate::ports::database_provider::{ConnectionParams, DatabaseProviderRegistry};
use crate::ports::repository::{Repository, RepositoryError};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum StatusRepoError {
    #[error("repository error: {0}")]
    Repository(#[from] RepositoryError),
}

// ---------------------------------------------------------------------------
// Use case
// ---------------------------------------------------------------------------

/// Use case for reporting the current status of a GFS repository and its compute instance.
///
/// Steps:
/// 1. Resolve current branch from the repository.
/// 2. Load environment and runtime config from the repo.
/// 3. If runtime is configured, call Compute::status and build connection string via DatabaseProvider.
/// 4. Aggregate into [`StatusResponse`].
pub struct StatusRepoUseCase<R: DatabaseProviderRegistry> {
    repository: std::sync::Arc<dyn Repository>,
    compute: std::sync::Arc<dyn Compute>,
    registry: std::sync::Arc<R>,
}

impl<R: DatabaseProviderRegistry> StatusRepoUseCase<R> {
    pub fn new(
        repository: std::sync::Arc<dyn Repository>,
        compute: std::sync::Arc<dyn Compute>,
        registry: std::sync::Arc<R>,
    ) -> Self {
        Self {
            repository,
            compute,
            registry,
        }
    }

    /// Build the status response for the repository at `path`.
    ///
    /// Caller must ensure `path` is a valid GFS repo (e.g. resolve from CWD or `--path`).
    pub async fn run(&self, path: &Path) -> Result<StatusResponse, StatusRepoError> {
        let current_branch = self.repository.get_current_branch(path).await?;
        let environment = self.repository.get_environment_config(path).await?;
        let runtime = self.repository.get_runtime_config(path).await?;
        let active_workspace_data_dir = self
            .repository
            .get_active_workspace_data_dir(path)
            .await
            .ok()
            .map(|p| p.to_string_lossy().into_owned());

        let (compute, bind_mismatch_warning) = build_compute_status(
            &*self.compute,
            self.registry.as_ref(),
            environment.as_ref(),
            runtime.as_ref(),
            active_workspace_data_dir.as_deref(),
        )
        .await;

        Ok(StatusResponse {
            current_branch,
            compute,
            active_workspace_data_dir,
            bind_mismatch_warning,
        })
    }
}

/// Build compute status when environment and runtime config are present.
/// Returns (compute_status, bind_mismatch_warning) when the container is bound to a different path than the active workspace.
async fn build_compute_status<R: DatabaseProviderRegistry>(
    compute: &dyn Compute,
    registry: &R,
    environment: Option<&crate::model::config::EnvironmentConfig>,
    runtime: Option<&crate::model::config::RuntimeConfig>,
    active_workspace_data_dir: Option<&str>,
) -> (Option<ComputeStatus>, Option<String>) {
    let (env, runtime) = match (environment, runtime) {
        (Some(e), Some(r)) if !e.database_provider.is_empty() && !r.container_name.is_empty() => {
            (e, r)
        }
        _ => return (None, None),
    };

    let instance_id = InstanceId(runtime.container_name.clone());
    let provider_name = env.database_provider.as_str();
    let version = env.database_version.clone();

    let (container_status, container_id, connection_string, data_bind_host_path) =
        match compute.status(&instance_id).await {
            Ok(status) => {
                let container_id = status.id.0.clone();
                let container_status = status.state.as_status_str().to_string();
                let conn =
                    build_connection_string(compute, registry, &instance_id, provider_name).await;
                let data_bind =
                    get_data_bind_host_path(compute, registry, &instance_id, provider_name).await;
                (container_status, container_id, conn, data_bind)
            }
            Err(_) => (
                "not_provisioned".to_string(),
                runtime.container_name.clone(),
                String::new(),
                None,
            ),
        };

    let bind_mismatch_warning = match (active_workspace_data_dir, &data_bind_host_path) {
        (Some(active), Some(bind)) if paths_differ(active, bind) => Some(format!(
            "Container is bound to a different branch's data: {} (current branch uses {}). \
             Stop and start the container to use the current branch's data.",
            bind, active
        )),
        _ => None,
    };

    let compute_status = Some(ComputeStatus {
        provider: provider_name.to_string(),
        version,
        container_status,
        container_id,
        connection_string,
        data_bind_host_path,
    });

    (compute_status, bind_mismatch_warning)
}

fn paths_differ(a: &str, b: &str) -> bool {
    let a = std::path::Path::new(a);
    let b = std::path::Path::new(b);
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a != b,
        _ => a != b,
    }
}

async fn get_data_bind_host_path<R: DatabaseProviderRegistry>(
    compute: &dyn Compute,
    registry: &R,
    instance_id: &InstanceId,
    provider_name: &str,
) -> Option<String> {
    let provider = registry.get(provider_name)?;
    let compute_data_path = provider
        .definition()
        .data_dir
        .to_string_lossy()
        .into_owned();
    compute
        .get_instance_data_mount_host_path(instance_id, &compute_data_path)
        .await
        .ok()
        .flatten()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Build connection string using DatabaseProvider from the registry and connection info from Compute.
async fn build_connection_string<R: DatabaseProviderRegistry>(
    compute: &dyn Compute,
    registry: &R,
    instance_id: &InstanceId,
    provider_name: &str,
) -> String {
    let provider = match registry.get(provider_name) {
        Some(p) => p,
        None => return String::new(),
    };
    let compute_port = provider.default_port();
    let info = match compute.get_connection_info(instance_id, compute_port).await {
        Ok(i) => i,
        Err(_) => return String::new(),
    };
    let params = ConnectionParams {
        host: info.host,
        port: info.port,
        env: info.env,
    };
    provider.connection_string(&params).unwrap_or_default()
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
        current_branch: String,
        environment: Option<EnvironmentConfig>,
        runtime: Option<RuntimeConfig>,
        active_workspace_data_dir: Option<PathBuf>,
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
            Ok(self.current_branch.clone())
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
            Ok(self.runtime.clone())
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
            Ok(self.environment.clone())
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
            Ok(self
                .active_workspace_data_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("/workspace/data")))
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
    async fn status_no_compute() {
        let repo = MockRepository {
            current_branch: "main".into(),
            environment: None,
            runtime: None,
            active_workspace_data_dir: None,
        };
        let usecase = StatusRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockCompute),
            Arc::new(MockRegistry),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase.run(dir.path()).await;
        assert!(result.is_ok());
        let status = result.unwrap();
        assert_eq!(status.current_branch, "main");
        assert!(status.compute.is_none());
    }

    #[tokio::test]
    async fn status_with_compute() {
        let repo = MockRepository {
            current_branch: "main".into(),
            environment: Some(EnvironmentConfig {
                database_provider: "postgres".into(),
                database_version: "17".into(),
            }),
            runtime: Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "container-1".into(),
            }),
            active_workspace_data_dir: Some(PathBuf::from("/workspace/data")),
        };
        let usecase = StatusRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockCompute),
            Arc::new(MockRegistry),
        );
        let dir = tempfile::tempdir().unwrap();
        let result = usecase.run(dir.path()).await;
        assert!(result.is_ok());
        let status = result.unwrap();
        assert_eq!(status.current_branch, "main");
        let compute = status.compute.unwrap();
        assert_eq!(compute.provider, "postgres");
        assert_eq!(compute.version, "17");
        assert_eq!(compute.container_id, "container-1");
        assert_eq!(compute.container_status, "running");
    }
}
