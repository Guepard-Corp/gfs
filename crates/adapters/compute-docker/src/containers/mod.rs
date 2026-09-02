//! Predefined container definitions. Each submodule registers a [`ComputeDefinition`]
//! with a [`DatabaseProviderRegistry`] so instances can be provisioned by name.

pub mod clickhouse;
pub mod mysql;
pub mod postgresql;
pub mod sqlite;

use gfs_domain::ports::database_provider::{DatabaseProviderRegistry, Result};

/// Registers all built-in database providers (e.g. postgres, mysql, clickhouse, sqlite) into `registry`.
/// Call this before looking up definitions by provider name.
pub fn register_all(registry: &impl DatabaseProviderRegistry) -> Result<()> {
    postgresql::register(registry)?;
    mysql::register(registry)?;
    clickhouse::register(registry)?;
    sqlite::register(registry)?;
    Ok(())
}
