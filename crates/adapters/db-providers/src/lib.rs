//! Database provider catalogue.
//!
//! A provider describes how GFS talks to one database engine: how to reach it,
//! what versions and features it supports, how to read its schema, and — for
//! engines that run as a server — how to provision and drive an instance of it.
//!
//! Providers are deliberately independent of any compute runtime. The same
//! catalogue serves the Docker and Kubernetes adapters, and the SQLite provider
//! serves no runtime at all: it is an embedded engine linked into this binary,
//! with no container to provision.

pub mod clickhouse;
pub mod mysql;
pub mod postgresql;
pub mod sqlite;

use gfs_domain::ports::database_provider::{DatabaseProviderRegistry, Result};

/// Registers every built-in database provider into `registry`.
///
/// Call this before looking a provider up by name.
pub fn register_all(registry: &impl DatabaseProviderRegistry) -> Result<()> {
    postgresql::register(registry)?;
    mysql::register(registry)?;
    clickhouse::register(registry)?;
    sqlite::register(registry)?;
    Ok(())
}
