//! Execute SQL against a running database instance via compute exec (no host clients).

use std::path::Path;
use std::sync::Arc;

use thiserror::Error;

use crate::model::config::GfsConfig;
use crate::ports::compute::{Compute, ExecOutput, InstanceId};
use crate::ports::database_provider::DatabaseProviderRegistry;

#[derive(Debug, Error)]
pub enum ExecuteQueryError {
    #[error("config: {0}")]
    Config(String),

    #[error("not configured: {0}")]
    NotConfigured(String),

    #[error("provider not found: {0}")]
    ProviderNotFound(String),

    #[error("query not supported: {0}")]
    Unsupported(String),

    #[error("compute: {0}")]
    Compute(String),

    #[error("query failed (exit {exit_code}): {message}")]
    QueryFailed { exit_code: i32, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteQueryOutput {
    pub stdout: String,
}

pub struct ExecuteQueryUseCase<R: DatabaseProviderRegistry> {
    compute: Arc<dyn Compute>,
    registry: Arc<R>,
}

impl<R: DatabaseProviderRegistry> ExecuteQueryUseCase<R> {
    pub fn new(compute: Arc<dyn Compute>, registry: Arc<R>) -> Self {
        Self { compute, registry }
    }

    /// Run `sql` inside the database container/pod for the repo at `path`.
    pub async fn run(
        &self,
        path: &Path,
        sql: &str,
    ) -> Result<ExecuteQueryOutput, ExecuteQueryError> {
        if sql.trim().is_empty() {
            return Err(ExecuteQueryError::Unsupported(
                "empty SQL is not supported via repo.query".into(),
            ));
        }

        let config = GfsConfig::load(path).map_err(|e| ExecuteQueryError::Config(e.to_string()))?;

        let provider_name = config
            .environment
            .as_ref()
            .map(|e| e.database_provider.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ExecuteQueryError::NotConfigured(
                    "no database provider configured (run gfs init)".into(),
                )
            })?
            .to_string();

        let container_name = config
            .runtime
            .as_ref()
            .map(|r| r.container_name.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ExecuteQueryError::NotConfigured(
                    "no container configured (run gfs compute start)".into(),
                )
            })?
            .to_string();

        let provider = self
            .registry
            .get(&provider_name)
            .ok_or_else(|| ExecuteQueryError::ProviderNotFound(provider_name.clone()))?;

        let command = provider
            .query_in_instance_command(sql)
            .map_err(|e| ExecuteQueryError::Unsupported(e.to_string()))?;

        let instance_id = InstanceId(container_name);
        let output = self
            .compute
            .exec(&instance_id, &command, None)
            .await
            .map_err(|e| ExecuteQueryError::Compute(e.to_string()))?;

        map_exec_output(output)
    }
}

