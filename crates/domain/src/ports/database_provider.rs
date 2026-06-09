//! Registry of database **providers**. Each provider supplies a
//! [`ComputeDefinition`] and provider-specific behaviour (connection string,
//! name, version extraction, etc.).
//!
//! Use [`DatabaseProviderRegistry::register`] to add a provider, and
//! [`DatabaseProviderRegistry::get`] / [`DatabaseProviderRegistry::list`] to look them up.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

use crate::ports::compute::ComputeDefinition;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("definition already registered: '{0}'")]
    AlreadyRegistered(String),

    #[error("definition not found: '{0}'")]
    NotFound(String),

    #[error("internal error: {0}")]
    Internal(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("missing required env var for connection string: '{0}'")]
    MissingEnvVar(String),

    #[error("invalid connection params: {0}")]
    InvalidParams(String),

    #[error("unsupported format: '{0}'")]
    UnsupportedFormat(String),
}

pub type Result<T> = std::result::Result<T, RegistryError>;

// ---------------------------------------------------------------------------
// Connection params
// ---------------------------------------------------------------------------

/// Parameters used by a provider to build a client connection string.
/// `env` typically holds container environment (e.g. POSTGRES_USER, POSTGRES_PASSWORD).
#[derive(Debug, Clone, Default)]
pub struct ConnectionParams {
    pub host: String,
    pub port: u16,
    /// Environment variables (e.g. from the running container) for user, password, db name.
    pub env: Vec<(String, String)>,
}

