//! MySQL provider: compute definition, connection string, and related behaviour.

use std::path::PathBuf;
use std::sync::Arc;

use gfs_domain::ports::compute::{ComputeDefinition, EnvVar, PortMapping};
use gfs_domain::ports::database_provider::{
    ConnectionParams, DataFormat, DatabaseProvider, DatabaseProviderArg, DatabaseProviderRegistry,
    ExportSpec, ImportSpec, ProviderError, Result, SIGTERM, SchemaExtractionSpec, SupportedFeature,
};

const NAME: &str = "mysql";

/// Default MySQL image (official image).
const DEFAULT_IMAGE: &str = "mysql:latest";

/// Path inside the container where MySQL stores data.
const CONTAINER_DATA_DIR: &str = "/var/lib/mysql";

const ENV_ROOT_PASSWORD: &str = "MYSQL_ROOT_PASSWORD";
const ENV_DATABASE: &str = "MYSQL_DATABASE";

const DEFAULT_ROOT_PASSWORD: &str = "mysql";
const DEFAULT_DB: &str = "mysql";

/// MySQL compute definition provider. Supplies the definition and
/// provider-specific behaviour (connection string, name, default port).
#[derive(Debug)]
pub struct MysqlProvider;

impl MysqlProvider {
    pub fn new() -> Self {
        Self
    }

    fn definition_impl() -> ComputeDefinition {
        ComputeDefinition {
            image: DEFAULT_IMAGE.to_string(),
            env: vec![
                EnvVar {
                    name: ENV_ROOT_PASSWORD.to_string(),
                    default: Some(DEFAULT_ROOT_PASSWORD.to_string()),
                },
                EnvVar {
                    name: ENV_DATABASE.to_string(),
                    default: Some(DEFAULT_DB.to_string()),
                },
            ],
            ports: vec![PortMapping {
                compute_port: 3306,
                host_port: None,
            }],
            data_dir: PathBuf::from(CONTAINER_DATA_DIR),
            host_data_dir: None, // set by caller at provision time
            logs_dir: None,
            conf_dir: None,
            args: vec![],
        }
    }
}

impl Default for MysqlProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl DatabaseProvider for MysqlProvider {
    fn name(&self) -> &str {
        NAME
    }

    fn definition(&self) -> ComputeDefinition {
        let mut def = Self::definition_impl();
        def.args = self
            .default_args()
            .into_iter()
            .flat_map(|a| {
                if a.value.is_empty() {
                    vec![a.name]
                } else {
                    vec![a.name, a.value]
                }
            })
            .collect();
        def
    }

    fn default_port(&self) -> u16 {
        3306
    }

    fn default_args(&self) -> Vec<DatabaseProviderArg> {
        vec![
            DatabaseProviderArg {
                name: "--skip-name-resolve".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--max_connections=5".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--max_connect_errors=100".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--table_open_cache=4".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--thread_cache_size=1".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--max_allowed_packet=256K".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--net_buffer_length=8K".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--innodb_buffer_pool_size=4M".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--innodb_log_buffer_size=128K".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--innodb_flush_method=O_DIRECT".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--innodb_flush_log_at_trx_commit=2".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--innodb_file_per_table=1".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--innodb_read_io_threads=1".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--innodb_write_io_threads=1".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--performance_schema=0".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--thread_stack=192K".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--skip-log-bin".into(),
                value: String::new(),
            },
            DatabaseProviderArg {
                name: "--bind-address=0.0.0.0".into(),
                value: String::new(),
            },
        ]
    }

    fn default_signal(&self) -> u32 {
        SIGTERM
    }

    fn connection_string(
        &self,
        params: &ConnectionParams,
    ) -> std::result::Result<String, ProviderError> {
        let user = "root";
        let password = params
            .get_env(ENV_ROOT_PASSWORD)
            .unwrap_or(DEFAULT_ROOT_PASSWORD);
        let db = params.get_env(ENV_DATABASE).unwrap_or(DEFAULT_DB);
        Ok(format!(
            "mysql://{}:{}@{}:{}/{}",
            user, password, params.host, params.port, db
        ))
    }

    fn supported_versions(&self) -> Vec<String> {
        vec!["8.0".into(), "8.1".into()]
    }