fn map_exec_output(output: ExecOutput) -> Result<ExecuteQueryOutput, ExecuteQueryError> {
    if output.exit_code != 0 {
        let message = if output.stderr.trim().is_empty() {
            output.stdout.trim().to_string()
        } else {
            output.stderr.trim().to_string()
        };
        return Err(ExecuteQueryError::QueryFailed {
            exit_code: output.exit_code,
            message,
        });
    }
    Ok(ExecuteQueryOutput {
        stdout: output.stdout,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::model::config::{EnvironmentConfig, GfsConfig, RuntimeConfig};
    use crate::ports::compute::{
        ComputeCapabilities, ComputeDefinition, ExecOutput, InstanceConnectionInfo, InstanceState,
        InstanceStatus, LogsOptions, PortMapping, StartOptions,
    };
    use crate::ports::database_provider::{
        ConnectionParams, DatabaseProvider, DatabaseProviderRegistry,
        InMemoryDatabaseProviderRegistry, ProviderError, Result as RegistryResult, SIGTERM,
        SupportedFeature,
    };
    use tempfile::TempDir;

    #[derive(Default)]
    struct QueryMockCompute {
        last_command: Mutex<Option<String>>,
        stdout: String,
        exit_code: i32,
        stderr: String,
    }

    #[async_trait]
    impl Compute for QueryMockCompute {
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
            _: LogsOptions,
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
            _: &InstanceId,
            port: u16,
        ) -> crate::ports::compute::Result<InstanceConnectionInfo> {
            Ok(InstanceConnectionInfo {
                host: "127.0.0.1".into(),
                port,
                env: vec![],
            })
        }
        async fn get_instance_data_mount_host_path(
            &self,
            _: &InstanceId,
            _: &str,
        ) -> crate::ports::compute::Result<Option<PathBuf>> {
            Ok(None)
        }
        async fn remove_instance(&self, _: &InstanceId) -> crate::ports::compute::Result<()> {
            Ok(())
        }
        async fn get_task_connection_info(
            &self,
            _: &InstanceId,
            port: u16,
        ) -> crate::ports::compute::Result<InstanceConnectionInfo> {
            Ok(InstanceConnectionInfo {
                host: "127.0.0.1".into(),
                port,
                env: vec![],
            })
        }
        async fn run_task(
            &self,
            _: &ComputeDefinition,
            _: &str,
            _: Option<&InstanceId>,
        ) -> crate::ports::compute::Result<ExecOutput> {
            Ok(ExecOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        async fn capabilities(&self) -> crate::ports::compute::Result<ComputeCapabilities> {
            Ok(ComputeCapabilities {
                supports_stream_snapshot: false,
                supports_exec_as_root: true,
                db_live_during_snapshot: false,
            })
        }
        async fn exec(
            &self,
            _: &InstanceId,
            command: &str,
            _: Option<&str>,
        ) -> crate::ports::compute::Result<ExecOutput> {
            *self.last_command.lock().unwrap() = Some(command.to_string());
            Ok(ExecOutput {
                exit_code: self.exit_code,
                stdout: self.stdout.clone(),
                stderr: self.stderr.clone(),
            })
        }
    }

    struct MockQueryProvider;

    impl DatabaseProvider for MockQueryProvider {
        fn name(&self) -> &str {
            "mock-query"
        }
        fn definition(&self) -> ComputeDefinition {
            ComputeDefinition {
                labels: Default::default(),
                image: "mock:latest".into(),
                env: vec![],
                ports: vec![PortMapping {
                    compute_port: 5432,
                    host_port: None,
                }],
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
        fn default_args(&self) -> Vec<crate::ports::database_provider::DatabaseProviderArg> {
            vec![]
        }
        fn default_signal(&self) -> u32 {
            SIGTERM
        }
        fn connection_string(
            &self,
            _: &ConnectionParams,
        ) -> std::result::Result<String, ProviderError> {
            Ok("mock://localhost".into())
        }
        fn supported_versions(&self) -> Vec<String> {
            vec!["latest".into()]
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
        fn query_in_instance_command(
            &self,
            sql: &str,
        ) -> std::result::Result<String, ProviderError> {
            Ok(format!("mock-exec-query: {sql}"))
        }
    }

    fn repo_with_config(provider: &str, container: &str) -> (TempDir, PathBuf) {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().to_path_buf();
        std::fs::create_dir_all(path.join(".gfs")).expect("create .gfs");
        let config = GfsConfig {
            mount_point: None,
            version: String::new(),
            description: String::new(),
            user: None,
            environment: Some(EnvironmentConfig {
                database_provider: provider.into(),
                database_version: "17".into(),
                database_port: None,
                display_name: None,
            }),
            runtime: Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "latest".into(),
                container_name: container.into(),
            }),
            storage: None,
            compute: None,
            remote: None,
        };
        config.save(&path).expect("save config");
        (temp, path)
    }

    #[tokio::test]
    async fn execute_query_runs_in_compute_not_host() {
        let (_temp, repo_path) = repo_with_config("mock-query", "pg-test-1");

        let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
        registry.register(Arc::new(MockQueryProvider)).unwrap();

        let compute = Arc::new(QueryMockCompute {
            stdout: " ?column? \n----------\n        1\n(1 row)\n".into(),
            exit_code: 0,
            ..Default::default()
        });

        let uc = ExecuteQueryUseCase::new(compute.clone(), registry);
        let out = uc.run(&repo_path, "SELECT 1").await.unwrap();
        assert!(out.stdout.contains("(1 row)"));

        let cmd = compute.last_command.lock().unwrap().clone().unwrap();
        assert!(cmd.contains("mock-exec-query: SELECT 1"));
        assert!(!cmd.contains("Command::new"));
    }

    #[tokio::test]
    async fn execute_query_surfaces_stderr_on_failure() {
        let (_temp, repo_path) = repo_with_config("mock-query", "pg-test-2");

        let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
        registry.register(Arc::new(MockQueryProvider)).unwrap();

        let compute = Arc::new(QueryMockCompute {
            exit_code: 1,
            stderr: "syntax error at line 1".into(),
            ..Default::default()
        });

        let uc = ExecuteQueryUseCase::new(compute, registry);
        let err = uc.run(&repo_path, "BAD SQL").await.unwrap_err();
        match err {
            ExecuteQueryError::QueryFailed { message, .. } => {
                assert!(message.contains("syntax error"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
