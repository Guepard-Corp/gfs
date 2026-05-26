//! PostgreSQL provider: compute definition, connection string, and related behaviour.

use std::path::PathBuf;
use std::sync::Arc;

use gfs_domain::ports::compute::{ComputeDefinition, EnvVar, PortMapping};
use gfs_domain::ports::database_provider::{
    CloneSpec, ConnectionParams, DataFormat, DatabaseProvider, DatabaseProviderArg,
    DatabaseProviderRegistry, ExportSpec, ImportSpec, ProviderError, RemoteSource, Result, SIGTERM,
    SchemaExtractionSpec, SupportedFeature,
};

const NAME: &str = "postgres";

/// Default PostgreSQL image (official image, current LTS-alpine).
const DEFAULT_IMAGE: &str = "postgres:latest";

/// Path inside the container where PostgreSQL stores data (PGDATA).
const CONTAINER_DATA_DIR: &str = "/var/lib/postgresql/data";

const ENV_USER: &str = "POSTGRES_USER";
const ENV_PASSWORD: &str = "POSTGRES_PASSWORD";
const ENV_DB: &str = "POSTGRES_DB";
const ENV_PGDATA: &str = "PGDATA";

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
                EnvVar {
                    name: ENV_PGDATA.to_string(),
                    default: Some(CONTAINER_DATA_DIR.to_string()),
                },
            ],
            ports: vec![PortMapping {
                compute_port: 5432,
                host_port: None,
            }],
            data_dir: PathBuf::from(CONTAINER_DATA_DIR),
            host_data_dir: None, // set by caller at provision time
            user: None,
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

/// Resolve `(user, password, db)` from connection params, falling back to the
/// provider defaults. Shared by every spec that builds a psql/pg_* command.
fn conn_creds(params: &ConnectionParams) -> (&str, &str, &str) {
    (
        params.get_env(ENV_USER).unwrap_or(DEFAULT_USER),
        params.get_env(ENV_PASSWORD).unwrap_or(DEFAULT_PASSWORD),
        params.get_env(ENV_DB).unwrap_or(DEFAULT_DB),
    )
}

/// Build the ephemeral tool-sidecar `ComputeDefinition` shared by export,
/// import, schema-extraction, and clone: the database image with `PGPASSWORD`
/// set and the data exchange directory mounted at `data_dir`.
fn sidecar_definition(image: String, password: &str, data_dir: &str) -> ComputeDefinition {
    ComputeDefinition {
        image,
        env: vec![EnvVar {
            name: "PGPASSWORD".into(),
            default: Some(password.to_string()),
        }],
        ports: vec![],
        data_dir: PathBuf::from(data_dir),
        host_data_dir: None, // set by the orchestrator when needed
        user: None,
        logs_dir: None,
        conf_dir: None,
        args: vec![],
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
        let (user, password, db) = conn_creds(params);
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

    fn data_dir_owner(&self) -> Option<&'static str> {
        Some("postgres:postgres")
    }

    fn container_startup_probes(&self) -> &'static [&'static str] {
        &[
            "pg_isready -h 127.0.0.1 -U \"$POSTGRES_USER\" -d \"$POSTGRES_DB\" >/dev/null",
            "PGPASSWORD=\"$POSTGRES_PASSWORD\" psql -h 127.0.0.1 -U \"$POSTGRES_USER\" -d \"$POSTGRES_DB\" -v ON_ERROR_STOP=1 -c \"SELECT 1;\" >/dev/null",
        ]
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
        let (user, password, db) = conn_creds(params);

        let (pg_format, filename, schema_only) = match format {
            "sql" => ("plain", "export.sql", false),
            "custom" => ("custom", "export.dump", false),
            "schema" => ("plain", "schema.sql", true),
            other => return Err(ProviderError::UnsupportedFormat(other.to_string())),
        };

        let schema_flag = if schema_only { " --schema-only" } else { "" };

        Ok(ExportSpec {
            definition: sidecar_definition(self.definition().image, password, "/data"),
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
        let (user, password, db) = conn_creds(params);

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
            definition: sidecar_definition(self.definition().image, password, "/data"),
            command,
            input_filename: input_filename.to_string(),
        })
    }

    // -----------------------------------------------------------------------
    // Lazy clone (RFC 008)
    // -----------------------------------------------------------------------

    fn clone_bootstrap_spec(
        &self,
        local: &ConnectionParams,
        remote: &RemoteSource,
    ) -> std::result::Result<CloneSpec, ProviderError> {
        let (user, password, db) = conn_creds(local);

        let bootstrap_sql = build_clone_bootstrap_sql(remote);

        // The sidecar runs psql against the LOCAL GFS database and feeds the
        // bootstrap script via a quoted heredoc (no shell expansion inside).
        let command = format!(
            "psql -h {host} -p {port} -U {user} -d {db} -v ON_ERROR_STOP=1 <<'GFS_CLONE_BOOTSTRAP'\n{sql}\nGFS_CLONE_BOOTSTRAP\n",
            host = local.host,
            port = local.port,
            user = user,
            db = db,
            sql = bootstrap_sql,
        );

        Ok(CloneSpec {
            definition: sidecar_definition(self.definition().image, password, "/data"),
            command,
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
        let (user, password, db) = conn_creds(params);

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
        let (user, password, db) = conn_creds(params);
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
            definition: sidecar_definition(self.definition().image, password, "/tmp"),
            command,
        }))
    }
}

