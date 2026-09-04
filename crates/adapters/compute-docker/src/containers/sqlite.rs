//! SQLite provider: file-backed database container for GFS.

use std::path::PathBuf;
use std::sync::Arc;

use gfs_domain::ports::compute::{ComputeDefinition, EnvVar, PortMapping};
use gfs_domain::ports::database_provider::{
    ConnectionParams, DataFormat, DatabaseProvider, DatabaseProviderArg, DatabaseProviderRegistry,
    ProviderError, Result, SupportedFeature,
};

// Provider name used to register and look up this implementation.
const NAME: &str = "sqlite";

// Lightweight helper image that exposes the `sqlite3` CLI and mounts a
// host-backed file under `/data`. Using a small, single-purpose image here
// keeps export/import tooling simple (the sidecar can run `sqlite3 .dump`).
const DEFAULT_IMAGE: &str = "nouchka/sqlite3:latest";

// Inside-container data directory where the SQLite file is stored.
const CONTAINER_DATA_DIR: &str = "/data";

// Default path (inside container) for the SQLite file when not overridden.
const DEFAULT_SQLITE_FILE: &str = "/data/db.sqlite";

// Environment variables that may be supplied by the orchestrator or the
// user to control where the SQLite file is placed on the host/container.
const ENV_SQLITE_FILE: &str = "SQLITE_FILE";
const ENV_SQLITE_HOST_PATH: &str = "SQLITE_HOST_PATH";

#[derive(Debug)]
pub struct SqliteProvider;

impl SqliteProvider {
    pub fn new() -> Self {
        Self
    }

    fn definition_impl() -> ComputeDefinition {
        ComputeDefinition {
            labels: Default::default(),
            image: DEFAULT_IMAGE.to_string(),
            env: vec![
                EnvVar {
                    name: ENV_SQLITE_FILE.to_string(),
                    default: Some(DEFAULT_SQLITE_FILE.to_string()),
                },
                EnvVar {
                    name: ENV_SQLITE_HOST_PATH.to_string(),
                    default: Some(DEFAULT_SQLITE_FILE.to_string()),
                },
            ],
            ports: vec![PortMapping {
                compute_port: 9999,
                host_port: None,
            }],
            data_dir: PathBuf::from(CONTAINER_DATA_DIR),
            host_data_dir: None,
            user: None,
            logs_dir: None,
            conf_dir: None,
            // The sqlite image used here does not normally run a long-lived
            // server process. We start the container with a dummy `tail -f`
            // so the container remains alive and the file mount is available
            // for sidecar/export/import operations.
            args: vec!["tail".into(), "-f".into(), "/dev/null".into()],
        }
    }
}

impl Default for SqliteProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl DatabaseProvider for SqliteProvider {
    fn name(&self) -> &str {
        NAME
    }

    fn definition(&self) -> ComputeDefinition {
        Self::definition_impl()
    }

    fn default_port(&self) -> u16 {
        9999
    }

    fn default_args(&self) -> Vec<DatabaseProviderArg> {
        vec![]
    }

    fn connection_string(
        &self,
        params: &ConnectionParams,
    ) -> std::result::Result<String, ProviderError> {
        // Prefer an explicit host path when provided (it reflects the host
        // filesystem path mounted into the container). Fall back to the
        // container-local env var for cases where orchestration only exposes
        // the in-container path. We return a SQLite connection URL using
        // the `sqlite:///` scheme; note that absolute paths will result in
        // an extra leading slash (e.g. `sqlite:////tmp/db.sqlite`), which is
        // a valid file URL form recognized by many SQLite clients.
        let path = params
            .get_env(ENV_SQLITE_HOST_PATH)
            .or_else(|| params.get_env(ENV_SQLITE_FILE))
            .unwrap_or(DEFAULT_SQLITE_FILE);

        Ok(format!("sqlite:///{}", path))
    }

    fn supported_versions(&self) -> Vec<String> {
        vec!["latest".into(), "3".into()]
    }

    fn supported_features(&self) -> Vec<SupportedFeature> {
        vec![
            SupportedFeature {
                id: "schema".into(),
                description: "Schema inspection via sqlite3".into(),
            },
            SupportedFeature {
                id: "import".into(),
                description: "Import SQL files into SQLite".into(),
            },
            SupportedFeature {
                id: "export".into(),
                description: "Export SQLite data as SQL".into(),
            },
        ]
    }

    fn prepare_for_snapshot(&self, _params: &ConnectionParams) -> Result<Vec<String>> {
        Ok(vec![])
    }

