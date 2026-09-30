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

#[cfg(test)]
mod tests {
    use super::*;
    use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;

    /// Every registered provider is either a server GFS provisions or a file it
    /// opens — exactly one, never both and never neither.
    ///
    /// The trait documents this ("Exactly one of this and `local_engine`
    /// returns `Some`") and nothing enforced it. Both accessors default to
    /// `None`, so a provider that implements neither gets
    /// `requires_compute() == false` — `init` takes the embedded path — while
    /// every embedded operation finds no engine and falls through to a
    /// container it does not have. That is the advertised-but-does-not-work
    /// shape this catalogue has produced before, and here it would be in the
    /// mechanism rather than in one provider.
    ///
    /// Asserted over the registry rather than per provider so a NEW provider is
    /// covered the day it is added, which is the only moment this can go wrong.
    #[test]
    fn every_provider_is_exactly_one_of_container_backed_or_embedded() {
        let registry = InMemoryDatabaseProviderRegistry::new();
        register_all(&registry).expect("register every built-in provider");

        let names = registry.list();
        assert!(!names.is_empty(), "the catalogue must not be empty");

        for name in names {
            let provider = registry.get(&name).expect("registered provider");
            let container = provider.container().is_some();
            let embedded = provider.local_engine().is_some();
            assert!(
                container ^ embedded,
                "provider '{name}' must be exactly one of container-backed or embedded, \
                 but container()={container} and local_engine()={embedded}"
            );
            assert_eq!(
                provider.requires_compute(),
                container,
                "provider '{name}' derives requires_compute() from container(); \
                 an override would let it claim to need no compute while offering \
                 no way to run without one"
            );
        }
    }
}