impl ConnectionParams {
    /// Look up an env var by name (case-sensitive).
    pub fn get_env(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

// ---------------------------------------------------------------------------
// Supported feature
// ---------------------------------------------------------------------------

/// A supported feature with an identifier and human-readable description.
/// Used for discovery/listing (e.g. `gfs providers`).
#[derive(Debug, Clone)]
pub struct SupportedFeature {
    /// Feature identifier (e.g. `"tls"`, `"schema"`, `"backup"`).
    pub id: String,
    /// Short human-readable description of the feature.
    pub description: String,
}

/// A database provider argument.
#[derive(Debug, Clone)]
pub struct DatabaseProviderArg {
    /// Argument name (e.g. `"tls"`, `"schema"`, `"backup"`).
    pub name: String,
    /// Argument value.
    pub value: String,
}

// ---------------------------------------------------------------------------
// Import / Export types
// ---------------------------------------------------------------------------

/// A data format supported by a provider for import/export.
#[derive(Debug, Clone)]
pub struct DataFormat {
    /// Format identifier (e.g. `"sql"`, `"custom"`, `"directory"`).
    pub id: String,
    /// Human-readable description of the format.
    pub description: String,
    /// Default file extension (e.g. `".sql"`, `".dump"`).
    pub file_extension: String,
}

/// Sidecar spec for exporting data from a database.
///
/// The provider returns a [`ComputeDefinition`] for an ephemeral tool instance
/// (e.g. a postgres image that ships `pg_dump`) together with the shell command
/// to run inside it. The orchestrator sets `definition.host_data_dir` to the
/// host directory where the exported file should land.
#[derive(Debug, Clone)]
pub struct ExportSpec {
    /// Compute definition for the tool sidecar.
    /// `data_dir` = path inside the sidecar where the output file is written.
    /// `host_data_dir` = set by the orchestrator to the host output directory.
    pub definition: ComputeDefinition,
    /// Shell command to execute in the sidecar.
    pub command: String,
    /// Name of the output file inside `definition.data_dir`.
    pub output_filename: String,
}

/// Sidecar spec for importing data into a database.
///
/// The provider returns a [`ComputeDefinition`] for an ephemeral tool instance
/// together with the shell command to run inside it. The orchestrator sets
/// `definition.host_data_dir` to the host directory that contains the file to
/// import.
#[derive(Debug, Clone)]
pub struct ImportSpec {
    /// Compute definition for the tool sidecar.
    /// `data_dir` = path inside the sidecar where the input file is available.
    /// `host_data_dir` = set by the orchestrator to the host directory containing the file.
    pub definition: ComputeDefinition,
    /// Shell command to execute in the sidecar.
    pub command: String,
    /// Expected name of the input file inside `definition.data_dir`.
    pub input_filename: String,
}

/// Sidecar spec for extracting schema metadata from a database.
///
/// The provider returns a [`ComputeDefinition`] for an ephemeral tool instance
/// (e.g. postgres image with psql) and a shell command that runs schema
/// extraction queries and outputs results to stdout. The orchestrator runs
/// the task linked to the database container and parses the output.
#[derive(Debug, Clone)]
pub struct SchemaExtractionSpec {
    /// Compute definition for the tool sidecar.
    pub definition: ComputeDefinition,
    /// Shell command to execute in the sidecar. Output must use delimiters
    /// `GFS_SCHEMA_VERSION`, `GFS_SCHEMA_SCHEMAS`, `GFS_SCHEMA_TABLES`, `GFS_SCHEMA_COLUMNS`.
    ///
    /// The command MAY additionally emit a `GFS_SCHEMA_DDL` section carrying the
    /// schema-only DDL dump. Emitting the DDL through stdout (rather than a
    /// mounted file) is the only channel that survives runtimes where the task
    /// sidecar runs on a different host than the gfs process (e.g. Kubernetes,
    /// where the task pod and the repository live on separate nodes). When the
    /// section is absent the stored schema object simply carries an empty DDL.
    pub command: String,
}

// ---------------------------------------------------------------------------
// Lazy clone (RFC 008)
// ---------------------------------------------------------------------------

/// A read-only remote database to lazily clone from (copy-on-read).
///
/// Only `SELECT` access is assumed; nothing is created on the remote. See
/// `docs/rfcs/008-remote-clone.md`.
#[derive(Debug, Clone)]
pub struct RemoteSource {
    pub host: String,
    pub port: u16,
    pub dbname: String,
    pub user: String,
    pub password: String,
    /// Remote schemas to mirror (e.g. `["public"]`). Empty means "all
    /// non-system schemas", discovered at bootstrap time.
    pub schemas: Vec<String>,
}

/// Sidecar spec that bootstraps a lazy clone inside the local GFS database.
///
/// The provider returns a [`ComputeDefinition`] for an ephemeral tool instance
/// (a database image shipping the client, e.g. `psql`) and a shell command that
/// connects to the **local** GFS database and sets up the foreign-data-wrapper
/// link, the mixed-partition tables, and the sync catalog. No data is copied at
/// bootstrap time — data is hydrated on first read.
#[derive(Debug, Clone)]
pub struct CloneSpec {
    /// Compute definition for the tool sidecar.
    pub definition: ComputeDefinition,
    /// Shell command to execute in the sidecar (against the local database).
    pub command: String,
}

/// Signal number for graceful shutdown. On Unix, 15 = SIGTERM.
pub const SIGTERM: u32 = 15;

// ---------------------------------------------------------------------------
// Provider port
// ---------------------------------------------------------------------------

/// A database provider: supplies a definition and provider-specific behaviour.
/// Implementations (e.g. postgresql, mysql) are registered in a [`DatabaseProviderRegistry`].
pub trait DatabaseProvider: Send + Sync {
    /// Display name used to register and look up this provider (e.g. `"postgresql"`).
    fn name(&self) -> &str;

    /// Compute definition used for provisioning (image, env, ports, data dir, etc.).
    fn definition(&self) -> ComputeDefinition;

    /// Default container port for this database (e.g. 5432 for PostgreSQL).
    fn default_port(&self) -> u16;

    /// Default arguments for this database provider.
    fn default_args(&self) -> Vec<DatabaseProviderArg>;

    /// Render user-supplied container parameter overrides (from
    /// `[compute.params]`) into this provider's native argument syntax.
    ///
    /// Keys are logical setting names; the provider maps each into its own form
    /// (e.g. PostgreSQL `-c name=value`, MySQL `--name=value`). The returned
    /// args are appended *after* [`default_args`](Self::default_args), so for
    /// engines where the last occurrence wins they override the defaults.
    ///
    /// Default: returns nothing (provider does not support overrides).
    fn render_param_overrides(
        &self,
        params: &BTreeMap<String, String>,
    ) -> Vec<DatabaseProviderArg> {
        let _ = params;
        Vec::new()
    }

    /// [`definition`](Self::definition) with `params` rendered and appended to
    /// `args`. Provisioning sites use this so container tuning from
    /// `[compute.params]` is (re-)applied on every init/checkout/restart.
    fn definition_with_overrides(&self, params: &BTreeMap<String, String>) -> ComputeDefinition {
        let mut def = self.definition();
        def.args.extend(
            self.render_param_overrides(params)
                .into_iter()
                .flat_map(|a| {
                    if a.value.is_empty() {
                        vec![a.name]
                    } else {
                        vec![a.name, a.value]
                    }
                }),
        );
        def
    }

    /// Default signal sent to the database process when stopping (e.g. for graceful shutdown).
    /// Returns the signal number (e.g. [`SIGTERM`] = 15 on Unix). Default implementation returns SIGTERM.
    fn default_signal(&self) -> u32 {
        SIGTERM
    }

    /// Build a client connection string from host, port, and optional env (credentials, db name).
    fn connection_string(
        &self,
        params: &ConnectionParams,
    ) -> std::result::Result<String, ProviderError>;

    /// Extract version string from the definition's image (e.g. `postgres:16` → `"16"`).
    fn version_from_image(&self, definition: &ComputeDefinition) -> String {
        definition
            .image
            .split(':')
            .nth(1)
            .unwrap_or("latest")
            .to_string()
    }

    /// List of supported version tags (e.g. `"16"`, `"8.0"`). Used for discovery/listing (e.g. `gfs providers`).
    fn supported_versions(&self) -> Vec<String>;

    /// List of supported features with id and description. Used for discovery/listing (e.g. `gfs providers`).
    fn supported_features(&self) -> Vec<SupportedFeature>;

    /// Return the description for a feature by id. Returns `None` if the feature is not supported.
    fn feature_description(&self, feature_id: &str) -> Option<String> {
        self.supported_features()
            .into_iter()
            .find(|f| f.id == feature_id)
            .map(|f| f.description)
    }

    /// Prepare the database provider for snapshotting.
    /// Returns a list of commands to run before taking the snapshot (e.g. `psql -U user -c "CHECKPOINT;"`).
    /// The compute runtime runs these commands in the container before taking the snapshot.
    fn prepare_for_snapshot(&self, params: &ConnectionParams) -> Result<Vec<String>>;

    /// Return the user/group that should own files under the provider's `definition().data_dir`
    /// inside the container (for example `"postgres:postgres"`).
    ///
    /// This is used for best-effort permission repair after checkout when the workspace
    /// was populated from a snapshot created via container streaming (which intentionally
    /// does not preserve original ownership/mode bits).
    ///
    /// Default: `None` (provider does not declare a canonical owner).
    fn data_dir_owner(&self) -> Option<&'static str> {
        None
    }

    /// Startup probes executed **inside the running database container** after checkout.
    ///
    /// Goal: turn “container is running” into “database is actually usable on this workspace”.
    /// Probes should be:
    /// - fast
    /// - deterministic
    /// - safe (no mutations unless explicitly intended)
    ///
    /// The compute runtime should execute these probes with root privileges when available,
    /// because permission repair may be needed before the container’s default user can read
    /// the mounted data directory.
    ///
    /// Default: empty (no health gate).
    fn container_startup_probes(&self) -> &'static [&'static str] {
        &[]
    }