    fn supported_features(&self) -> Vec<SupportedFeature> {
        vec![
            SupportedFeature {
                id: "tls".into(),
                description: "TLS/SSL encryption for connections.".into(),
            },
            SupportedFeature {
                id: "schema".into(),
                description: "Schema and DDL management.".into(),
            },
            SupportedFeature {
                id: "masking".into(),
                description: "Data masking and redaction.".into(),
            },
            SupportedFeature {
                id: "backup".into(),
                description: "Backup and restore.".into(),
            },
            SupportedFeature {
                id: "import".into(),
                description: "Data import from external sources.".into(),
            },
        ]
    }

    fn prepare_for_snapshot(&self, _params: &ConnectionParams) -> Result<Vec<String>> {
        Ok(vec![])
    }

    // -----------------------------------------------------------------------
    // Import / Export
    // -----------------------------------------------------------------------

    fn supported_export_formats(&self) -> Vec<DataFormat> {
        vec![
            DataFormat {
                id: "sql".into(),
                description: "Plain-text SQL dump (mysqldump).".into(),
                file_extension: ".sql".into(),
            },
            DataFormat {
                id: "schema".into(),
                description: "Schema-only DDL dump (mysqldump --no-data).".into(),
                file_extension: ".sql".into(),
            },
        ]
    }

    fn supported_import_formats(&self) -> Vec<DataFormat> {
        vec![DataFormat {
            id: "sql".into(),
            description: "Plain-text SQL file (loaded via mysql client).".into(),
            file_extension: ".sql".into(),
        }]
    }

    fn export_spec(
        &self,
        params: &ConnectionParams,
        format: &str,
    ) -> std::result::Result<ExportSpec, ProviderError> {
        let password = params
            .get_env(ENV_ROOT_PASSWORD)
            .unwrap_or(DEFAULT_ROOT_PASSWORD);
        let db = params.get_env(ENV_DATABASE).unwrap_or(DEFAULT_DB);

        match format {
            "sql" => Ok(ExportSpec {
                definition: ComputeDefinition {
                    image: self.definition().image,
                    env: vec![],
                    ports: vec![],
                    data_dir: PathBuf::from("/data"),
                    host_data_dir: None, // set by orchestrator
                    logs_dir: None,
                    conf_dir: None,
                    args: vec![],
                },
                command: format!(
                    "mysqldump -h {host} -P {port} -u root -p'{password}' {db} > /data/export.sql",
                    host = params.host,
                    port = params.port,
                    password = password,
                    db = db,
                ),
                output_filename: "export.sql".into(),
            }),
            "schema" => Ok(ExportSpec {
                definition: ComputeDefinition {
                    image: self.definition().image,
                    env: vec![],
                    ports: vec![],
                    data_dir: PathBuf::from("/data"),
                    host_data_dir: None, // set by orchestrator
                    logs_dir: None,
                    conf_dir: None,
                    args: vec![],
                },
                command: format!(
                    "mysqldump -h {host} -P {port} -u root -p'{password}' --no-data {db} > /data/schema.sql",
                    host = params.host,
                    port = params.port,
                    password = password,
                    db = db,
                ),
                output_filename: "schema.sql".into(),
            }),
            other => Err(ProviderError::UnsupportedFormat(other.to_string())),
        }
    }

    fn import_spec(
        &self,
        params: &ConnectionParams,
        format: &str,
        input_filename: &str,
    ) -> std::result::Result<ImportSpec, ProviderError> {
        let password = params
            .get_env(ENV_ROOT_PASSWORD)
            .unwrap_or(DEFAULT_ROOT_PASSWORD);
        let db = params.get_env(ENV_DATABASE).unwrap_or(DEFAULT_DB);

        match format {
            "sql" => Ok(ImportSpec {
                definition: ComputeDefinition {
                    image: self.definition().image,
                    env: vec![],
                    ports: vec![],
                    data_dir: PathBuf::from("/data"),
                    host_data_dir: None, // set by orchestrator
                    logs_dir: None,
                    conf_dir: None,
                    args: vec![],
                },
                command: format!(
                    "mysql -h {host} -P {port} -u root -p'{password}' {db} < /data/{file}",
                    host = params.host,
                    port = params.port,
                    password = password,
                    db = db,
                    file = input_filename,
                ),
                input_filename: input_filename.to_string(),
            }),
            other => Err(ProviderError::UnsupportedFormat(other.to_string())),
        }
    }

    // -----------------------------------------------------------------------
    // Query / Interactive Terminal
    // -----------------------------------------------------------------------