    fn query_client_command(
        &self,
        params: &ConnectionParams,
        query: Option<&str>,
    ) -> std::result::Result<std::process::Command, ProviderError> {
        // Build a native `sqlite3` client invocation that either opens an
        // interactive shell (when `query` is None) or executes a single SQL
        // statement. The orchestrator runs this command on the host where
        // the sqlite3 binary is available (or via a sidecar that provides
        // the client); therefore we expose the host-backed file path.
        let path = params
            .get_env(ENV_SQLITE_HOST_PATH)
            .or_else(|| params.get_env(ENV_SQLITE_FILE))
            .unwrap_or(DEFAULT_SQLITE_FILE);

        let mut cmd = std::process::Command::new("sqlite3");
        cmd.arg(path);

        if let Some(q) = query {
            // When executing a statement, pass it as a single argument so
            // sqlite3 runs it and exits. The caller is responsible for any
            // shell quoting if this command is forwarded through `sh -c`.
            cmd.arg(q);
        }

        Ok(cmd)
    }

    fn supported_export_formats(&self) -> Vec<DataFormat> {
        vec![DataFormat {
            id: "sql".into(),
            description: "SQLite SQL dump".into(),
            file_extension: ".sql".into(),
        }]
    }

    fn supported_import_formats(&self) -> Vec<DataFormat> {
        vec![DataFormat {
            id: "sql".into(),
            description: "SQLite SQL import".into(),
            file_extension: ".sql".into(),
        }]
    }

    fn export_spec(
        &self,
        _params: &ConnectionParams,
        format: &str,
    ) -> std::result::Result<gfs_domain::ports::database_provider::ExportSpec, ProviderError> {
        if format != "sql" {
            return Err(ProviderError::UnsupportedFormat(format.to_string()));
        }

        // The export runs inside a small sidecar image that has `sqlite3`.
        // It mounts the provider `data_dir` at `/data` so the command below
        // can read the database file and write an SQL dump into the shared
        // `data_dir`. The orchestrator sets `host_data_dir` before running
        // the sidecar so the output becomes available on the host.
        Ok(gfs_domain::ports::database_provider::ExportSpec {
            definition: ComputeDefinition {
                labels: Default::default(),
                image: self.definition().image,
                env: vec![],
                ports: vec![],
                data_dir: PathBuf::from("/data"),
                host_data_dir: None,
                user: None,
                logs_dir: None,
                conf_dir: None,
                args: vec![],
            },
            // Produce a SQL dump using the sqlite3 CLI. The output file
            // (`export.sql`) will be written inside the sidecar's `data_dir`.
            command: format!("sqlite3 {} .dump > {}", DEFAULT_SQLITE_FILE, "export.sql"),
            output_filename: "export.sql".into(),
        })
    }

    fn import_spec(
        &self,
        _params: &ConnectionParams,
        format: &str,
        input_filename: &str,
    ) -> std::result::Result<gfs_domain::ports::database_provider::ImportSpec, ProviderError> {
        if format != "sql" {
            return Err(ProviderError::UnsupportedFormat(format.to_string()));
        }

        // The import runs a sidecar that executes `sqlite3` and reads the
        // provided SQL file (mounted at `data_dir`). The input filename is
        // controlled by the caller/orchestrator and must match the file
        // placed into the sidecar's `data_dir`.
        Ok(gfs_domain::ports::database_provider::ImportSpec {
            definition: ComputeDefinition {
                labels: Default::default(),
                image: self.definition().image,
                env: vec![],
                ports: vec![],
                data_dir: PathBuf::from("/data"),
                host_data_dir: None,
                user: None,
                logs_dir: None,
                conf_dir: None,
                args: vec![],
            },
            command: format!("sqlite3 {} < {}", DEFAULT_SQLITE_FILE, input_filename),
            input_filename: input_filename.to_string(),
        })
    }
}

/// Registers the SQLite provider in `registry` under the name `"sqlite"`.
pub fn register(registry: &impl DatabaseProviderRegistry) -> Result<()> {
    registry.register(Arc::new(SqliteProvider::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_and_default_port() {
        let provider = SqliteProvider::new();
        assert_eq!(provider.name(), "sqlite");
        assert_eq!(provider.default_port(), 9999);
    }

    #[test]
    fn connection_string_uses_host_path() {
        let provider = SqliteProvider::new();
        let params = ConnectionParams {
            host: "localhost".into(),
            port: 9999,
            env: vec![(ENV_SQLITE_HOST_PATH.to_string(), "/tmp/test.db".to_string())],
        };
        let s = provider.connection_string(&params).unwrap();
        assert_eq!(s, "sqlite:////tmp/test.db");
    }

    #[test]
    fn query_client_command_uses_sqlite3() {
        let provider = SqliteProvider::new();
        let params = ConnectionParams {
            host: "localhost".into(),
            port: 9999,
            env: vec![(ENV_SQLITE_HOST_PATH.to_string(), "/tmp/test.db".to_string())],
        };
        let cmd = provider
            .query_client_command(&params, Some("SELECT 1;"))
            .unwrap();
        assert_eq!(cmd.get_program().to_string_lossy(), "sqlite3");
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec!["/tmp/test.db".to_string(), "SELECT 1;".to_string()]
        );
    }
}