    // -----------------------------------------------------------------------
    // Import / Export
    // -----------------------------------------------------------------------

    /// List of formats this provider supports for exporting data.
    /// Default: empty (provider does not advertise export support).
    fn supported_export_formats(&self) -> Vec<DataFormat> {
        vec![]
    }

    /// List of formats this provider supports for importing data.
    /// Default: empty (provider does not advertise import support).
    fn supported_import_formats(&self) -> Vec<DataFormat> {
        vec![]
    }

    /// Describe how to export data in the given format as a sidecar task.
    ///
    /// Returns a [`ComputeDefinition`] for the tool sidecar, the shell command
    /// to run inside it, and the output filename. The orchestrator will set
    /// `definition.host_data_dir` before running the task.
    ///
    /// `params` carries the connection info the sidecar uses to reach the
    /// database instance (host, port, credentials).
    fn export_spec(
        &self,
        _params: &ConnectionParams,
        format: &str,
    ) -> std::result::Result<ExportSpec, ProviderError> {
        Err(ProviderError::UnsupportedFormat(format.to_string()))
    }

    /// Describe how to import data in the given format as a sidecar task.
    ///
    /// Returns a [`ComputeDefinition`] for the tool sidecar, the shell command
    /// to run inside it, and the expected input filename. The orchestrator will
    /// set `definition.host_data_dir` before running the task.
    ///
    /// `params` carries the connection info the sidecar uses to reach the
    /// database instance (host, port, credentials).
    /// `input_filename` is the basename of the file to import (e.g. from the user's `--file` path).
    fn import_spec(
        &self,
        _params: &ConnectionParams,
        format: &str,
        _input_filename: &str,
    ) -> std::result::Result<ImportSpec, ProviderError> {
        Err(ProviderError::UnsupportedFormat(format.to_string()))
    }