    fn query_client_command(
        &self,
        params: &ConnectionParams,
        query: Option<&str>,
    ) -> std::result::Result<std::process::Command, ProviderError> {
        let password = params
            .get_env(ENV_ROOT_PASSWORD)
            .unwrap_or(DEFAULT_ROOT_PASSWORD);
        let db = params.get_env(ENV_DATABASE).unwrap_or(DEFAULT_DB);

        // Build mysql command with connection parameters
        let mut cmd = std::process::Command::new("mysql");
        cmd.arg("-h").arg(&params.host);
        cmd.arg("-P").arg(params.port.to_string());
        cmd.arg("-u").arg("root");
        cmd.arg(format!("-p{}", password));
        cmd.arg(db);

        // If a query is provided, execute it with -e; otherwise open interactive terminal
        if let Some(q) = query {
            cmd.arg("-e").arg(q);
        }

        Ok(cmd)
    }

    // -----------------------------------------------------------------------
    // Schema Extraction
    // -----------------------------------------------------------------------

    fn schema_extraction_queries(&self) -> std::collections::HashMap<String, String> {
        let mut queries = std::collections::HashMap::new();

        // Version query - returns database version string
        queries.insert("version".to_string(), "SELECT version();".to_string());

        // Schemas query - returns JSON array of schemas
        queries.insert(
            "schemas".to_string(),
            "SELECT COALESCE(
                JSON_ARRAYAGG(
                    JSON_OBJECT(
                        'id', 0,
                        'name', SCHEMA_NAME,
                        'owner', ''
                    )
                ),
                JSON_ARRAY()
            ) as result
            FROM information_schema.SCHEMATA
            WHERE SCHEMA_NAME NOT IN ('information_schema', 'mysql', 'performance_schema', 'sys')
            ORDER BY SCHEMA_NAME;"
                .to_string(),
        );

        // Tables query - returns JSON array of tables with metadata
        queries.insert(
            "tables".to_string(),
            "SELECT COALESCE(
                JSON_ARRAYAGG(
                    JSON_OBJECT(
                        'id', 0,
                        'schema', TABLE_SCHEMA,
                        'name', TABLE_NAME,
                        'rls_enabled', false,
                        'rls_forced', false,
                        'bytes', COALESCE(DATA_LENGTH + INDEX_LENGTH, 0),
                        'size', CONCAT(ROUND((DATA_LENGTH + INDEX_LENGTH) / 1024, 2), ' KB'),
                        'live_rows_estimate', COALESCE(TABLE_ROWS, 0),
                        'dead_rows_estimate', 0,
                        'comment', TABLE_COMMENT,
                        'primary_keys', JSON_ARRAY(),
                        'relationships', JSON_ARRAY()
                    )
                ),
                JSON_ARRAY()
            ) as result
            FROM information_schema.TABLES
            WHERE TABLE_SCHEMA NOT IN ('information_schema', 'mysql', 'performance_schema', 'sys')
                AND TABLE_TYPE = 'BASE TABLE'
            ORDER BY TABLE_SCHEMA, TABLE_NAME;"
                .to_string(),
        );

        // Columns query - returns JSON array of columns with full metadata
        queries.insert(
            "columns".to_string(),
            "SELECT COALESCE(
                JSON_ARRAYAGG(
                    JSON_OBJECT(
                        'id', CONCAT(TABLE_SCHEMA, '.', TABLE_NAME, '.', COLUMN_NAME),
                        'table_id', 0,
                        'schema', TABLE_SCHEMA,
                        'table', TABLE_NAME,
                        'name', COLUMN_NAME,
                        'ordinal_position', ORDINAL_POSITION,
                        'data_type', DATA_TYPE,
                        'format', DATA_TYPE,
                        'is_identity', IF(EXTRA LIKE '%auto_increment%', true, false),
                        'identity_generation', NULL,
                        'is_generated', IF(EXTRA LIKE '%GENERATED%', true, false),
                        'is_nullable', IF(IS_NULLABLE = 'YES', true, false),
                        'is_updatable', true,
                        'is_unique', IF(COLUMN_KEY = 'PRI' OR COLUMN_KEY = 'UNI', true, false),
                        'check', NULL,
                        'default_value', COLUMN_DEFAULT,
                        'enums', JSON_ARRAY(),
                        'comment', COLUMN_COMMENT
                    )
                ),
                JSON_ARRAY()
            ) as result
            FROM information_schema.COLUMNS
            WHERE TABLE_SCHEMA NOT IN ('information_schema', 'mysql', 'performance_schema', 'sys')
            ORDER BY TABLE_SCHEMA, TABLE_NAME, ORDINAL_POSITION;"
                .to_string(),
        );

        queries
    }

    fn schema_extraction_spec(
        &self,
        params: &ConnectionParams,
    ) -> std::result::Result<Option<SchemaExtractionSpec>, ProviderError> {
        let password = params
            .get_env(ENV_ROOT_PASSWORD)
            .unwrap_or(DEFAULT_ROOT_PASSWORD);
        let db = params.get_env(ENV_DATABASE).unwrap_or(DEFAULT_DB);
        let queries = self.schema_extraction_queries();
        let schemas_q = queries
            .get("schemas")
            .ok_or_else(|| ProviderError::InvalidParams("missing schemas query".into()))?;
        let tables_q = queries
            .get("tables")
            .ok_or_else(|| ProviderError::InvalidParams("missing tables query".into()))?;
        let columns_q = queries
            .get("columns")
            .ok_or_else(|| ProviderError::InvalidParams("missing columns query".into()))?;

        let command = format!(
            r#"echo "GFS_SCHEMA_VERSION"
MYSQL_PWD="{password}" mysql -h {host} -P {port} -u root -N -e "SELECT version();"
echo "GFS_SCHEMA_SCHEMAS"
MYSQL_PWD="{password}" mysql -h {host} -P {port} -u root -D {db} -N -e "$(cat <<'SCHEMAS_EOF'
{schemas_query}
SCHEMAS_EOF
)"
echo "GFS_SCHEMA_TABLES"
MYSQL_PWD="{password}" mysql -h {host} -P {port} -u root -D {db} -N -e "$(cat <<'TABLES_EOF'
{tables_query}
TABLES_EOF
)"
echo "GFS_SCHEMA_COLUMNS"
MYSQL_PWD="{password}" mysql -h {host} -P {port} -u root -D {db} -N -e "$(cat <<'COLUMNS_EOF'
{columns_query}
COLUMNS_EOF
)""#,
            password = password,
            host = params.host,
            port = params.port,
            db = db,
            schemas_query = schemas_q,
            tables_query = tables_q,
            columns_query = columns_q,
        );

        Ok(Some(SchemaExtractionSpec {
            definition: ComputeDefinition {
                image: self.definition().image,
                env: vec![EnvVar {
                    name: "MYSQL_PWD".into(),
                    default: Some(password.to_string()),
                }],
                ports: vec![],
                data_dir: PathBuf::from("/tmp"),
                host_data_dir: None,
                logs_dir: None,
                conf_dir: None,
                args: vec![],
            },
            command,
        }))
    }
}

