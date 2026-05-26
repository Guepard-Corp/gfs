//! Use case for bootstrapping a lazy (copy-on-read) clone of a remote database.
//!
//! Unlike `import`, this copies **no data** up front. It sets up a
//! foreign-data-wrapper link plus mixed-partition tables in the already
//! provisioned local GFS database, so that data is fetched from the remote on
//! first read and served locally thereafter. See `docs/rfcs/008-remote-clone.md`.
//!
//! Orchestration (mirrors the export/import sidecar pattern):
//! 1. Load repo config to get the provider name and container name.
//! 2. Resolve the provider from the registry.
//! 3. Get the internal connection info the sidecar uses to reach the LOCAL db.
//! 4. Ask the provider for a `clone_bootstrap_spec` (sidecar definition + command).
//! 5. Run the bootstrap sidecar linked to the local database instance.

use std::path::Path;
use std::sync::Arc;

use crate::model::config::GfsConfig;
use crate::ports::compute::{Compute, ComputeError, EnvVar, InstanceId};
use crate::ports::database_provider::{
    ConnectionParams, DatabaseProviderRegistry, RemoteSource,
};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum CloneRepoError {
    #[error("repository not configured for compute: {0}")]
    NotConfigured(String),

    #[error("database provider not found: '{0}'")]
    ProviderNotFound(String),

    #[error("provider does not support lazy clone: {0}")]
    Unsupported(String),

    #[error(transparent)]
    Compute(#[from] ComputeError),

    #[error("clone bootstrap failed (exit {exit_code}): {stderr}")]
    TaskFailed { exit_code: i32, stderr: String },

    #[error("config error: {0}")]
    Config(String),
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Result of a successful clone bootstrap.
pub struct CloneOutput {
    /// Remote host:port that was cloned from.
    pub remote: String,
    /// Stdout captured from the bootstrap sidecar.
    pub stdout: String,
    /// Stderr captured from the bootstrap sidecar.
    pub stderr: String,
}

// ---------------------------------------------------------------------------
// Use case
// ---------------------------------------------------------------------------

pub struct CloneRepoUseCase<R: DatabaseProviderRegistry> {
    compute: Arc<dyn Compute>,
    registry: Arc<R>,
}

impl<R: DatabaseProviderRegistry> CloneRepoUseCase<R> {
    pub fn new(compute: Arc<dyn Compute>, registry: Arc<R>) -> Self {
        Self { compute, registry }
    }

    /// Probe the remote PostgreSQL for its **major** version (e.g. `"16"`), so
    /// the clone can provision a matching local engine instead of a default.
    ///
    /// Runs a one-off sidecar `psql ... SHOW server_version_num` against the
    /// remote (password via `PGPASSWORD`, never on the command line).
    pub async fn detect_remote_version(
        &self,
        remote: &RemoteSource,
    ) -> Result<String, CloneRepoError> {
        let provider = self
            .registry
            .get("postgres")
            .ok_or_else(|| CloneRepoError::ProviderNotFound("postgres".into()))?;

        let mut def = provider.definition();
        def.env = vec![EnvVar {
            name: "PGPASSWORD".into(),
            default: Some(remote.password.clone()),
        }];
        def.ports = vec![];
        def.host_data_dir = None;
        def.user = None;

        let cmd = format!(
            "psql -h {} -p {} -U {} -d {} -tAc 'SHOW server_version_num'",
            remote.host, remote.port, remote.user, remote.dbname,
        );

        let out = self.compute.run_task(&def, &cmd, None).await?;
        if out.exit_code != 0 {
            return Err(CloneRepoError::Config(format!(
                "remote version probe failed (exit {}): {}",
                out.exit_code,
                out.stderr.trim()
            )));
        }
        let num: u32 = out.stdout.trim().parse().map_err(|_| {
            CloneRepoError::Config(format!(
                "unexpected server_version_num from remote: '{}'",
                out.stdout.trim()
            ))
        })?;
        Ok((num / 10000).to_string())
    }

    /// Bootstrap a lazy clone of `remote` into the local GFS database at `path`.
    ///
    /// The local repository must already be initialised and its database
    /// container running (e.g. via `gfs init --database-provider postgres ...`).
    pub async fn run(
        &self,
        path: &Path,
        remote: RemoteSource,
    ) -> Result<CloneOutput, CloneRepoError> {
        // 1. Load repo config.
        let config = GfsConfig::load(path).map_err(|e| CloneRepoError::Config(e.to_string()))?;

        let provider_name = config
            .environment
            .as_ref()
            .map(|e| e.database_provider.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                CloneRepoError::NotConfigured(
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
                CloneRepoError::NotConfigured(
                    "no container configured (run gfs compute start)".into(),
                )
            })?
            .to_string();

        // 2. Resolve provider.
        let provider = self
            .registry
            .get(&provider_name)
            .ok_or_else(|| CloneRepoError::ProviderNotFound(provider_name.clone()))?;

        let instance_id = InstanceId(container_name);

        // 3. Internal connection info for the sidecar → the LOCAL database.
        let conn_info = self
            .compute
            .get_task_connection_info(&instance_id, provider.default_port())
            .await?;

        let local = ConnectionParams {
            host: conn_info.host,
            port: conn_info.port,
            env: conn_info.env,
        };

        let remote_label = format!("{}:{}", remote.host, remote.port);

        // 4. Build the bootstrap spec.
        let spec = provider
            .clone_bootstrap_spec(&local, &remote)
            .map_err(|e| CloneRepoError::Unsupported(e.to_string()))?;

        // 5. Run the bootstrap sidecar linked to the local database instance.
        let output = self
            .compute
            .run_task(&spec.definition, &spec.command, Some(&instance_id))
            .await?;

        if output.exit_code != 0 {
            return Err(CloneRepoError::TaskFailed {
                exit_code: output.exit_code,
                stderr: output.stderr,
            });
        }

        Ok(CloneOutput {
            remote: remote_label,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}