    // -----------------------------------------------------------------------
    // Query / Interactive Terminal
    // -----------------------------------------------------------------------

    /// Build a command to execute a query using the native database client (e.g. psql, mysql).
    ///
    /// The returned command is configured to execute `query` against the database
    /// instance specified by `params`. If `query` is `None`, the command should
    /// open an interactive terminal session.
    ///
    /// The caller is responsible for spawning the command and handling its output.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let cmd = provider.query_client_command(&params, Some("SELECT * FROM users"))?;
    /// let output = cmd.output()?;
    /// println!("{}", String::from_utf8_lossy(&output.stdout));
    /// ```
    fn query_client_command(
        &self,
        params: &ConnectionParams,
        query: Option<&str>,
    ) -> std::result::Result<std::process::Command, ProviderError>;

    /// Shell command to run **inside** the running database instance (via [`Compute::exec`]).
    ///
    /// Uses container env vars and loopback — no host-side client binaries. Interactive
    /// sessions are not supported; `sql` must be non-empty.
    fn query_in_instance_command(&self, sql: &str) -> std::result::Result<String, ProviderError> {
        let _ = sql;
        Err(ProviderError::UnsupportedFormat("query_in_instance".into()))
    }

    // -----------------------------------------------------------------------
    // Schema Extraction
    // -----------------------------------------------------------------------

    /// Returns SQL queries for extracting schema metadata.
    ///
    /// Each query should return JSON output that can be deserialized into
    /// the corresponding field of [`DatasourceMetadata`](crate::model::datasource::DatasourceMetadata).
    ///
    /// Standard query keys:
    /// - `"version"`: Database version string
    /// - `"schemas"`: List of schemas/namespaces
    /// - `"tables"`: List of tables with metadata
    /// - `"columns"`: List of columns with full metadata
    /// - `"relationships"`: List of foreign key relationships
    ///
    /// Default implementation returns empty map (provider doesn't support schema extraction).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let queries = provider.schema_extraction_queries();
    /// if let Some(version_query) = queries.get("version") {
    ///     // Execute query and parse JSON result
    /// }
    /// ```
    fn schema_extraction_queries(&self) -> HashMap<String, String> {
        HashMap::new()
    }

    /// Return a sidecar spec for schema extraction, or `None` if not supported.
    ///
    /// When provided, schema extraction runs inside a container (no host-side
    /// client tools required). The command must output to stdout with markers:
    /// `GFS_SCHEMA_VERSION`, `GFS_SCHEMA_SCHEMAS`, `GFS_SCHEMA_TABLES`, `GFS_SCHEMA_COLUMNS`.
    fn schema_extraction_spec(
        &self,
        _params: &ConnectionParams,
    ) -> std::result::Result<Option<SchemaExtractionSpec>, ProviderError> {
        Ok(None)
    }

