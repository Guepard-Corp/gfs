//! PostgreSQL provider: compute definition, connection string, and related behaviour.

use std::path::PathBuf;
use std::sync::Arc;

use gfs_domain::ports::compute::{ComputeDefinition, EnvVar, PortMapping};
use gfs_domain::ports::database_provider::{
    ConnectionParams, DataFormat, DatabaseProvider, DatabaseProviderArg, DatabaseProviderRegistry,
    ExportSpec, ImportSpec, ProviderError, Result, SIGTERM, SchemaExtractionSpec, SupportedFeature,
};

const NAME: &str = "postgres";

/// Default PostgreSQL image (official image, current LTS-alpine).
const DEFAULT_IMAGE: &str = "postgres:latest";

/// Path inside the container where PostgreSQL stores data (PGDATA).
const CONTAINER_DATA_DIR: &str = "/var/lib/postgresql/data";

const ENV_USER: &str = "POSTGRES_USER";
const ENV_PASSWORD: &str = "POSTGRES_PASSWORD";
const ENV_DB: &str = "POSTGRES_DB";

const DEFAULT_USER: &str = "postgres";
const DEFAULT_PASSWORD: &str = "postgres";
const DEFAULT_DB: &str = "postgres";

/// PostgreSQL compute definition provider. Supplies the definition and
/// provider-specific behaviour (connection string, name, default port).
#[derive(Debug)]
pub struct PostgresqlProvider;

impl PostgresqlProvider {
    pub fn new() -> Self {
        Self
    }

    fn definition_impl() -> ComputeDefinition {
        ComputeDefinition {
            image: DEFAULT_IMAGE.to_string(),
            env: vec![
                EnvVar {
                    name: ENV_USER.to_string(),
                    default: Some(DEFAULT_USER.to_string()),
                },
                EnvVar {
                    name: ENV_PASSWORD.to_string(),
                    default: Some(DEFAULT_PASSWORD.to_string()),
                },
                EnvVar {
                    name: ENV_DB.to_string(),
                    default: Some(DEFAULT_DB.to_string()),
                },
            ],
            ports: vec![PortMapping {
                compute_port: 5432,
                host_port: None,
            }],
            data_dir: PathBuf::from(CONTAINER_DATA_DIR),
            host_data_dir: None, // set by caller at provision time
            logs_dir: None,
            conf_dir: None,
            args: vec![],
        }
    }

    fn default_args_impl() -> Vec<DatabaseProviderArg> {
        vec![
            DatabaseProviderArg {
                name: "-c".into(),
                value: "shared_buffers=32MB".into(),
            },
            DatabaseProviderArg {
                name: "-c".into(),
                value: "work_mem=2MB".into(),
            },
            DatabaseProviderArg {
                name: "-c".into(),
                value: "maintenance_work_mem=4MB".into(),
            },
            DatabaseProviderArg {
                name: "-c".into(),
                value: "wal_buffers=4MB".into(),
            },
            DatabaseProviderArg {
                name: "-c".into(),
                value: "max_wal_size=128MB".into(),
            },
            DatabaseProviderArg {
                name: "-c".into(),
                value: "checkpoint_timeout=15min".into(),
            },
            DatabaseProviderArg {
                name: "-c".into(),
                value: "checkpoint_completion_target=0.9".into(),
            },
            DatabaseProviderArg {
                name: "-c".into(),
                value: "synchronous_commit=on".into(),
            },
            DatabaseProviderArg {
                name: "-c".into(),
                value: "max_connections=10".into(),
            },
            DatabaseProviderArg {
                name: "-c".into(),
                value: "max_parallel_workers=0".into(),
            },
            DatabaseProviderArg {
                name: "-c".into(),
                value: "max_parallel_workers_per_gather=0".into(),
            },
            DatabaseProviderArg {
                name: "-c".into(),
                value: "idle_in_transaction_session_timeout=60s".into(),
            },
            DatabaseProviderArg {
                name: "-c".into(),
                value: "log_min_duration_statement=1000".into(),
            },
            DatabaseProviderArg {
                name: "-c".into(),
                value: "autovacuum=on".into(),
            },
            DatabaseProviderArg {
                name: "-c".into(),
                value: "full_page_writes=on".into(),
            },
        ]
    }
}

