//! Manage database users/roles inside a running instance via compute exec.
//!
//! Mirrors [`super::execute_query_usecase::ExecuteQueryUseCase`]: resolve the
//! provider + container from `.gfs` config, ask the provider to build the
//! in-instance role command, run it via [`Compute::exec`], and map the output.
//! No host-side DB client is used.

use std::path::Path;
use std::sync::Arc;

use thiserror::Error;

use crate::model::config::GfsConfig;
use crate::model::db_user::{RoleInfo, RolePreset, RoleSpec};
use crate::ports::compute::{Compute, ExecOutput, InstanceId};
use crate::ports::database_provider::{DatabaseProvider, DatabaseProviderRegistry};

#[derive(Debug, Error)]
pub enum ManageUsersError {
    #[error("config: {0}")]
    Config(String),

    #[error("not configured: {0}")]
    NotConfigured(String),

    #[error("provider not found: {0}")]
    ProviderNotFound(String),

    #[error("user management not supported by provider: {0}")]
    Unsupported(String),

    #[error("compute: {0}")]
    Compute(String),

    #[error("operation failed (exit {exit_code}): {message}")]
    Failed { exit_code: i32, message: String },

    #[error("could not parse role list: {0}")]
    Parse(String),
}

pub struct ManageUsersUseCase<R: DatabaseProviderRegistry> {
    compute: Arc<dyn Compute>,
    registry: Arc<R>,
}

impl<R: DatabaseProviderRegistry> ManageUsersUseCase<R> {
    pub fn new(compute: Arc<dyn Compute>, registry: Arc<R>) -> Self {
        Self { compute, registry }
    }

    /// Resolve `(provider, container_name)` from the repo's `.gfs` config.
    fn resolve(
        &self,
        path: &Path,
    ) -> Result<(Arc<dyn DatabaseProvider>, String), ManageUsersError> {
        let config = GfsConfig::load(path).map_err(|e| ManageUsersError::Config(e.to_string()))?;

        let provider_name = config
            .environment
            .as_ref()
            .map(|e| e.database_provider.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ManageUsersError::NotConfigured(
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
                ManageUsersError::NotConfigured(
                    "no container configured (run gfs compute start)".into(),
                )
            })?
            .to_string();

        let provider = self
            .registry
            .get(&provider_name)
            .ok_or_else(|| ManageUsersError::ProviderNotFound(provider_name.clone()))?;

        Ok((provider, container_name))
    }

    /// Run an already-built in-instance command and return its output.
    async fn run(&self, container: &str, command: &str) -> Result<ExecOutput, ManageUsersError> {
        self.compute
            .exec(&InstanceId(container.to_string()), command, None)
            .await
            .map_err(|e| ManageUsersError::Compute(e.to_string()))
    }

    /// Create a login role (optionally with a preset).
    pub async fn create_role(&self, path: &Path, spec: &RoleSpec) -> Result<(), ManageUsersError> {
        let (provider, container) = self.resolve(path)?;
        let command = provider
            .create_role_command(spec)
            .map_err(|e| ManageUsersError::Unsupported(e.to_string()))?;
        expect_success(self.run(&container, &command).await?)
    }

    /// Set / rotate a role's password.
    pub async fn set_password(
        &self,
        path: &Path,
        username: &str,
        password: &str,
    ) -> Result<(), ManageUsersError> {
        let (provider, container) = self.resolve(path)?;
        let command = provider
            .alter_password_command(username, password)
            .map_err(|e| ManageUsersError::Unsupported(e.to_string()))?;
        expect_success(self.run(&container, &command).await?)
    }

    /// Drop a role.
    pub async fn drop_role(&self, path: &Path, username: &str) -> Result<(), ManageUsersError> {
        let (provider, container) = self.resolve(path)?;
        let command = provider
            .drop_role_command(username)
            .map_err(|e| ManageUsersError::Unsupported(e.to_string()))?;
        expect_success(self.run(&container, &command).await?)
    }

    /// Apply a role preset to an existing role.
    pub async fn apply_preset(
        &self,
        path: &Path,
        username: &str,
        preset: RolePreset,
    ) -> Result<(), ManageUsersError> {
        let (provider, container) = self.resolve(path)?;
        let command = provider
            .apply_preset_command(username, preset)
            .map_err(|e| ManageUsersError::Unsupported(e.to_string()))?;
        expect_success(self.run(&container, &command).await?)
    }