    // -----------------------------------------------------------------------
    // Lazy clone (RFC 008)
    // -----------------------------------------------------------------------

    /// Build a sidecar spec that bootstraps a lazy (copy-on-read) clone of a
    /// read-only `remote` database inside the local GFS database.
    ///
    /// `local` carries the connection info the sidecar uses to reach the local
    /// GFS database. The default implementation reports the feature as
    /// unsupported.
    fn clone_bootstrap_spec(
        &self,
        _local: &ConnectionParams,
        _remote: &RemoteSource,
    ) -> std::result::Result<CloneSpec, ProviderError> {
        Err(ProviderError::UnsupportedFormat(
            "lazy clone not supported by this provider".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Registry port
// ---------------------------------------------------------------------------

/// Port for a registry of database **providers**. Callers register
/// provider implementations and look them up by name for provisioning and
/// provider-specific operations (e.g. connection string).
pub trait DatabaseProviderRegistry: Send + Sync {
    /// Register a provider. Overwrites any existing entry with the same name.
    fn register(&self, provider: Arc<dyn DatabaseProvider>) -> Result<()>;

    /// Return the provider for `name`, if registered.
    fn get(&self, name: &str) -> Option<Arc<dyn DatabaseProvider>>;

    /// Return the definition for `name`, if registered. Convenience over `get(name).map(|p| p.definition())`.
    fn get_definition(&self, name: &str) -> Option<ComputeDefinition> {
        self.get(name).map(|p| p.definition())
    }

    /// Return all registered provider names.
    fn list(&self) -> Vec<String>;

    /// Remove the provider for `name`. Returns the removed provider if it existed.
    fn unregister(&self, name: &str) -> Option<Arc<dyn DatabaseProvider>>;
}

// ---------------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------------

/// Default in-memory registry. Safe to share via `Arc<InMemoryDatabaseProviderRegistry>`.
#[derive(Default)]
pub struct InMemoryDatabaseProviderRegistry {
    providers: RwLock<HashMap<String, Arc<dyn DatabaseProvider>>>,
}

impl InMemoryDatabaseProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DatabaseProviderRegistry for InMemoryDatabaseProviderRegistry {
    fn register(&self, provider: Arc<dyn DatabaseProvider>) -> Result<()> {
        let name = provider.name().to_string();
        self.providers
            .write()
            .map_err(|_| RegistryError::Internal("lock poisoned".to_string()))?
            .insert(name, provider);
        Ok(())
    }

    fn get(&self, name: &str) -> Option<Arc<dyn DatabaseProvider>> {
        self.providers.read().ok()?.get(name).cloned()
    }

    fn list(&self) -> Vec<String> {
        self.providers
            .read()
            .map(|g| g.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    }

    fn unregister(&self, name: &str) -> Option<Arc<dyn DatabaseProvider>> {
        self.providers.write().ok()?.remove(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::ports::compute::ComputeDefinition;

    struct TestProvider {
        name: String,
    }

    impl DatabaseProvider for TestProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn definition(&self) -> ComputeDefinition {
            ComputeDefinition {
                labels: Default::default(),
                image: "test:latest".into(),
                env: vec![],
                ports: vec![],
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
        fn connection_string(
            &self,
            _: &ConnectionParams,
        ) -> std::result::Result<String, ProviderError> {
            Ok("test://localhost".into())
        }
        fn supported_versions(&self) -> Vec<String> {
            vec!["latest".into()]
        }
        fn supported_features(&self) -> Vec<SupportedFeature> {
            vec![
                SupportedFeature {
                    id: "tls".into(),
                    description: "TLS support".into(),
                },
                SupportedFeature {
                    id: "schema".into(),
                    description: "Schema extraction".into(),
                },
            ]
        }
        fn prepare_for_snapshot(&self, _: &ConnectionParams) -> Result<Vec<String>> {
            Ok(vec![])
        }
        fn query_client_command(
            &self,
            _: &ConnectionParams,
            _: Option<&str>,
        ) -> std::result::Result<std::process::Command, ProviderError> {
            Ok(std::process::Command::new("true"))
        }
    }

    #[test]
    fn in_memory_registry_register_get_list() {
        let registry = InMemoryDatabaseProviderRegistry::new();
        let provider = Arc::new(TestProvider {
            name: "postgres".into(),
        });
        registry.register(provider).unwrap();
        assert!(registry.get("postgres").is_some());
        assert_eq!(registry.list(), vec!["postgres"]);
    }

    #[test]
    fn in_memory_registry_unregister() {
        let registry = InMemoryDatabaseProviderRegistry::new();
        let provider = Arc::new(TestProvider {
            name: "mysql".into(),
        });
        registry.register(provider).unwrap();
        assert!(registry.get("mysql").is_some());
        let removed = registry.unregister("mysql");
        assert!(removed.is_some());
        assert!(registry.get("mysql").is_none());
        assert!(registry.list().is_empty());
    }

    #[test]
    fn in_memory_registry_empty_list() {
        let registry = InMemoryDatabaseProviderRegistry::new();
        assert!(registry.list().is_empty());
        assert!(registry.get("any").is_none());
    }

    #[test]
    fn connection_params_get_env() {
        let params = ConnectionParams {
            host: "localhost".into(),
            port: 5432,
            env: vec![
                ("USER".into(), "alice".into()),
                ("PASSWORD".into(), "secret".into()),
            ],
        };
        assert_eq!(params.get_env("USER"), Some("alice"));
        assert_eq!(params.get_env("PASSWORD"), Some("secret"));
        assert_eq!(params.get_env("MISSING"), None);
    }

    #[test]
    fn test_provider_version_from_image() {
        let provider = TestProvider {
            name: "test".into(),
        };
        let def = ComputeDefinition {
            labels: Default::default(),
            image: "postgres:16".into(),
            env: vec![],
            ports: vec![],
            data_dir: PathBuf::from("/data"),
            host_data_dir: None,
            user: None,
            logs_dir: None,
            conf_dir: None,
            args: vec![],
        };
        assert_eq!(provider.version_from_image(&def), "16");
        let def_latest = ComputeDefinition {
            labels: Default::default(),
            image: "postgres".into(),
            env: vec![],
            ports: vec![],
            data_dir: PathBuf::from("/data"),
            host_data_dir: None,
            user: None,
            logs_dir: None,
            conf_dir: None,
            args: vec![],
        };
        assert_eq!(provider.version_from_image(&def_latest), "latest");
    }

    #[test]
    fn test_provider_default_signal() {
        let provider = TestProvider {
            name: "test".into(),
        };
        assert_eq!(provider.default_signal(), SIGTERM);
    }

    #[test]
    fn registry_error_display() {
        assert_eq!(
            RegistryError::AlreadyRegistered("x".into()).to_string(),
            "definition already registered: 'x'"
        );
        assert_eq!(
            RegistryError::NotFound("y".into()).to_string(),
            "definition not found: 'y'"
        );
        assert_eq!(
            RegistryError::Internal("z".into()).to_string(),
            "internal error: z"
        );
    }

    #[test]
    fn test_provider_feature_description() {
        let provider = TestProvider {
            name: "test".into(),
        };
        assert_eq!(
            provider.feature_description("tls"),
            Some("TLS support".into())
        );
        assert_eq!(
            provider.feature_description("schema"),
            Some("Schema extraction".into())
        );
        assert_eq!(provider.feature_description("unknown"), None);
    }

    #[test]
    fn provider_error_display() {
        assert_eq!(
            ProviderError::MissingEnvVar("X".into()).to_string(),
            "missing required env var for connection string: 'X'"
        );
        assert_eq!(
            ProviderError::InvalidParams("bad".into()).to_string(),
            "invalid connection params: bad"
        );
        assert_eq!(
            ProviderError::UnsupportedFormat("xyz".into()).to_string(),
            "unsupported format: 'xyz'"
        );
    }
}
