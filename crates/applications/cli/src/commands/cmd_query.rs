//! `gfs query` — query the database using native client (psql, mysql, etc.).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_compute_docker::DockerCompute;
use gfs_domain::model::config::GfsConfig;
use gfs_domain::ports::compute::{Compute, InstanceId};
use gfs_domain::ports::database_provider::{
    ConnectionParams, DatabaseProviderRegistry, InMemoryDatabaseProviderRegistry,
};

use crate::cli_utils::get_repo_dir;

/// Execute a SQL query against the running database instance.
///
/// If `query` is `None`, opens an interactive terminal session.
/// Otherwise, executes the query and prints the results.
///
/// The `database` parameter allows overriding the default database name
/// from the container configuration.
pub async fn run(
    path: Option<PathBuf>,
    database: Option<String>,
    query: Option<String>,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);

    // Load config to get provider name and container name
    let config =
        GfsConfig::load(&repo_path).context("not a GFS repository (run gfs init first)")?;

    let environment = config
        .environment
        .as_ref()
        .context("no database configured (run gfs init with --database-provider)")?;

    let runtime = config
        .runtime
        .as_ref()
        .context("no runtime configured (run gfs init with --database-provider)")?;

    let provider_name = &environment.database_provider;
    let container_name = &runtime.container_name;

    // Set up compute and registry
    let compute = Arc::new(DockerCompute::new().context(
        "failed to connect to Docker/Podman daemon (is your container runtime running?)",
    )?);

    let registry_impl = InMemoryDatabaseProviderRegistry::new();
    gfs_compute_docker::containers::register_all(&registry_impl)
        .context("failed to register database providers")?;
    let registry: Arc<dyn DatabaseProviderRegistry> = Arc::new(registry_impl);

    // Get the provider
    let provider = registry
        .get(provider_name)
        .with_context(|| format!("unknown database provider: '{}'", provider_name))?;

    // Get connection info from the running container
    let instance_id = InstanceId(container_name.clone());
    let default_port = provider.default_port();

    let conn_info = compute
        .get_connection_info(&instance_id, default_port)
        .await
        .context(
            "failed to get connection info (is the database running? try 'gfs compute start')",
        )?;

    // Override database name if --database flag is provided
    let mut env = conn_info.env;
    if let Some(db_name) = database {
        // Determine the database environment variable based on provider
        let db_env_var = match provider_name.as_str() {
            "postgres" => "POSTGRES_DB",
            "mysql" => "MYSQL_DATABASE",
            _ => "DATABASE", // fallback for future providers
        };

        // Remove existing database env var and add the override
        env.retain(|(k, _)| k != db_env_var);
        env.push((db_env_var.to_string(), db_name));
    }

    let params = ConnectionParams {
        host: conn_info.host,
        port: conn_info.port,
        env,
    };

    // Build the query command
    let mut cmd = provider
        .query_client_command(&params, query.as_deref())
        .context("failed to build query command")?;

    // Execute the command (let the OS handle "command not found" errors)
    let status = cmd.status().or_else(|e| {
        let client_name = cmd.get_program().to_string_lossy();
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::bail!(
                "database client '{}' not found. Install it to use 'gfs query'.\n  \
                 - PostgreSQL: install postgresql client tools (psql)\n  \
                 - MySQL: install mysql client tools",
                client_name
            )
        } else {
            Err(e).with_context(|| format!("failed to execute '{}'", client_name))
        }
    })?;

    // Exit with the same code as the native client
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}