impl Default for PostgresqlProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl DatabaseProvider for PostgresqlProvider {
    fn name(&self) -> &str {
        NAME
    }

    fn definition(&self) -> ComputeDefinition {
        let mut def = Self::definition_impl();
        def.args = self
            .default_args()
            .into_iter()
            .flat_map(|a| [a.name, a.value])
            .collect();
        def
    }

    fn default_port(&self) -> u16 {
        5432
    }

    fn default_args(&self) -> Vec<DatabaseProviderArg> {
        Self::default_args_impl()
    }

    fn default_signal(&self) -> u32 {
        SIGTERM
    }

    fn connection_string(
        &self,
        params: &ConnectionParams,
    ) -> std::result::Result<String, ProviderError> {
        let user = params.get_env(ENV_USER).unwrap_or(DEFAULT_USER);
        let password = params.get_env(ENV_PASSWORD).unwrap_or(DEFAULT_PASSWORD);
        let db = params.get_env(ENV_DB).unwrap_or(DEFAULT_DB);
        Ok(format!(
            "postgresql://{}:{}@{}:{}/{}",
            user, password, params.host, params.port, db
        ))
    }

    fn supported_versions(&self) -> Vec<String> {
        vec![
            "13".into(),
            "14".into(),
            "15".into(),
            "16".into(),
            "17".into(),
            "18".into(),
        ]
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
                id: "auto-scaling".into(),
                description: "Automatic resource scaling.".into(),
            },
            SupportedFeature {
                id: "performance-profile".into(),
                description: "Performance tuning profiles.".into(),
            },
            SupportedFeature {
                id: "backup".into(),
                description: "Backup and restore.".into(),
            },
            SupportedFeature {
                id: "import".into(),
                description: "Data import from external sources.".into(),
            },
            SupportedFeature {
                id: "replication".into(),
                description: "Replication and high availability.".into(),
            },
            SupportedFeature {
                id: "ai-agents".into(),
                description: "AI agent integration.".into(),
            },
        ]
    }

    fn prepare_for_snapshot(&self, _params: &ConnectionParams) -> Result<Vec<String>> {
        // Use TCP (127.0.0.1) + env vars so the command works when run via docker exec as root.
        // Peer auth would fail for root; password auth over TCP works.
        Ok(vec![
            "PGPASSWORD=\"$POSTGRES_PASSWORD\" psql -h 127.0.0.1 -U \"$POSTGRES_USER\" -d \"$POSTGRES_DB\" -c \"CHECKPOINT;\""
                .to_string(),
        ])
    }

    // -----------------------------------------------------------------------
    // Import / Export
    // -----------------------------------------------------------------------

    fn supported_export_formats(&self) -> Vec<DataFormat> {
        vec![
            DataFormat {
                id: "sql".into(),
                description: "Plain-text SQL dump (pg_dump --format=plain).".into(),
                file_extension: ".sql".into(),
            },
            DataFormat {
                id: "custom".into(),
                description: "PostgreSQL custom binary format (pg_dump --format=custom).".into(),
                file_extension: ".dump".into(),
            },
            DataFormat {
                id: "schema".into(),
                description: "Schema-only DDL dump (pg_dump --schema-only).".into(),
                file_extension: ".sql".into(),
            },
        ]
    }

    fn supported_import_formats(&self) -> Vec<DataFormat> {
        vec![
            DataFormat {
                id: "sql".into(),
                description: "Plain-text SQL file (loaded via psql -f).".into(),
                file_extension: ".sql".into(),
            },
            DataFormat {
                id: "custom".into(),
                description: "PostgreSQL custom binary dump (loaded via pg_restore).".into(),
                file_extension: ".dump".into(),
            },
            DataFormat {
                id: "csv".into(),
                description: "CSV file (loaded via COPY with HEADER).".into(),
                file_extension: ".csv".into(),
            },
        ]
    }

    fn export_spec(
        &self,
        params: &ConnectionParams,
        format: &str,
    ) -> std::result::Result<ExportSpec, ProviderError> {
        let user = params.get_env(ENV_USER).unwrap_or(DEFAULT_USER);
        let password = params.get_env(ENV_PASSWORD).unwrap_or(DEFAULT_PASSWORD);
        let db = params.get_env(ENV_DB).unwrap_or(DEFAULT_DB);

        let (pg_format, filename, schema_only) = match format {
            "sql" => ("plain", "export.sql", false),
            "custom" => ("custom", "export.dump", false),
            "schema" => ("plain", "schema.sql", true),
            other => return Err(ProviderError::UnsupportedFormat(other.to_string())),
        };

        let schema_flag = if schema_only { " --schema-only" } else { "" };

        Ok(ExportSpec {
            definition: ComputeDefinition {
                image: self.definition().image,
                env: vec![EnvVar {
                    name: "PGPASSWORD".into(),
                    default: Some(password.to_string()),
                }],
                ports: vec![],
                data_dir: PathBuf::from("/data"),
                host_data_dir: None, // set by orchestrator
                logs_dir: None,
                conf_dir: None,
                args: vec![],
            },
            command: format!(
                "pg_dump -h {host} -p {port} -U {user} -d {db} --format={fmt}{schema_flag} -f /data/{file}",
                host = params.host,
                port = params.port,
                user = user,
                db = db,
                fmt = pg_format,
                schema_flag = schema_flag,
                file = filename,
            ),
            output_filename: filename.to_string(),
        })
    }

    fn import_spec(
        &self,
        params: &ConnectionParams,
        format: &str,
        input_filename: &str,
    ) -> std::result::Result<ImportSpec, ProviderError> {
        let user = params.get_env(ENV_USER).unwrap_or(DEFAULT_USER);
        let password = params.get_env(ENV_PASSWORD).unwrap_or(DEFAULT_PASSWORD);
        let db = params.get_env(ENV_DB).unwrap_or(DEFAULT_DB);

        let command = match format {
            "sql" => format!(
                "psql -h {host} -p {port} -U {user} -d {db} -f /data/{file}",
                host = params.host,
                port = params.port,
                user = user,
                db = db,
                file = input_filename,
            ),
            "custom" => format!(
                "pg_restore -h {host} -p {port} -U {user} -d {db} /data/{file}",
                host = params.host,
                port = params.port,
                user = user,
                db = db,
                file = input_filename,
            ),
            "csv" => format!(
                "printf 'CREATE TABLE IF NOT EXISTS csv_import (id text, name text);\\n\\\\copy csv_import FROM ''/data/{}'' WITH (FORMAT csv, HEADER true);\\n' > /tmp/import.sql && psql -h {host} -p {port} -U {user} -d {db} -f /tmp/import.sql",
                input_filename,
                host = params.host,
                port = params.port,
                user = user,
                db = db,
            ),
            other => return Err(ProviderError::UnsupportedFormat(other.to_string())),
        };

        Ok(ImportSpec {
            definition: ComputeDefinition {
                image: self.definition().image,
                env: vec![EnvVar {
                    name: "PGPASSWORD".into(),
                    default: Some(password.to_string()),
                }],
                ports: vec![],
                data_dir: PathBuf::from("/data"),
                host_data_dir: None, // set by orchestrator
                logs_dir: None,
                conf_dir: None,
                args: vec![],
            },
            command,
            input_filename: input_filename.to_string(),
        })
    }

    // -----------------------------------------------------------------------
    // Query / Interactive Terminal
    // -----------------------------------------------------------------------

    fn query_client_command(
        &self,
        params: &ConnectionParams,
        query: Option<&str>,
    ) -> std::result::Result<std::process::Command, ProviderError> {
        let user = params.get_env(ENV_USER).unwrap_or(DEFAULT_USER);
        let password = params.get_env(ENV_PASSWORD).unwrap_or(DEFAULT_PASSWORD);
        let db = params.get_env(ENV_DB).unwrap_or(DEFAULT_DB);

        // Build psql command with connection parameters
        let mut cmd = std::process::Command::new("psql");
        cmd.arg(format!(
            "postgresql://{}:{}@{}:{}/{}",
            user, password, params.host, params.port, db
        ));

        // If a query is provided, execute it with -c; otherwise open interactive terminal
        if let Some(q) = query {
            cmd.arg("-c").arg(q);
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
            "SELECT COALESCE(json_agg(row_to_json(t)), '[]'::json)::text FROM (
                SELECT
                    oid::bigint as id,
                    nspname as name,
                    pg_get_userbyid(nspowner) as owner
                FROM pg_namespace
                WHERE nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast')
                    AND nspname NOT LIKE 'pg_temp_%'
                    AND nspname NOT LIKE 'pg_toast_temp_%'
                ORDER BY nspname
            ) t;"
                .to_string(),
        );

        // Tables query - returns JSON array of tables with metadata
        queries.insert(
            "tables".to_string(),
            "SELECT COALESCE(json_agg(row_to_json(t)), '[]'::json)::text FROM (
                SELECT
                    c.oid::bigint as id,
                    n.nspname as schema,
                    c.relname as name,
                    false as rls_enabled,
                    false as rls_forced,
                    COALESCE(pg_total_relation_size(c.oid), 0) as bytes,
                    COALESCE(pg_size_pretty(pg_total_relation_size(c.oid)), '0 bytes') as size,
                    COALESCE(s.n_live_tup, 0)::bigint as live_rows_estimate,
                    COALESCE(s.n_dead_tup, 0)::bigint as dead_rows_estimate,
                    obj_description(c.oid, 'pg_class') as comment,
                    '[]'::json as primary_keys,
                    '[]'::json as relationships
                FROM pg_class c
                JOIN pg_namespace n ON n.oid = c.relnamespace
                LEFT JOIN pg_stat_user_tables s ON s.relid = c.oid
                WHERE c.relkind = 'r'
                    AND n.nspname NOT IN ('pg_catalog', 'information_schema')
                ORDER BY n.nspname, c.relname
            ) t;"
                .to_string(),
        );

        // Columns query - returns JSON array of columns with full metadata
        queries.insert(
            "columns".to_string(),
            "SELECT COALESCE(json_agg(row_to_json(t)), '[]'::json)::text FROM (
                SELECT
                    format('%s.%s.%s', table_schema, table_name, column_name) as id,
                    (
                        SELECT c.oid::bigint
                        FROM pg_class c
                        JOIN pg_namespace n ON n.oid = c.relnamespace
                        WHERE n.nspname = cols.table_schema AND c.relname = cols.table_name
                    ) as table_id,
                    table_schema as schema,
                    table_name as \"table\",
                    column_name as name,
                    ordinal_position,
                    udt_name as data_type,
                    udt_name as format,
                    COALESCE(is_identity = 'YES', false) as is_identity,
                    identity_generation as identity_generation,
                    COALESCE(is_generated = 'ALWAYS', false) as is_generated,
                    COALESCE(is_nullable = 'YES', false) as is_nullable,
                    COALESCE(is_updatable = 'YES', false) as is_updatable,
                    false as is_unique,
                    NULL as \"check\",
                    CASE
                        WHEN column_default IS NULL THEN NULL
                        ELSE to_jsonb(column_default)
                    END as default_value,
                    '[]'::json as enums,
                    NULL as comment
                FROM information_schema.columns cols
                WHERE table_schema NOT IN ('pg_catalog', 'information_schema')
                ORDER BY table_schema, table_name, ordinal_position
            ) t;"
                .to_string(),
        );

        queries
    }

    fn schema_extraction_spec(
        &self,
        params: &ConnectionParams,
    ) -> std::result::Result<Option<SchemaExtractionSpec>, ProviderError> {
        let user = params.get_env(ENV_USER).unwrap_or(DEFAULT_USER);
        let password = params.get_env(ENV_PASSWORD).unwrap_or(DEFAULT_PASSWORD);
        let db = params.get_env(ENV_DB).unwrap_or(DEFAULT_DB);
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

        // Run schema extraction inside a container (no psql on host required).
        // Output uses delimiters for parsing.
        let command = format!(
            r#"echo "GFS_SCHEMA_VERSION"
PGPASSWORD="{password}" psql -h {host} -p {port} -U {user} -d {db} -t -A -c "SELECT version();"
echo "GFS_SCHEMA_SCHEMAS"
PGPASSWORD="{password}" psql -h {host} -p {port} -U {user} -d {db} -t -A -c "$(cat <<'SCHEMAS_EOF'
{schemas_query}
SCHEMAS_EOF
)"
echo "GFS_SCHEMA_TABLES"
PGPASSWORD="{password}" psql -h {host} -p {port} -U {user} -d {db} -t -A -c "$(cat <<'TABLES_EOF'
{tables_query}
TABLES_EOF
)"
echo "GFS_SCHEMA_COLUMNS"
PGPASSWORD="{password}" psql -h {host} -p {port} -U {user} -d {db} -t -A -c "$(cat <<'COLUMNS_EOF'
{columns_query}
COLUMNS_EOF
)""#,
            password = password,
            host = params.host,
            port = params.port,
            user = user,
            db = db,
            schemas_query = schemas_q,
            tables_query = tables_q,
            columns_query = columns_q,
        );

        Ok(Some(SchemaExtractionSpec {
            definition: ComputeDefinition {
                image: self.definition().image,
                env: vec![EnvVar {
                    name: "PGPASSWORD".into(),
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

/// Registers the PostgreSQL provider in `registry` under the name `"postgres"`.
pub fn register(registry: &impl DatabaseProviderRegistry) -> Result<()> {
    registry.register(Arc::new(PostgresqlProvider::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_string_uses_defaults() {
        let provider = PostgresqlProvider::new();
        let params = ConnectionParams {
            host: "localhost".to_string(),
            port: 5432,
            env: vec![],
        };
        let s = provider.connection_string(&params).unwrap();
        assert_eq!(s, "postgresql://postgres:postgres@localhost:5432/postgres");
    }

    #[test]
    fn connection_string_uses_env_overrides() {
        let provider = PostgresqlProvider::new();
        let params = ConnectionParams {
            host: "db.example.com".to_string(),
            port: 15432,
            env: vec![
                ("POSTGRES_USER".to_string(), "myuser".to_string()),
                ("POSTGRES_PASSWORD".to_string(), "secret".to_string()),
                ("POSTGRES_DB".to_string(), "mydb".to_string()),
            ],
        };
        let s = provider.connection_string(&params).unwrap();
        assert_eq!(s, "postgresql://myuser:secret@db.example.com:15432/mydb");
    }

    #[test]
    fn name_and_default_port() {
        let provider = PostgresqlProvider::new();
        assert_eq!(provider.name(), "postgres");
        assert_eq!(provider.default_port(), 5432);
    }

    #[test]
    fn supported_versions_non_empty() {
        let provider = PostgresqlProvider::new();
        let versions = provider.supported_versions();
        assert!(!versions.is_empty());
        assert!(versions.contains(&"16".to_string()));
    }

    #[test]
    fn supported_features_contains_tls_and_schema() {
        let provider = PostgresqlProvider::new();
        let features = provider.supported_features();
        let ids: Vec<_> = features.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&"tls"));
        assert!(ids.contains(&"schema"));
    }

    #[test]
    fn feature_description_returns_some_for_tls() {
        let provider = PostgresqlProvider::new();
        let desc = provider.feature_description("tls");
        assert!(desc.is_some());
        assert!(desc.unwrap().contains("TLS"));
    }

    #[test]
    fn default_signal_is_sigterm() {
        let provider = PostgresqlProvider::new();
        assert_eq!(provider.default_signal(), SIGTERM);
    }

    #[test]
    fn default_args_non_empty_and_definition_includes_flattened_args() {
        let provider = PostgresqlProvider::new();
        let args = provider.default_args();
        assert!(!args.is_empty());
        assert!(args.iter().all(|a| a.name == "-c"));
        let def = provider.definition();
        assert_eq!(def.args.len(), args.len() * 2);
        assert_eq!(def.args.first(), Some(&"-c".to_string()));
        assert_eq!(def.args.get(1), Some(&"shared_buffers=32MB".to_string()));
    }

    #[test]
    fn prepare_for_snapshot_returns_checkpoint_command_over_tcp() {
        let provider = PostgresqlProvider::new();
        let params = ConnectionParams {
            host: "localhost".to_string(),
            port: 5432,
            env: vec![],
        };
        let commands = provider.prepare_for_snapshot(&params).unwrap();
        assert_eq!(commands.len(), 1);
        let cmd = &commands[0];
        assert!(cmd.contains("PGPASSWORD="), "uses password from env");
        assert!(
            cmd.contains("-h 127.0.0.1"),
            "uses TCP to avoid peer auth in docker exec"
        );
        assert!(cmd.contains("$POSTGRES_USER"));
        assert!(cmd.contains("$POSTGRES_DB"));
        assert!(cmd.contains("CHECKPOINT;"));
    }

    #[test]
    fn supported_export_formats_includes_sql_and_custom() {
        let provider = PostgresqlProvider::new();
        let formats = provider.supported_export_formats();
        let ids: Vec<_> = formats.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&"sql"));
        assert!(ids.contains(&"custom"));
    }

    #[test]
    fn supported_import_formats_includes_sql_and_custom() {
        let provider = PostgresqlProvider::new();
        let formats = provider.supported_import_formats();
        let ids: Vec<_> = formats.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&"sql"));
        assert!(ids.contains(&"custom"));
    }

    #[test]
    fn export_spec_sql_produces_pg_dump_plain() {
        let provider = PostgresqlProvider::new();
        let params = ConnectionParams {
            host: "172.17.0.2".into(),
            port: 5432,
            env: vec![
                ("POSTGRES_USER".into(), "myuser".into()),
                ("POSTGRES_PASSWORD".into(), "secret".into()),
                ("POSTGRES_DB".into(), "mydb".into()),
            ],
        };
        let spec = provider.export_spec(&params, "sql").unwrap();
        assert!(spec.command.contains("pg_dump"));
        assert!(spec.command.contains("--format=plain"));
        assert!(spec.command.contains("-h 172.17.0.2"));
        assert!(spec.command.contains("-U myuser"));
        assert!(spec.command.contains("-d mydb"));
        assert_eq!(spec.output_filename, "export.sql");
        assert_eq!(spec.definition.data_dir.to_string_lossy(), "/data");
        assert!(spec.definition.host_data_dir.is_none());
    }

    #[test]
    fn export_spec_custom_produces_pg_dump_custom() {
        let provider = PostgresqlProvider::new();
        let params = ConnectionParams {
            host: "172.17.0.2".into(),
            port: 5432,
            env: vec![],
        };
        let spec = provider.export_spec(&params, "custom").unwrap();
        assert!(spec.command.contains("--format=custom"));
        assert_eq!(spec.output_filename, "export.dump");
    }

    #[test]
    fn import_spec_sql_produces_psql_command() {
        let provider = PostgresqlProvider::new();
        let params = ConnectionParams {
            host: "172.17.0.2".into(),
            port: 5432,
            env: vec![
                ("POSTGRES_USER".into(), "myuser".into()),
                ("POSTGRES_PASSWORD".into(), "secret".into()),
                ("POSTGRES_DB".into(), "mydb".into()),
            ],
        };
        let spec = provider.import_spec(&params, "sql", "import.sql").unwrap();
        assert!(spec.command.contains("psql"));
        assert!(spec.command.contains("-f /data/import.sql"));
        assert!(spec.command.contains("-h 172.17.0.2"));
        assert!(spec.command.contains("-U myuser"));
        assert_eq!(spec.input_filename, "import.sql");
    }

    #[test]
    fn import_spec_custom_produces_pg_restore_command() {
        let provider = PostgresqlProvider::new();
        let params = ConnectionParams {
            host: "172.17.0.2".into(),
            port: 5432,
            env: vec![],
        };
        let spec = provider
            .import_spec(&params, "custom", "import.dump")
            .unwrap();
        assert!(spec.command.contains("pg_restore"));
        assert!(spec.command.contains("/data/import.dump"));
        assert_eq!(spec.input_filename, "import.dump");
    }

    #[test]
    fn schema_extraction_spec_returns_some_with_delimiters() {
        let provider = PostgresqlProvider::new();
        let params = ConnectionParams {
            host: "172.17.0.2".into(),
            port: 5432,
            env: vec![
                ("POSTGRES_USER".into(), "myuser".into()),
                ("POSTGRES_PASSWORD".into(), "secret".into()),
                ("POSTGRES_DB".into(), "mydb".into()),
            ],
        };
        let spec = provider.schema_extraction_spec(&params).unwrap();
        let spec = spec.expect("postgres provider supports schema extraction");
        assert_eq!(spec.definition.image, "postgres:latest");
        assert!(spec.command.contains("GFS_SCHEMA_VERSION"));
        assert!(spec.command.contains("GFS_SCHEMA_SCHEMAS"));
        assert!(spec.command.contains("GFS_SCHEMA_TABLES"));
        assert!(spec.command.contains("GFS_SCHEMA_COLUMNS"));
        assert!(spec.command.contains("psql"));
        assert!(spec.command.contains("-h 172.17.0.2"));
        assert!(spec.command.contains("-U myuser"));
        assert!(spec.command.contains("-d mydb"));
    }

    #[test]
    fn export_spec_unsupported_format_returns_error() {
        let provider = PostgresqlProvider::new();
        let params = ConnectionParams {
            host: "172.17.0.2".into(),
            port: 5432,
            env: vec![],
        };
        let result = provider.export_spec(&params, "csv");
        assert!(matches!(result, Err(ProviderError::UnsupportedFormat(_))));
    }

    #[test]
    fn import_spec_csv_produces_copy_command() {
        let provider = PostgresqlProvider::new();
        let params = ConnectionParams {
            host: "172.17.0.2".into(),
            port: 5432,
            env: vec![
                ("POSTGRES_USER".into(), "myuser".into()),
                ("POSTGRES_PASSWORD".into(), "secret".into()),
                ("POSTGRES_DB".into(), "mydb".into()),
            ],
        };
        let spec = provider.import_spec(&params, "csv", "import.csv").unwrap();
        assert!(
            spec.command
                .contains("CREATE TABLE IF NOT EXISTS csv_import")
        );
        assert!(spec.command.contains("/data/import.csv"));
        assert!(spec.command.contains("FORMAT csv"));
        assert!(spec.command.contains("HEADER true"));
        assert!(spec.command.contains("psql"));
        assert_eq!(spec.input_filename, "import.csv");
    }

    #[test]
    fn import_spec_sql_uses_arbitrary_filename() {
        let provider = PostgresqlProvider::new();
        let params = ConnectionParams {
            host: "localhost".into(),
            port: 5432,
            env: vec![],
        };
        let spec = provider
            .import_spec(&params, "sql", "demo-small-en-20170815.sql")
            .unwrap();
        assert!(spec.command.contains("/data/demo-small-en-20170815.sql"));
        assert_eq!(spec.input_filename, "demo-small-en-20170815.sql");
    }

    #[test]
    fn import_spec_unsupported_format_returns_error() {
        let provider = PostgresqlProvider::new();
        let params = ConnectionParams {
            host: "172.17.0.2".into(),
            port: 5432,
            env: vec![],
        };
        let result = provider.import_spec(&params, "unknown", "file.sql");
        assert!(matches!(result, Err(ProviderError::UnsupportedFormat(_))));
    }

    #[test]
    fn export_spec_sidecar_uses_same_image_as_definition() {
        let provider = PostgresqlProvider::new();
        let params = ConnectionParams {
            host: "172.17.0.2".into(),
            port: 5432,
            env: vec![],
        };
        let spec = provider.export_spec(&params, "sql").unwrap();
        assert_eq!(spec.definition.image, provider.definition().image);
    }
}
