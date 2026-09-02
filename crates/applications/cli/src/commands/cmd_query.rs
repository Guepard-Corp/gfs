//! `gfs query` — query the database using native client (psql, mysql, etc.).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_db_providers as containers;
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::model::config::GfsConfig;
use gfs_domain::ports::compute::InstanceId;
use gfs_domain::ports::database_provider::{
    ConnectionParams, DatabaseProviderRegistry, InMemoryDatabaseProviderRegistry,
};
use gfs_domain::ports::repository::Repository;
use gfs_domain::usecases::repository::execute_query_usecase::ExecuteQueryUseCase;

use super::compute_support::compute_for_repo;
use gfs_domain::repo_utils::repo_layout;

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
    _json_output: bool,
) -> Result<()> {
    let repo_path = path.clone().unwrap_or_else(get_repo_dir);

    // Load config to get provider name and container name
    let config =
        GfsConfig::load(&repo_path).context("not a GFS repository (run gfs init first)")?;

    let environment = config
        .environment
        .as_ref()
        .context("no database configured (run gfs init with --database-provider)")?;

    let provider_name = &environment.database_provider;

    let registry_impl = InMemoryDatabaseProviderRegistry::new();
    containers::register_all(&registry_impl).context("failed to register database providers")?;
    let registry = Arc::new(registry_impl);

    // An embedded provider opens a file: there is no container to look up, no
    // connection info to fetch, and no runtime to require. `--database` has no
    // meaning either, since the file *is* the database.
    if let Some(provider) = registry.get(provider_name)
        && provider.local_engine().is_some()
    {
        // The file *is* the database, so there is nothing for `--database` to
        // select. Say so rather than accepting the flag and ignoring it.
        if database.is_some() {
            anyhow::bail!(
                "--database does not apply to the '{provider_name}' provider: an embedded \
                 database is a single file, and `gfs query` always uses the one in the \
                 active workspace"
            );
        }
        let params = repo_layout::local_connection_params(&repo_path)
            .context("failed to resolve the active workspace")?;
        return run_client(&*provider, &params, query.as_deref());
    }

    let runtime = config
        .runtime
        .as_ref()
        .context("no runtime configured (run gfs init with --database-provider)")?;
    let container_name = &runtime.container_name;

    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute = compute_for_repo(&repository, &repo_path).await?;

    let is_k8s = runtime
        .runtime_provider
        .trim()
        .eq_ignore_ascii_case("kubernetes");

    if is_k8s {
        let sql = query.as_deref().context(
            "interactive query is not supported for kubernetes runtime; pass SQL as an argument",
        )?;
        let out = ExecuteQueryUseCase::new(compute, registry.clone())
            .run(&repo_path, sql, database.as_deref())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        print!("{}", out.stdout);
        return Ok(());
    }

    let registry: Arc<dyn DatabaseProviderRegistry> = registry;

    // Get the provider
    let provider = registry
        .get(provider_name)
        .with_context(|| format!("unknown database provider: '{}'", provider_name))?;

    // Get connection info from the running container
    let instance_id = InstanceId(container_name.clone());
    let default_port = provider.require_container()?.default_port();

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
            "clickhouse" => "CLICKHOUSE_DB",
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

    run_client(&*provider, &params, query.as_deref())
}

/// Spawn the provider's native client, inheriting stdio so an interactive
/// session works, and exit with the client's own status code.
fn run_client(
    provider: &dyn gfs_domain::ports::database_provider::DatabaseProvider,
    params: &ConnectionParams,
    query: Option<&str>,
) -> Result<()> {
    let mut cmd = provider
        .query_client_command(params, query)
        .context("failed to build query command")?;

    // Let the OS report "command not found" so the hint below is accurate.
    let status = cmd.status().or_else(|e| {
        let client_name = cmd.get_program().to_string_lossy();
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::bail!(
                "database client '{}' not found. Install it to use 'gfs query'.\n  \
                 - PostgreSQL: install postgresql client tools (psql)\n  \
                 - MySQL: install mysql client tools\n  \
                 - ClickHouse: install clickhouse client tools (clickhouse-client)\n  \
                 - SQLite: install sqlite3 (only the interactive shell needs it; \
                   gfs itself uses a linked engine)",
                client_name
            )
        } else {
            Err(e).with_context(|| format!("failed to execute '{}'", client_name))
        }
    })?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