/// Registers the PostgreSQL provider in `registry` under the name `"postgres"`.
pub fn register(registry: &impl DatabaseProviderRegistry) -> Result<()> {
    registry.register(Arc::new(PostgresqlProvider::new()))
}

// ---------------------------------------------------------------------------
// Lazy-clone bootstrap SQL generation (RFC 008)
// ---------------------------------------------------------------------------

/// The overlay bootstrap template, kept as a real `.sql` file (proper syntax
/// highlighting / linting). `__PLACEHOLDER__` sentinels are substituted by
/// `build_clone_bootstrap_sql`.
const CLONE_BOOTSTRAP_TMPL: &str = include_str!("clone_bootstrap.sql");

/// Escape a value for use inside a single-quoted SQL string literal.
fn sql_lit(s: &str) -> String {
    s.replace('\'', "''")
}

/// Build the bootstrap SQL run inside the local GFS database to set up a lazy
/// (copy-on-read) clone of `remote`, using the **overlay** mechanic.
///
/// Installs `postgres_fdw` + `dblink`, imports the remote schema as foreign
/// tables, and — for each remote table with a single-column primary key —
/// creates a local store, a delete-tombstone table, an updatable overlay view
/// (local wins; remote rows only if neither local nor tombstoned), and
/// `INSTEAD OF` triggers for copy-on-write. No data is copied.
///
/// Correctness is guaranteed by the view (disjoint row sets), independent of
/// hydration. The mechanic is the one validated by
/// `docs/rfcs/008-remote-clone/poc-overlay`.
fn build_clone_bootstrap_sql(remote: &RemoteSource) -> String {
    // dblink connection string, used only for read-only introspection of the
    // remote (schema/table/key discovery).
    let conn = format!(
        "host={} port={} dbname={} user={} password={}",
        remote.host, remote.port, remote.dbname, remote.user, remote.password,
    );

    // SQL array literal of schemas to mirror, or NULL meaning "all user schemas".
    let schemas_array = if remote.schemas.is_empty() {
        "NULL::text[]".to_string()
    } else {
        let items: Vec<String> = remote
            .schemas
            .iter()
            .map(|s| format!("'{}'", sql_lit(s)))
            .collect();
        format!("ARRAY[{}]::text[]", items.join(", "))
    };

    let template = CLONE_BOOTSTRAP_TMPL;

    template
        .replace("__RHOST__", &sql_lit(&remote.host))
        .replace("__RPORT__", &remote.port.to_string())
        .replace("__RDB__", &sql_lit(&remote.dbname))
        .replace("__RUSER__", &sql_lit(&remote.user))
        .replace("__RPASS__", &sql_lit(&remote.password))
        .replace("__CONN__", &sql_lit(&conn))
        .replace("__SCHEMAS_ARRAY__", &schemas_array)
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
    fn definition_pins_pgdata_to_mounted_data_dir() {
        // PostgreSQL 18's image moved the default PGDATA; pinning PGDATA to the
        // bind-mounted path keeps the data dir consistent across all versions.
        let provider = PostgresqlProvider::new();
        let def = provider.definition();
        let pgdata = def
            .env
            .iter()
            .find(|e| e.name == "PGDATA")
            .expect("PGDATA env var must be set");
        assert_eq!(pgdata.default.as_deref(), Some(CONTAINER_DATA_DIR));
        assert_eq!(def.data_dir.to_string_lossy(), CONTAINER_DATA_DIR);
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

    // -- lazy clone (RFC 008) ------------------------------------------------

    fn sample_remote() -> RemoteSource {
        RemoteSource {
            host: "rds.example.com".into(),
            port: 5432,
            dbname: "shop".into(),
            user: "reader".into(),
            password: "p@ss".into(),
            schemas: vec!["public".into()],
        }
    }

    fn local_params() -> ConnectionParams {
        ConnectionParams {
            host: "172.17.0.2".into(),
            port: 5432,
            env: vec![
                ("POSTGRES_USER".into(), "postgres".into()),
                ("POSTGRES_PASSWORD".into(), "localpw".into()),
                ("POSTGRES_DB".into(), "gfs".into()),
            ],
        }
    }

    #[test]
    fn clone_bootstrap_sql_substitutes_remote() {
        let sql = build_clone_bootstrap_sql(&sample_remote());
        // FDW server + mapping carry the remote connection params.
        assert!(sql.contains("OPTIONS (host 'rds.example.com', port '5432', dbname 'shop')"));
        assert!(sql.contains("OPTIONS (user 'reader', password 'p@ss')"));
        // Requested schemas become an array literal driving the per-schema import.
        assert!(sql.contains("ARRAY['public']::text[]"));
        // Per-table import (LIMIT TO) so one bad table cannot abort the clone.
        assert!(sql.contains("IMPORT FOREIGN SCHEMA %I LIMIT TO (%I) FROM SERVER gfs_remote_srv INTO %I"));
        // Resilience hooks: skip un-importable tables / overlays.
        assert!(sql.contains("CREATE EXTENSION IF NOT EXISTS %I"));
        assert!(sql.contains("to_regclass(fq_remote) IS NULL"));
        // dblink introspection connection string is present.
        assert!(sql.contains("host=rds.example.com port=5432 dbname=shop user=reader password=p@ss"));
        // No leftover placeholders.
        assert!(!sql.contains("__"));
    }

    #[test]
    fn clone_bootstrap_sql_all_schemas_when_none_requested() {
        let mut remote = sample_remote();
        remote.schemas = vec![];
        let sql = build_clone_bootstrap_sql(&remote);
        // Empty list → NULL sentinel → gfs_sync.clone discovers all user schemas.
        assert!(sql.contains("gfs_sync.clone('"));
        assert!(sql.contains("NULL::text[]"));
        assert!(sql.contains("array_agg(nspname)"));
    }

    #[test]
    fn clone_bootstrap_sql_builds_overlay() {
        let sql = build_clone_bootstrap_sql(&sample_remote());
        assert!(sql.contains("CREATE EXTENSION IF NOT EXISTS postgres_fdw;"));
        assert!(sql.contains("CREATE EXTENSION IF NOT EXISTS dblink;"));
        // Overlay objects: local store, tombstones, view, copy-on-write triggers
        // (schema-qualified, %I.%I).
        assert!(sql.contains("CREATE TABLE %I.%I (LIKE %s INCLUDING DEFAULTS INCLUDING GENERATED)"));
        // Generated columns are excluded from the writable column list (recomputed).
        assert!(sql.contains("AND attgenerated = ''"));
        assert!(sql.contains("CREATE VIEW %I.%I AS"));
        assert!(sql.contains("UNION ALL"));
        assert!(sql.contains("NOT EXISTS"));
        assert!(sql.contains("INSTEAD OF INSERT ON %I.%I"));
        assert!(sql.contains("INSTEAD OF UPDATE ON %I.%I"));
        assert!(sql.contains("INSTEAD OF DELETE ON %I.%I"));
        assert!(sql.contains("ON CONFLICT (%s)"));
        assert!(sql.contains("gfs_sync.table_meta"));
        // Composite-key support: discovery returns (schema, table, key columns).
        assert!(sql.contains("AS r(nsp text, tab text, keycols text[])"));
        // Per-schema shadow import.
        assert!(sql.contains("'gfs_remote_' || p_sch"));
        // Tier 3: logic lives in reusable gfs_sync.* functions orchestrated by clone().
        assert!(sql.contains("CREATE OR REPLACE FUNCTION gfs_sync.build_overlay("));
        assert!(sql.contains("CREATE OR REPLACE FUNCTION gfs_sync.clone(p_conn text, p_schemas text[])"));
        assert!(sql.contains("PERFORM gfs_sync.build_overlay(p_conn, rec.nsp, rec.tab, rec.keycols)"));
        // Remediations: any-role FDW access, local sequences, overlay comment.
        assert!(sql.contains("CREATE USER MAPPING FOR PUBLIC"));
        assert!(sql.contains("CREATE SEQUENCE %I.%I START WITH %s"));
        assert!(sql.contains("SET DEFAULT nextval(%L)"));
        // Non-sequence defaults (now(), uuid_generate_v4(), ...) are mirrored
        // onto both the view and the local store.
        assert!(sql.contains("ALTER VIEW %I.%I ALTER COLUMN %I SET DEFAULT %s"));
        assert!(sql.contains("ALTER TABLE %I.%I ALTER COLUMN %I SET DEFAULT %s"));
        assert!(sql.contains("a.attidentity NOT IN ('a','d')"));
        assert!(sql.contains("COMMENT ON VIEW %I.%I IS %L"));
        // Copy-on-read warming function is installed.
        assert!(sql.contains("CREATE OR REPLACE FUNCTION gfs_sync.warm_for_query(p_sql text)"));
        assert!(sql.contains("EXPLAIN (VERBOSE) "));
        // The flawed partition mechanic must be gone.
        assert!(!sql.contains("PARTITION BY RANGE"));
        assert!(!sql.contains("ATTACH PARTITION"));
        assert!(!sql.contains("hydrate_for_query"));
    }

    #[test]
    fn clone_bootstrap_sql_has_range_elision() {
        let sql = build_clone_bootstrap_sql(&sample_remote());
        // Cached-range metadata + constraint exclusion + the warm_range primitive
        // that hydrates a range and rebuilds the foreign table's exclusion CHECK.
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS gfs_sync.cached_range"));
        assert!(sql.contains("SET constraint_exclusion = on"));
        assert!(sql.contains("CREATE OR REPLACE FUNCTION gfs_sync.rebuild_exclusion(p_nsp text, p_tab text)"));
        assert!(sql.contains("CREATE OR REPLACE FUNCTION gfs_sync.warm_range(p_nsp text, p_tab text, p_lo text, p_hi text)"));
        assert!(sql.contains("ADD CONSTRAINT gfs_excl CHECK"));
        // Driveable by a low-privilege role (proxy/cron).
        assert!(sql.contains("SECURITY DEFINER"));
        assert!(sql.contains("GRANT EXECUTE ON FUNCTION gfs_sync.warm_range"));
        // Query-driven entry point: expands a query's key span to chunks.
        assert!(sql.contains("CREATE OR REPLACE FUNCTION gfs_sync.warm_query_chunks("));
        assert!(sql.contains("PERFORM gfs_sync.warm_range(sch, cur_tab"));
        assert!(sql.contains("GRANT EXECUTE ON FUNCTION gfs_sync.warm_query_chunks"));
        // Hydration writes an explicit non-generated column list (never SELECT *),
        // so warming tolerates STORED generated columns in the local store.
        assert!(sql.contains("CREATE OR REPLACE FUNCTION gfs_sync.writable_cols(p_relid regclass)"));
        assert!(sql.contains("AND attgenerated = ''"));
        assert!(!sql.contains("_local SELECT * FROM"));
        // CHECK rebuild is decoupled from hydration into a periodic refresher
        // (lock_timeout-bounded) instead of running on every warm.
        assert!(sql.contains("CREATE OR REPLACE FUNCTION gfs_sync.refresh_exclusions()"));
        assert!(sql.contains("SET LOCAL lock_timeout"));
        assert!(sql.contains("GRANT EXECUTE ON FUNCTION gfs_sync.refresh_exclusions"));
        // warm_range no longer rebuilds the CHECK itself.
        assert!(!sql.contains("PERFORM gfs_sync.rebuild_exclusion(p_nsp, p_tab)"));
        // Ranges are coalesced before each rebuild so the CHECK stays compact.
        assert!(sql.contains("CREATE OR REPLACE FUNCTION gfs_sync.coalesce_ranges(p_nsp text, p_tab text)"));
        assert!(sql.contains("PERFORM gfs_sync.coalesce_ranges(rec.schema_name, rec.table_name)"));
        // refresh_exclusions skips unchanged tables (signature) and stays quiet.
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS gfs_sync.applied_exclusion"));
        assert!(sql.contains("CREATE OR REPLACE FUNCTION gfs_sync.exclusion_sig(p_nsp text, p_tab text)"));
        assert!(sql.contains("CONTINUE WHEN prev_sig IS NOT DISTINCT FROM cur_sig"));
        assert!(sql.contains("SET client_min_messages = 'warning'"));
        // whole_table strategy for non-range-able keys (uuid/text/composite):
        // hydrate everything + CHECK (false) so the foreign scan is always pruned.
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS gfs_sync.fully_cached"));
        assert!(sql.contains("CREATE OR REPLACE FUNCTION gfs_sync.warm_whole_table(p_nsp text, p_tab text)"));
        // Fully-cached tables drop the foreign branch from the view entirely.
        assert!(sql.contains("CREATE OR REPLACE VIEW %I.%I AS SELECT * FROM %I.%I"));
        assert!(sql.contains("GRANT EXECUTE ON FUNCTION gfs_sync.warm_whole_table"));
    }

    #[test]
    fn clone_bootstrap_sql_builds_composite_key_fragments() {
        let sql = build_clone_bootstrap_sql(&sample_remote());
        // Key-column fragments are built per key ordinal, so composite PKs work.
        assert!(sql.contains("unnest(p_keycols) WITH ORDINALITY AS u(kc, ord)"));
        // Key-change detection in the UPDATE trigger uses IS DISTINCT FROM per col.
        assert!(sql.contains("IS DISTINCT FROM OLD."));
        // Tombstone table preserves each key column's type (composite-aware).
        assert!(sql.contains("format_type(a.atttypid, a.atttypmod)"));
        // Non-key columns drive the upsert SET list.
        assert!(sql.contains("FILTER (WHERE NOT (attname = ANY(p_keycols)))"));
    }

    #[test]
    fn clone_bootstrap_sql_mirrors_only_non_sequence_defaults() {
        let sql = build_clone_bootstrap_sql(&sample_remote());
        // The default-mirroring loop selects column defaults that are NOT
        // identity and NOT a nextval(...) — those are handled by the sequence
        // loop above and must not be double-applied here.
        assert!(sql.contains("a.attidentity NOT IN ('a','d')"));
        assert!(sql.contains("NOT LIKE 'nextval(%"));
        assert!(sql.contains("pg_get_expr(ad.adbin, ad.adrelid)"));
    }

    #[test]
    fn clone_bootstrap_sql_mirrors_enum_types() {
        let sql = build_clone_bootstrap_sql(&sample_remote());
        // User-defined ENUMs are discovered (typtype 'e') and recreated locally
        // before import, preserving label order.
        assert!(sql.contains("t.typtype = 'e'"));
        assert!(sql.contains("array_agg(e.enumlabel ORDER BY e.enumsortorder)"));
        assert!(sql.contains("CREATE TYPE %I.%I AS ENUM (%s)"));
    }

    #[test]
    fn clone_bootstrap_sql_mirrors_domain_and_composite_types() {
        let sql = build_clone_bootstrap_sql(&sample_remote());
        // DOMAINs: base type + constraints recreated.
        assert!(sql.contains("t.typtype = 'd'"));
        assert!(sql.contains("CREATE DOMAIN %I.%I AS %s%s%s %s"));
        assert!(sql.contains("pg_get_constraintdef(c.oid)"));
        // COMPOSITEs: attribute list recreated, multi-pass for dependencies.
        assert!(sql.contains("t.typtype = 'c' AND c.relkind = 'c'"));
        assert!(sql.contains("CREATE TYPE %I.%I AS (%s)"));
        assert!(sql.contains("FOR pass IN 1..10 LOOP"));
        assert!(sql.contains("to_regtype(format('%I.%I', comptyp.nsp, comptyp.typ)) IS NOT NULL"));
    }

    #[test]
    fn clone_bootstrap_sql_escapes_single_quotes() {
        let mut remote = sample_remote();
        remote.password = "a'b".into();
        let sql = build_clone_bootstrap_sql(&remote);
        // Single quote is doubled inside the SQL string literal.
        assert!(sql.contains("password 'a''b'"));
    }

    #[test]
    fn clone_bootstrap_spec_wraps_sql_in_local_psql_heredoc() {
        let provider = PostgresqlProvider::new();
        let spec = provider
            .clone_bootstrap_spec(&local_params(), &sample_remote())
            .unwrap();
        // Connects to the LOCAL database.
        assert!(spec.command.contains("psql -h 172.17.0.2 -p 5432 -U postgres -d gfs"));
        assert!(spec.command.contains("<<'GFS_CLONE_BOOTSTRAP'"));
        // Local password is supplied via PGPASSWORD on the sidecar.
        assert!(spec.definition.env.iter().any(|e| e.name == "PGPASSWORD"
            && e.default.as_deref() == Some("localpw")));
        assert_eq!(spec.definition.image, provider.definition().image);
    }
}