/// Registers the MySQL provider in `registry` under the name `"mysql"`.
pub fn register(registry: &impl DatabaseProviderRegistry) -> Result<()> {
    registry.register(Arc::new(MysqlProvider::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_string_uses_defaults() {
        let provider = MysqlProvider::new();
        let params = ConnectionParams {
            host: "localhost".to_string(),
            port: 3306,
            env: vec![],
        };
        let s = provider.connection_string(&params).unwrap();
        assert_eq!(s, "mysql://root:mysql@localhost:3306/mysql");
    }

    #[test]
    fn connection_string_uses_env_overrides() {
        let provider = MysqlProvider::new();
        let params = ConnectionParams {
            host: "db.example.com".to_string(),
            port: 13306,
            env: vec![
                ("MYSQL_ROOT_PASSWORD".to_string(), "secret".to_string()),
                ("MYSQL_DATABASE".to_string(), "mydb".to_string()),
            ],
        };
        let s = provider.connection_string(&params).unwrap();
        assert_eq!(s, "mysql://root:secret@db.example.com:13306/mydb");
    }

    #[test]
    fn name_and_default_port() {
        let provider = MysqlProvider::new();
        assert_eq!(provider.name(), "mysql");
        assert_eq!(provider.default_port(), 3306);
    }

    #[test]
    fn supported_versions_non_empty() {
        let provider = MysqlProvider::new();
        let versions = provider.supported_versions();
        assert!(!versions.is_empty());
        assert!(versions.contains(&"8.0".to_string()));
    }

    #[test]
    fn supported_features_contains_tls_and_schema() {
        let provider = MysqlProvider::new();
        let features = provider.supported_features();
        let ids: Vec<_> = features.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&"tls"));
        assert!(ids.contains(&"schema"));
    }

    #[test]
    fn feature_description_returns_some_for_backup() {
        let provider = MysqlProvider::new();
        let desc = provider.feature_description("backup");
        assert!(desc.is_some());
        assert!(desc.unwrap().contains("Backup"));
    }

    #[test]
    fn default_signal_is_sigterm() {
        let provider = MysqlProvider::new();
        assert_eq!(provider.default_signal(), SIGTERM);
    }

    #[test]
    fn default_args_non_empty_and_definition_includes_them() {
        let provider = MysqlProvider::new();
        let args = provider.default_args();
        assert!(!args.is_empty());
        assert_eq!(
            args.first().map(|a| a.name.as_str()),
            Some("--skip-name-resolve")
        );
        let def = provider.definition();
        assert_eq!(def.args.len(), args.len());
        assert_eq!(def.args.first(), Some(&"--skip-name-resolve".to_string()));
        assert_eq!(def.args.last(), Some(&"--bind-address=0.0.0.0".to_string()));
    }

    #[test]
    fn supported_export_formats_includes_sql() {
        let provider = MysqlProvider::new();
        let formats = provider.supported_export_formats();
        assert_eq!(formats.len(), 2);
        let ids: Vec<_> = formats.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&"sql"));
        assert!(ids.contains(&"schema"));
    }

    #[test]
    fn supported_import_formats_includes_sql() {
        let provider = MysqlProvider::new();
        let formats = provider.supported_import_formats();
        assert_eq!(formats.len(), 1);
        assert_eq!(formats[0].id, "sql");
    }

    #[test]
    fn schema_extraction_spec_returns_some_with_delimiters() {
        let provider = MysqlProvider::new();
        let params = ConnectionParams {
            host: "172.17.0.3".into(),
            port: 3306,
            env: vec![
                ("MYSQL_ROOT_PASSWORD".into(), "secret".into()),
                ("MYSQL_DATABASE".into(), "mydb".into()),
            ],
        };
        let spec = provider.schema_extraction_spec(&params).unwrap();
        let spec = spec.expect("mysql provider supports schema extraction");
        assert_eq!(spec.definition.image, "mysql:latest");
        assert!(spec.command.contains("GFS_SCHEMA_VERSION"));
        assert!(spec.command.contains("GFS_SCHEMA_SCHEMAS"));
        assert!(spec.command.contains("GFS_SCHEMA_TABLES"));
        assert!(spec.command.contains("GFS_SCHEMA_COLUMNS"));
        assert!(spec.command.contains("mysql"));
        assert!(spec.command.contains("-h 172.17.0.3"));
        assert!(spec.command.contains("-D mydb"));
    }

    #[test]
    fn export_spec_sql_produces_mysqldump_command() {
        let provider = MysqlProvider::new();
        let params = ConnectionParams {
            host: "172.17.0.3".into(),
            port: 3306,
            env: vec![
                ("MYSQL_ROOT_PASSWORD".into(), "secret".into()),
                ("MYSQL_DATABASE".into(), "mydb".into()),
            ],
        };
        let spec = provider.export_spec(&params, "sql").unwrap();
        assert!(spec.command.contains("mysqldump"));
        assert!(spec.command.contains("-h 172.17.0.3"));
        assert!(spec.command.contains("-P 3306"));
        assert!(spec.command.contains("-p'secret'"));
        assert!(spec.command.contains("mydb"));
        assert_eq!(spec.output_filename, "export.sql");
    }

    #[test]
    fn import_spec_sql_produces_mysql_command() {
        let provider = MysqlProvider::new();
        let params = ConnectionParams {
            host: "172.17.0.3".into(),
            port: 3306,
            env: vec![
                ("MYSQL_ROOT_PASSWORD".into(), "secret".into()),
                ("MYSQL_DATABASE".into(), "mydb".into()),
            ],
        };
        let spec = provider.import_spec(&params, "sql", "import.sql").unwrap();
        assert!(spec.command.contains("mysql"));
        assert!(!spec.command.contains("mysqldump"));
        assert!(spec.command.contains("< /data/import.sql"));
        assert_eq!(spec.input_filename, "import.sql");
    }

    #[test]
    fn export_spec_unsupported_format_returns_error() {
        let provider = MysqlProvider::new();
        let params = ConnectionParams {
            host: "172.17.0.3".into(),
            port: 3306,
            env: vec![],
        };
        let result = provider.export_spec(&params, "custom");
        assert!(matches!(result, Err(ProviderError::UnsupportedFormat(_))));
    }

    #[test]
    fn export_spec_sidecar_uses_same_image_as_definition() {
        let provider = MysqlProvider::new();
        let params = ConnectionParams {
            host: "172.17.0.3".into(),
            port: 3306,
            env: vec![],
        };
        let spec = provider.export_spec(&params, "sql").unwrap();
        assert_eq!(spec.definition.image, provider.definition().image);
    }
}
