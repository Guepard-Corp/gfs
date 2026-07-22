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
