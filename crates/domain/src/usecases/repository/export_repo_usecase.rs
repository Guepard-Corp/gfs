//! Use case for exporting data from a running database instance via the sidecar pattern.
//!
//! Orchestration:
//! 1. Load repo config to get the provider name and container name.
//! 2. Resolve the provider from the registry.
//! 3. Call `Compute::get_task_connection_info` to get the internal host:port the sidecar uses.
//! 4. Call `DatabaseProvider::export_spec` to get the sidecar definition and shell command.
//! 5. Set `host_data_dir` on the sidecar definition to the requested output directory.
//! 6. Call `Compute::run_task` (linked to the DB instance) to run the export sidecar.
//! 7. Return the path of the exported file on success.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::model::config::GfsConfig;
use crate::ports::compute::{Compute, ComputeError, InstanceId};
use crate::ports::database_provider::{ConnectionParams, DatabaseProviderRegistry};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ExportRepoError {
    #[error("repository not configured for compute: {0}")]
    NotConfigured(String),

    #[error("database provider not found: '{0}'")]
    ProviderNotFound(String),

    #[error("unsupported export format: '{0}'")]
    UnsupportedFormat(String),

    #[error("compute error: {0}")]
    Compute(#[from] ComputeError),

    #[error("export task failed (exit {exit_code}): {stderr}")]
    TaskFailed { exit_code: i32, stderr: String },

    #[error("config error: {0}")]
    Config(String),
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Result of a successful export operation.
pub struct ExportOutput {
    /// Absolute host path of the exported file.
    pub file_path: PathBuf,
    /// Format used for the export (e.g. `"sql"`, `"custom"`).
    pub format: String,
    /// Stdout captured from the export sidecar.
    pub stdout: String,
    /// Stderr captured from the export sidecar (non-empty on partial warnings).
    pub stderr: String,
}

// ---------------------------------------------------------------------------
// Use case
// ---------------------------------------------------------------------------

pub struct ExportRepoUseCase<R: DatabaseProviderRegistry> {
    compute: Arc<dyn Compute>,
    registry: Arc<R>,
}

impl<R: DatabaseProviderRegistry> ExportRepoUseCase<R> {
    pub fn new(compute: Arc<dyn Compute>, registry: Arc<R>) -> Self {
        Self { compute, registry }
    }

    /// Export data from the database instance associated with the repo at `path`.
    ///
    /// - `path`: GFS repository root.
    /// - `output_dir`: host directory where the export file will be written (created if absent).
    /// - `format`: export format identifier (e.g. `"sql"`, `"custom"`).
    pub async fn run(
        &self,
        path: &Path,
        output_dir: PathBuf,
        format: &str,
    ) -> Result<ExportOutput, ExportRepoError> {
        // 1. Load repo config.
        let config = GfsConfig::load(path).map_err(|e| ExportRepoError::Config(e.to_string()))?;

        let provider_name = config
            .environment
            .as_ref()
            .map(|e| e.database_provider.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ExportRepoError::NotConfigured(
                    "no database provider configured (run gfs init --database-provider <name>)"
                        .into(),
                )
            })?
            .to_string();

        let container_name = config
            .runtime
            .as_ref()
            .map(|r| r.container_name.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ExportRepoError::NotConfigured(
                    "no container configured (run gfs compute start)".into(),
                )
            })?
            .to_string();

        // 2. Resolve provider.
        let provider = self
            .registry
            .get(&provider_name)
            .ok_or_else(|| ExportRepoError::ProviderNotFound(provider_name.clone()))?;

        let instance_id = InstanceId(container_name);

        // 3. Get internal connection info for the sidecar.
        let conn_info = self
            .compute
            .get_task_connection_info(&instance_id, provider.default_port())
            .await?;

        let params = ConnectionParams {
            host: conn_info.host,
            port: conn_info.port,
            env: conn_info.env,
        };

        // 4. Build the export spec.
        let mut spec = provider
            .export_spec(&params, format)
            .map_err(|e| ExportRepoError::UnsupportedFormat(e.to_string()))?;

        // 5. Point the sidecar volume mount at the host output directory.
        std::fs::create_dir_all(&output_dir)
            .map_err(|e| ExportRepoError::Config(format!("cannot create output dir: {e}")))?;
        spec.definition.host_data_dir = Some(output_dir.clone());

        // 6. Run the export sidecar linked to the database instance.
        let output = self
            .compute
            .run_task(&spec.definition, &spec.command, Some(&instance_id))
            .await?;

        // 7. Check exit code.
        if output.exit_code != 0 {
            return Err(ExportRepoError::TaskFailed {
                exit_code: output.exit_code,
                stderr: output.stderr,
            });
        }

        Ok(ExportOutput {
            file_path: output_dir.join(&spec.output_filename),
            format: format.to_string(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
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
    use crate::model::layout::GFS_DIR;
    use crate::ports::compute::{
        Compute, ComputeDefinition, InstanceId, InstanceState, InstanceStatus, StartOptions,
    };
    use crate::ports::database_provider::{
        ConnectionParams, DatabaseProvider, DatabaseProviderArg, DatabaseProviderRegistry,
        ExportSpec, ProviderError, Result as RegistryResult, SIGTERM, SupportedFeature,
    };

    struct MockCompute {
        exit_code: i32,
    }

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
                exit_code: self.exit_code,
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
        fn export_spec(
            &self,
            _: &ConnectionParams,
            format: &str,
        ) -> std::result::Result<ExportSpec, ProviderError> {
            if format == "sql" {
                Ok(ExportSpec {
                    definition: ComputeDefinition {
                        image: "postgres:17".into(),
                        env: vec![],
                        ports: vec![],
                        data_dir: PathBuf::from("/output"),
                        host_data_dir: None,
                        logs_dir: None,
                        conf_dir: None,
                        args: vec![],
                    },
                    command: "pg_dump".into(),
                    output_filename: "export.sql".into(),
                })
            } else {
                Err(ProviderError::UnsupportedFormat(format.to_string()))
            }
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

    fn create_repo_with_config(
        dir: &std::path::Path,
        env: &EnvironmentConfig,
        runtime: &RuntimeConfig,
    ) {
        let gfs_dir = dir.join(GFS_DIR);
        std::fs::create_dir_all(&gfs_dir).unwrap();
        let config = GfsConfig {
            mount_point: None,
            version: String::new(),
            description: String::new(),
            user: None,
            environment: Some(env.clone()),
            runtime: Some(runtime.clone()),
        };
        config.save(dir).unwrap();
    }

    #[tokio::test]
    async fn export_success() {
        let dir = tempfile::tempdir().unwrap();
        let env = EnvironmentConfig {
            database_provider: "postgres".into(),
            database_version: "17".into(),
        };
        let runtime = RuntimeConfig {
            runtime_provider: "docker".into(),
            runtime_version: "24".into(),
            container_name: "container-1".into(),
        };
        create_repo_with_config(dir.path(), &env, &runtime);

        let usecase = ExportRepoUseCase::new(
            Arc::new(MockCompute { exit_code: 0 }),
            Arc::new(MockRegistry),
        );
        let output_dir = dir.path().join("export_out");
        let result = usecase.run(dir.path(), output_dir.clone(), "sql").await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output.format, "sql");
        assert!(output.file_path.ends_with("export.sql"));
    }

    #[tokio::test]
    async fn export_task_failed() {
        let dir = tempfile::tempdir().unwrap();
        let env = EnvironmentConfig {
            database_provider: "postgres".into(),
            database_version: "17".into(),
        };
        let runtime = RuntimeConfig {
            runtime_provider: "docker".into(),
            runtime_version: "24".into(),
            container_name: "container-1".into(),
        };
        create_repo_with_config(dir.path(), &env, &runtime);

        let usecase = ExportRepoUseCase::new(
            Arc::new(MockCompute { exit_code: 1 }),
            Arc::new(MockRegistry),
        );
        let output_dir = dir.path().join("export_out");
        let result = usecase.run(dir.path(), output_dir, "sql").await;
        assert!(matches!(result, Err(ExportRepoError::TaskFailed { .. })));
    }

    #[tokio::test]
    async fn export_not_configured_no_provider() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(GFS_DIR)).unwrap();
        let config = GfsConfig {
            mount_point: None,
            version: String::new(),
            description: String::new(),
            user: None,
            environment: None,
            runtime: Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "c1".into(),
            }),
        };
        config.save(dir.path()).unwrap();

        let usecase = ExportRepoUseCase::new(
            Arc::new(MockCompute { exit_code: 0 }),
            Arc::new(MockRegistry),
        );
        let result = usecase
            .run(dir.path(), dir.path().to_path_buf(), "sql")
            .await;
        assert!(matches!(result, Err(ExportRepoError::NotConfigured(_))));
    }

    #[tokio::test]
    async fn export_not_configured_no_container() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(GFS_DIR)).unwrap();
        let config = GfsConfig {
            mount_point: None,
            version: String::new(),
            description: String::new(),
            user: None,
            environment: Some(EnvironmentConfig {
                database_provider: "postgres".into(),
                database_version: "17".into(),
            }),
            runtime: Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "".into(),
            }),
        };
        config.save(dir.path()).unwrap();

        let usecase = ExportRepoUseCase::new(
            Arc::new(MockCompute { exit_code: 0 }),
            Arc::new(MockRegistry),
        );
        let result = usecase
            .run(dir.path(), dir.path().to_path_buf(), "sql")
            .await;
        assert!(matches!(result, Err(ExportRepoError::NotConfigured(_))));
    }

    #[tokio::test]
    async fn export_provider_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let env = EnvironmentConfig {
            database_provider: "mysql".into(),
            database_version: "8".into(),
        };
        let runtime = RuntimeConfig {
            runtime_provider: "docker".into(),
            runtime_version: "24".into(),
            container_name: "c1".into(),
        };
        create_repo_with_config(dir.path(), &env, &runtime);

        let usecase = ExportRepoUseCase::new(
            Arc::new(MockCompute { exit_code: 0 }),
            Arc::new(MockRegistry),
        );
        let result = usecase
            .run(dir.path(), dir.path().to_path_buf(), "sql")
            .await;
        assert!(matches!(result, Err(ExportRepoError::ProviderNotFound(_))));
    }

    #[tokio::test]
    async fn export_unsupported_format() {
        let dir = tempfile::tempdir().unwrap();
        let env = EnvironmentConfig {
            database_provider: "postgres".into(),
            database_version: "17".into(),
        };
        let runtime = RuntimeConfig {
            runtime_provider: "docker".into(),
            runtime_version: "24".into(),
            container_name: "c1".into(),
        };
        create_repo_with_config(dir.path(), &env, &runtime);

        let usecase = ExportRepoUseCase::new(
            Arc::new(MockCompute { exit_code: 0 }),
            Arc::new(MockRegistry),
        );
        let result = usecase
            .run(dir.path(), dir.path().to_path_buf(), "custom")
            .await;
        assert!(matches!(result, Err(ExportRepoError::UnsupportedFormat(_))));
    }
}