    /// List login roles (never a password).
    pub async fn list_roles(&self, path: &Path) -> Result<Vec<RoleInfo>, ManageUsersError> {
        let (provider, container) = self.resolve(path)?;
        let command = provider
            .list_roles_command()
            .map_err(|e| ManageUsersError::Unsupported(e.to_string()))?;
        let output = self.run(&container, &command).await?;
        if output.exit_code != 0 {
            return Err(fail(output));
        }
        serde_json::from_str(output.stdout.trim())
            .map_err(|e| ManageUsersError::Parse(e.to_string()))
    }
}

/// A mutating op succeeded iff the exit code is zero.
fn expect_success(output: ExecOutput) -> Result<(), ManageUsersError> {
    if output.exit_code == 0 {
        Ok(())
    } else {
        Err(fail(output))
    }
}

fn fail(output: ExecOutput) -> ManageUsersError {
    let message = if output.stderr.trim().is_empty() {
        output.stdout.trim().to_string()
    } else {
        output.stderr.trim().to_string()
    };
    ManageUsersError::Failed {
        exit_code: output.exit_code,
        message,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use tempfile::TempDir;

    use super::*;
    use crate::model::config::{EnvironmentConfig, RuntimeConfig};
    use crate::ports::compute::{
        ComputeCapabilities, ComputeDefinition, InstanceConnectionInfo, InstanceState,
        InstanceStatus, LogEntry, LogsOptions, PortMapping, StartOptions,
    };
    use crate::ports::database_provider::{
        ConnectionParams, DatabaseProvider, DatabaseProviderArg, InMemoryDatabaseProviderRegistry,
        ProviderError, Result as RegistryResult, SIGTERM, SupportedFeature,
    };

    /// Compute mock: records the last `exec` command and returns a canned output.
    #[derive(Default)]
    struct MockCompute {
        last_command: Mutex<Option<String>>,
        stdout: String,
        stderr: String,
        exit_code: i32,
    }

    type CResult<T> = crate::ports::compute::Result<T>;

    #[async_trait]
    impl Compute for MockCompute {
        async fn provision(&self, _: &ComputeDefinition) -> CResult<InstanceId> {
            Ok(InstanceId("mock".into()))
        }
        async fn start(&self, id: &InstanceId, _: StartOptions) -> CResult<InstanceStatus> {
            Ok(running(id))
        }
        async fn stop(&self, id: &InstanceId) -> CResult<InstanceStatus> {
            Ok(running(id))
        }
        async fn restart(&self, id: &InstanceId) -> CResult<InstanceStatus> {
            Ok(running(id))
        }
        async fn status(&self, id: &InstanceId) -> CResult<InstanceStatus> {
            Ok(running(id))
        }
        async fn prepare_for_snapshot(&self, _: &InstanceId, _: &[String]) -> CResult<()> {
            Ok(())
        }
        async fn logs(&self, _: &InstanceId, _: LogsOptions) -> CResult<Vec<LogEntry>> {
            Ok(vec![])
        }
        async fn pause(&self, id: &InstanceId) -> CResult<InstanceStatus> {
            Ok(running(id))
        }
        async fn unpause(&self, id: &InstanceId) -> CResult<InstanceStatus> {
            Ok(running(id))
        }
        async fn get_connection_info(
            &self,
            _: &InstanceId,
            port: u16,
        ) -> CResult<InstanceConnectionInfo> {
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
        ) -> CResult<Option<PathBuf>> {
            Ok(None)
        }
        async fn remove_instance(&self, _: &InstanceId) -> CResult<()> {
            Ok(())
        }
        async fn get_task_connection_info(
            &self,
            _: &InstanceId,
            port: u16,
        ) -> CResult<InstanceConnectionInfo> {
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
        ) -> CResult<ExecOutput> {
            Ok(ok_output())
        }
        async fn capabilities(&self) -> CResult<ComputeCapabilities> {
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
        ) -> CResult<ExecOutput> {
            *self.last_command.lock().unwrap() = Some(command.to_string());
            Ok(ExecOutput {
                exit_code: self.exit_code,
                stdout: self.stdout.clone(),
                stderr: self.stderr.clone(),
            })
        }
    }

    fn running(id: &InstanceId) -> InstanceStatus {
        InstanceStatus {
            id: id.clone(),
            state: InstanceState::Running,
            pid: None,
            started_at: None,
            exit_code: None,
        }
    }

    fn ok_output() -> ExecOutput {
        ExecOutput {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    /// Provider mock: role methods return marker commands; `support_users=false`
    /// makes them report the feature as unsupported.
    struct MockRoleProvider {
        support_users: bool,
    }

    impl MockRoleProvider {
        fn guard(&self) -> std::result::Result<(), ProviderError> {
            if self.support_users {
                Ok(())
            } else {
                Err(ProviderError::UnsupportedFormat("users".into()))
            }
        }
    }

    impl DatabaseProvider for MockRoleProvider {
        fn name(&self) -> &str {
            "mock-role"
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
        fn create_role_command(
            &self,
            spec: &RoleSpec,
        ) -> std::result::Result<String, ProviderError> {
            self.guard()?;
            Ok(format!("MOCK-CREATE:{}", spec.username))
        }
        fn alter_password_command(
            &self,
            username: &str,
            _: &str,
        ) -> std::result::Result<String, ProviderError> {
            self.guard()?;
            Ok(format!("MOCK-ALTER:{username}"))
        }
        fn drop_role_command(&self, username: &str) -> std::result::Result<String, ProviderError> {
            self.guard()?;
            Ok(format!("MOCK-DROP:{username}"))
        }
        fn list_roles_command(&self) -> std::result::Result<String, ProviderError> {
            self.guard()?;
            Ok("MOCK-LIST".into())
        }
        fn apply_preset_command(
            &self,
            username: &str,
            _: RolePreset,
        ) -> std::result::Result<String, ProviderError> {
            self.guard()?;
            Ok(format!("MOCK-PRESET:{username}"))
        }
    }

    fn repo_with_config(container: &str) -> (TempDir, PathBuf) {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().to_path_buf();
        std::fs::create_dir_all(path.join(".gfs")).expect("create .gfs");
        let config = GfsConfig {
            mount_point: None,
            version: String::new(),
            description: String::new(),
            user: None,
            environment: Some(EnvironmentConfig {
                database_provider: "mock-role".into(),
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

    fn use_case(
        compute: MockCompute,
        support_users: bool,
    ) -> (
        ManageUsersUseCase<InMemoryDatabaseProviderRegistry>,
        Arc<MockCompute>,
    ) {
        let compute = Arc::new(compute);
        let registry = InMemoryDatabaseProviderRegistry::new();
        registry
            .register(Arc::new(MockRoleProvider { support_users }))
            .unwrap();
        (
            ManageUsersUseCase::new(compute.clone(), Arc::new(registry)),
            compute,
        )
    }

    #[tokio::test]
    async fn create_role_execs_the_provider_command() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let (uc, compute) = use_case(MockCompute::default(), true);
        uc.create_role(
            &repo,
            &RoleSpec {
                username: "alice".into(),
                password: "pw".into(),
                preset: None,
            },
        )
        .await
        .expect("ok");
        assert_eq!(
            compute.last_command.lock().unwrap().clone(),
            Some("MOCK-CREATE:alice".into())
        );
    }

    #[tokio::test]
    async fn list_roles_parses_json() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let compute = MockCompute {
            stdout: r#"[{"username":"alice","can_login":true,"is_superuser":false}]"#.into(),
            ..Default::default()
        };
        let (uc, _c) = use_case(compute, true);
        let roles = uc.list_roles(&repo).await.expect("ok");
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].username, "alice");
        assert!(roles[0].can_login && !roles[0].is_superuser);
    }

    #[tokio::test]
    async fn non_zero_exit_is_failed_with_message() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let compute = MockCompute {
            exit_code: 1,
            stderr: "role \"alice\" already exists".into(),
            ..Default::default()
        };
        let (uc, _c) = use_case(compute, true);
        let err = uc
            .create_role(
                &repo,
                &RoleSpec {
                    username: "alice".into(),
                    password: "pw".into(),
                    preset: None,
                },
            )
            .await
            .unwrap_err();
        match err {
            ManageUsersError::Failed { exit_code, message } => {
                assert_eq!(exit_code, 1);
                assert!(message.contains("already exists"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unsupported_provider_maps_to_unsupported() {
        let (_temp, repo) = repo_with_config("pg-c1");
        let (uc, _c) = use_case(MockCompute::default(), /* support_users */ false);
        let err = uc.drop_role(&repo, "alice").await.unwrap_err();
        assert!(matches!(err, ManageUsersError::Unsupported(_)));
    }
}
