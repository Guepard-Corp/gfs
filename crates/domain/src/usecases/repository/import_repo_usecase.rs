//! Use case for importing data into a running database instance via the sidecar pattern.
//!
//! Orchestration:
//! 1. Load repo config to get the provider name and container name.
//! 2. Resolve the provider from the registry.
//! 3. Call `Compute::get_task_connection_info` to get the internal host:port the sidecar uses.
//! 4. Call `DatabaseProvider::import_spec` to get the sidecar definition and shell command.
//! 5. Set `host_data_dir` on the sidecar definition to the directory containing the import file.
//! 6. Call `Compute::run_task` (linked to the DB instance) to run the import sidecar.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::model::config::GfsConfig;
use crate::ports::compute::{Compute, ComputeError, InstanceId};
use crate::ports::database_provider::{ConnectionParams, DatabaseProviderRegistry};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ImportRepoError {
    #[error("repository not configured for compute: {0}")]
    NotConfigured(String),

    #[error("database provider not found: '{0}'")]
    ProviderNotFound(String),

    #[error("unsupported import format: '{0}'")]
    UnsupportedFormat(String),

    #[error("compute error: {0}")]
    Compute(#[from] ComputeError),

    #[error("import task failed (exit {exit_code}): {stderr}")]
    TaskFailed { exit_code: i32, stderr: String },

    #[error("config error: {0}")]
    Config(String),

    #[error("input file not found: {0}")]
    FileNotFound(String),
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Result of a successful import operation.
pub struct ImportOutput {
    /// Absolute host path of the file that was imported.
    pub imported_from: PathBuf,
    /// Format used for the import.
    pub format: String,
    /// Stdout captured from the import sidecar.
    pub stdout: String,
    /// Stderr captured from the import sidecar.
    pub stderr: String,
}

// ---------------------------------------------------------------------------
// Format detection
// ---------------------------------------------------------------------------

/// Infer a format identifier from a file extension.
/// Returns `None` when the extension is not recognised.
pub fn format_from_extension(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("sql") => Some("sql"),
        Some("dump") => Some("custom"),
        Some("csv") => Some("csv"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Use case
// ---------------------------------------------------------------------------

pub struct ImportRepoUseCase<R: DatabaseProviderRegistry> {
    compute: Arc<dyn Compute>,
    registry: Arc<R>,
}

impl<R: DatabaseProviderRegistry> ImportRepoUseCase<R> {
    pub fn new(compute: Arc<dyn Compute>, registry: Arc<R>) -> Self {
        Self { compute, registry }
    }

    /// Import data into the database instance associated with the repo at `path`.
    ///
    /// - `path`: GFS repository root.
    /// - `input_file`: absolute host path to the dump file to import.
    /// - `format`: import format identifier; if empty, inferred from the file extension.
    pub async fn run(
        &self,
        path: &Path,
        input_file: PathBuf,
        format: &str,
    ) -> Result<ImportOutput, ImportRepoError> {
        // Resolve absolute path.
        let input_file = if input_file.is_absolute() {
            input_file
        } else {
            path.join(&input_file)
        };

        if !input_file.exists() {
            return Err(ImportRepoError::FileNotFound(
                input_file.display().to_string(),
            ));
        }

        // Determine format (explicit or inferred from extension).
        let resolved_format = if format.is_empty() {
            format_from_extension(&input_file)
                .ok_or_else(|| {
                    ImportRepoError::UnsupportedFormat(
                        "cannot infer format from file extension; pass --format explicitly".into(),
                    )
                })?
                .to_string()
        } else {
            format.to_string()
        };

        // 1. Load repo config.
        let config = GfsConfig::load(path).map_err(|e| ImportRepoError::Config(e.to_string()))?;

        let provider_name = config
            .environment
            .as_ref()
            .map(|e| e.database_provider.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ImportRepoError::NotConfigured(
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
                ImportRepoError::NotConfigured(
                    "no container configured (run gfs compute start)".into(),
                )
            })?
            .to_string();

        // 2. Resolve provider.
        let provider = self
            .registry
            .get(&provider_name)
            .ok_or_else(|| ImportRepoError::ProviderNotFound(provider_name.clone()))?;

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

        let input_filename = input_file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("import.sql");

        // 4. Build the import spec.
        let mut spec = provider
            .import_spec(&params, &resolved_format, input_filename)
            .map_err(|e| ImportRepoError::UnsupportedFormat(e.to_string()))?;

        // 5. Mount the directory containing the import file into the sidecar.
        let host_data_dir = input_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        spec.definition.host_data_dir = Some(host_data_dir);

        // 6. Run the import sidecar linked to the database instance.
        let output = self
            .compute
            .run_task(&spec.definition, &spec.command, Some(&instance_id))
            .await?;

        // 7. Check exit code.
        if output.exit_code != 0 {
            return Err(ImportRepoError::TaskFailed {
                exit_code: output.exit_code,
                stderr: output.stderr,
            });
        }

        Ok(ImportOutput {
            imported_from: input_file,
            format: resolved_format,
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
        ImportSpec, ProviderError, Result as RegistryResult, SIGTERM, SupportedFeature,
    };

    #[test]
    fn format_from_extension_sql() {
        assert_eq!(format_from_extension(Path::new("dump.sql")), Some("sql"));
    }

    #[test]
    fn format_from_extension_dump() {
        assert_eq!(
            format_from_extension(Path::new("backup.dump")),
            Some("custom")
        );
    }

    #[test]
    fn format_from_extension_csv() {
        assert_eq!(format_from_extension(Path::new("data.csv")), Some("csv"));
    }

    #[test]
    fn format_from_extension_unknown() {
        assert_eq!(format_from_extension(Path::new("file.txt")), None);
        assert_eq!(format_from_extension(Path::new("noext")), None);
    }

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
        fn import_spec(
            &self,
            _: &ConnectionParams,
            format: &str,
            input_filename: &str,
        ) -> std::result::Result<ImportSpec, ProviderError> {
            if format == "sql" {
                Ok(ImportSpec {
                    definition: ComputeDefinition {
                        image: "postgres:17".into(),
                        env: vec![],
                        ports: vec![],
                        data_dir: PathBuf::from("/input"),
                        host_data_dir: None,
                        logs_dir: None,
                        conf_dir: None,
                        args: vec![],
                    },
                    command: "psql".into(),
                    input_filename: input_filename.to_string(),
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
    async fn import_success() {
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

        let input_file = dir.path().join("data.sql");
        std::fs::write(&input_file, "SELECT 1;").unwrap();

        let usecase = ImportRepoUseCase::new(
            Arc::new(MockCompute { exit_code: 0 }),
            Arc::new(MockRegistry),
        );
        let result = usecase.run(dir.path(), input_file.clone(), "sql").await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output.format, "sql");
        assert_eq!(output.imported_from, input_file);
    }

    #[tokio::test]
    async fn import_infers_format_from_extension() {
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

        let input_file = dir.path().join("dump.sql");
        std::fs::write(&input_file, "SELECT 1;").unwrap();

        let usecase = ImportRepoUseCase::new(
            Arc::new(MockCompute { exit_code: 0 }),
            Arc::new(MockRegistry),
        );
        let result = usecase.run(dir.path(), input_file, "").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().format, "sql");
    }

    #[tokio::test]
    async fn import_file_not_found() {
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

        let usecase = ImportRepoUseCase::new(
            Arc::new(MockCompute { exit_code: 0 }),
            Arc::new(MockRegistry),
        );
        let result = usecase
            .run(dir.path(), dir.path().join("nonexistent.sql"), "sql")
            .await;
        assert!(matches!(result, Err(ImportRepoError::FileNotFound(_))));
    }

    #[tokio::test]
    async fn import_unsupported_format_no_extension() {
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

        let input_file = dir.path().join("data");
        std::fs::write(&input_file, "data").unwrap();

        let usecase = ImportRepoUseCase::new(
            Arc::new(MockCompute { exit_code: 0 }),
            Arc::new(MockRegistry),
        );
        let result = usecase.run(dir.path(), input_file, "").await;
        assert!(matches!(result, Err(ImportRepoError::UnsupportedFormat(_))));
    }
}
