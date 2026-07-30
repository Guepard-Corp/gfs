//! PostgreSQL provider: compute definition, connection string, and related behaviour.

use std::path::PathBuf;
use std::sync::Arc;

use gfs_domain::model::db_user::{
    DeployEnvSpec, GrantSpec, GrantableObject, Privilege, RevokeSpec, RolePreset, RoleSpec,
};
use gfs_domain::ports::compute::{ComputeDefinition, EnvVar, PortMapping};
use gfs_domain::ports::database_provider::{
    CloneSpec, ConnectionParams, DataFormat, DatabaseProvider, DatabaseProviderArg,
    DatabaseProviderRegistry, ExportSpec, ImportSpec, ProviderError, RemoteSource, Result, SIGTERM,
    SchemaExtractionSpec, SupportedFeature,
};

const NAME: &str = "postgres";

/// Default PostgreSQL image. The tag here is only a fallback base — every
/// provisioning and sidecar-task site re-tags it with the repository's
/// configured `database_version` (see `task_image_for_version`, deploy, and
/// checkout), so the effective image follows the user's chosen PG version,
/// defaulting to the supported `17`.
const DEFAULT_IMAGE: &str = "gfs-postgres:17";

/// Path inside the container where PostgreSQL stores data (PGDATA).
const CONTAINER_DATA_DIR: &str = "/var/lib/postgresql/data";

const ENV_USER: &str = "POSTGRES_USER";
const ENV_PASSWORD: &str = "POSTGRES_PASSWORD";
const ENV_DB: &str = "POSTGRES_DB";
const ENV_PGDATA: &str = "PGDATA";

const DEFAULT_USER: &str = "postgres";
const DEFAULT_PASSWORD: &str = "postgres";
const DEFAULT_DB: &str = "postgres";

/// Shell fragment (POSIX `sh`) run at deploy bootstrap to confine the management
/// superuser (`${POSTGRES_USER}`) to the container's loopback. It prepends four
/// rules ahead of the existing pg_hba entries — `local`/`127.0.0.1`/`::1` allow,
/// then a non-loopback `reject` — so, under pg_hba first-match, the management
/// role authenticates over the loopback exec seam but is refused over the exposed
/// endpoint, while every client role falls through to the catch-all unchanged.
/// The rewrite truncates the file in place to preserve its owner and `0600` mode,
/// is guarded by a marker line for idempotency, and leaves the reload to the
/// caller (a following `SELECT pg_reload_conf()`).
const RESTRICT_MGMT_ROLE_TO_LOOPBACK: &str = concat!(
    r#"HBA="${PGDATA:-/var/lib/postgresql/data}/pg_hba.conf"; "#,
    r#"ADMIN="${POSTGRES_USER:-postgres}"; "#,
    r#"if ! grep -q "gfs-managed loopback-only ${ADMIN}" "$HBA" 2>/dev/null; then "#,
    r#"{ printf '# gfs-managed loopback-only %s\n' "$ADMIN"; "#,
    r#"printf 'local all "%s" trust\n' "$ADMIN"; "#,
    r#"printf 'host all "%s" 127.0.0.1/32 trust\n' "$ADMIN"; "#,
    r#"printf 'host all "%s" ::1/128 trust\n' "$ADMIN"; "#,
    r#"printf 'host all "%s" all reject\n' "$ADMIN"; "#,
    r#"cat "$HBA"; } > "$HBA.gfs" && cat "$HBA.gfs" > "$HBA" && rm -f "$HBA.gfs"; "#,
    r#"fi"#,
);

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
            labels: Default::default(),
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
                // Listen on all interfaces so the container is reachable via the
                // k8s Service/NodePort (and Docker port mapping). Without this a
                // fresh initdb defaults to localhost, so the pod runs healthy but
                // the CP's connection to the NodePort times out (deploy 500).
                name: "-c".into(),
                value: "listen_addresses=*".into(),
            },
            DatabaseProviderArg {
                name: "-c".into(),
                value: "shared_buffers=32MB".into(),
            },
            DatabaseProviderArg {
                // 16MB (vs the 2MB minimum): lets the planner pick a hash join
                // (O(N+M)) instead of a nested-loop join-filter (O(N×M)) when a
                // query still federates a multi-table join before warming has made
                // the tables local — the difference between a pegged core and a
                // bounded scan. Overridable via clone params.
                name: "-c".into(),
                value: "work_mem=16MB".into(),
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

/// Wrap a value in single quotes for safe use in a `sh -c` command, escaping any
/// embedded single quote (`'` -> `'\''`).
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Build the ephemeral tool-sidecar `ComputeDefinition` shared by export,
/// import, schema-extraction, and clone: the database image with `PGPASSWORD`
/// set and the data exchange directory mounted at `data_dir`.
fn sidecar_definition(image: String, password: &str, data_dir: &str) -> ComputeDefinition {
    ComputeDefinition {
        labels: Default::default(),
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

impl PostgresqlProvider {
    /// Single-line `psql -c` for k8s exec (stdin=false); heredocs hang there.
    fn psql_inline_instance_command(
        &self,
        sql: &str,
    ) -> std::result::Result<String, ProviderError> {
        let escaped = sql.replace('\\', "\\\\").replace('"', "\\\"");
        Ok(format!(
            r#"PGPASSWORD="${{POSTGRES_PASSWORD:-postgres}}" psql -h 127.0.0.1 -U "${{POSTGRES_USER:-postgres}}" -d "${{POSTGRES_DB:-postgres}}" -v ON_ERROR_STOP=1 -c "{escaped}""#
        ))
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

    /// PostgreSQL takes runtime settings as repeated `-c name=value` flags; the
    /// last occurrence wins, so these (appended after the defaults) override
    /// `default_args` (e.g. `max_connections=200`).
    fn render_param_overrides(
        &self,
        params: &std::collections::BTreeMap<String, String>,
    ) -> Vec<DatabaseProviderArg> {
        params
            .iter()
            .map(|(k, v)| DatabaseProviderArg {
                name: "-c".into(),
                value: format!("{k}={v}"),
            })
            .collect()
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

        // Step 1 — FAITHFUL schema: dump the remote's DDL (tables, triggers,
        // functions, indexes, constraints, sequences, types) and replay it onto
        // the local clone, so the source's real objects exist as real local heap
        // tables. The `gfs` copy-on-read extension then serves each one lazily via
        // its planner hook (no overlay views or INSTEAD OF triggers).
        // `--no-owner --no-privileges` avoids depending on remote roles;
        // restrict to the requested schemas when given.
        let schema_flags = remote
            .schemas
            .iter()
            .map(|s| format!(" -n {}", shell_single_quote(s)))
            .collect::<String>();
        let ssl_env = remote
            .sslmode
            .as_ref()
            .map(|m| format!("PGSSLMODE={} ", shell_single_quote(m)))
            .unwrap_or_default();
        let dump = format!(
            "{ssl_env}PGCONNECT_TIMEOUT=15 PGPASSWORD={rpass} pg_dump -h {rhost} -p {rport} -U {ruser} -d {rdb} --schema-only --no-owner --no-privileges{schemas} -f /tmp/gfs_faithful.sql",
            ssl_env = ssl_env,
            rpass = shell_single_quote(&remote.password),
            rhost = remote.host,
            rport = remote.port,
            ruser = remote.user,
            rdb = remote.dbname,
            schemas = schema_flags,
        );

        // Step 1b — sanitize the dump for cross-version replay. A pg_dump client
        // >= 17 unconditionally emits `SET transaction_timeout = 0;` in the header,
        // but that GUC only exists on a server >= 17. Replaying it onto an older
        // local server raises "unrecognized configuration parameter" — harmless to
        // the schema, noisy in the logs, and fatal if anything ever tightens the
        // replay to ON_ERROR_STOP. Strip the line; it is irrelevant to a DDL replay.
        let sanitize = "sed -i '/^SET transaction_timeout/d' /tmp/gfs_faithful.sql";

        // Step 2 — replay the faithful schema into the LOCAL database. Best-effort
        // (no ON_ERROR_STOP): an object that can't be recreated locally (e.g. a
        // missing extension) is skipped, and its table is later skipped during
        // copy-on-read registration, rather than aborting the whole clone.
        let replay = format!(
            "psql -h {host} -p {port} -U {user} -d {db} -f /tmp/gfs_faithful.sql || true",
            host = local.host,
            port = local.port,
            user = user,
            db = db,
        );

        // Step 3 — bootstrap the FDW + register each table for copy-on-read with the
        // `gfs` planner-hook extension (fed via a quoted heredoc; no shell expansion
        // inside). ON_ERROR_STOP=1 so a real failure fails the clone.
        let bootstrap = format!(
            "psql -h {host} -p {port} -U {user} -d {db} -v ON_ERROR_STOP=1 <<'GFS_CLONE_BOOTSTRAP'\n{sql}\nGFS_CLONE_BOOTSTRAP\n",
            host = local.host,
            port = local.port,
            user = user,
            db = db,
            sql = bootstrap_sql,
        );

        // Step 1c — wait for the clone to actually ACCEPT QUERIES before replaying
        // onto it. The engine's port can be open (and the framework's TCP readiness
        // probe satisfied) while postgres is still starting up, especially under
        // host load -- replaying then fails with "connection refused". Poll a real
        // SELECT 1 (same creds as the replay, via the sidecar's PGPASSWORD).
        let wait_clone = format!(
            "for i in $(seq 1 120); do if PGCONNECT_TIMEOUT=5 psql -h {host} -p {port} -U {user} -d {db} -c 'SELECT 1' >/dev/null 2>&1; then break; fi; sleep 1; done",
            host = local.host,
            port = local.port,
            user = user,
            db = db,
        );

        let command = format!(
            "set -e\nexport PGCONNECT_TIMEOUT=15\n{wait_clone}\n{dump}\n{sanitize}\n{replay}\n{bootstrap}"
        );

        Ok(CloneSpec {
            definition: sidecar_definition(self.definition().image, password, "/data"),
            command,
        })
    }

    fn supports_lazy_clone(&self) -> bool {
        true
    }

    fn lazy_clone_detach_in_instance_commands(
        &self,
    ) -> std::result::Result<Vec<String>, ProviderError> {
        const DETACH: &[&str] = &[
            "DROP SERVER IF EXISTS gfs_remote_srv CASCADE",
            "UPDATE gfs.clone_source SET whole_cached = true, no_partial = true \
             WHERE EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'gfs')",
        ];
        DETACH
            .iter()
            .map(|sql| self.psql_inline_instance_command(sql))
            .collect()
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

    fn query_in_instance_command(
        &self,
        sql: &str,
        database: Option<&str>,
    ) -> std::result::Result<String, ProviderError> {
        const DELIM: &str = "GFS_SQL_EOF";
        let body = gfs_domain::utils::shell::sql_heredoc_body(DELIM, sql)?;
        // Target an explicit database when given (`gfs query --database`), else the
        // container's configured POSTGRES_DB.
        let db = match database.map(str::trim).filter(|s| !s.is_empty()) {
            Some(name) => gfs_domain::utils::shell::shell_single_quote(name),
            None => r#""${POSTGRES_DB:-postgres}""#.to_string(),
        };
        Ok(format!(
            r#"PGPASSWORD="${{POSTGRES_PASSWORD:-postgres}}" psql -h 127.0.0.1 -U "${{POSTGRES_USER:-postgres}}" -d {db} -v ON_ERROR_STOP=1 -c "{body}""#
        ))
    }

    // -----------------------------------------------------------------------
    // User / role management (`gfs user`)
    // -----------------------------------------------------------------------

    fn create_role_command(&self, spec: &RoleSpec) -> std::result::Result<String, ProviderError> {
        let ident = pg_quote_ident(&spec.username)?;
        // NOSUPERUSER / NOCREATEROLE: client roles are never privileged (fixes
        // v2 escalation). Password via a quoted literal — never bare (v2 gap #3).
        let mut sql = format!(
            "CREATE ROLE {ident} WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE PASSWORD '{}';",
            sql_lit(&spec.password)
        );
        if let Some(preset) = spec.preset {
            // Quote the deploy owner (if any) so the preset's default privileges
            // are role-scoped to the customer's object-creating role.
            let owner = spec
                .default_privileges_owner
                .as_deref()
                .map(pg_quote_ident)
                .transpose()?;
            sql.push('\n');
            sql.push_str(&pg_preset_sql(&ident, preset, owner.as_deref()));
            // Record the applied preset in the role comment so `list` surfaces it.
            sql.push_str(&format!(
                "\nCOMMENT ON ROLE {ident} IS 'gfs-preset:{}';",
                preset.as_str()
            ));
        }
        // Wrap in a transaction so create+grants are atomic (fixes v2 partial-state).
        self.query_in_instance_command(&format!("BEGIN;\n{sql}\nCOMMIT;"), None)
    }

    fn alter_password_command(
        &self,
        username: &str,
        password: &str,
    ) -> std::result::Result<String, ProviderError> {
        let ident = pg_quote_ident(username)?;
        self.query_in_instance_command(
            &format!("ALTER ROLE {ident} WITH PASSWORD '{}';", sql_lit(password)),
            None,
        )
    }

    fn drop_role_command(&self, username: &str) -> std::result::Result<String, ProviderError> {
        let ident = pg_quote_ident(username)?;
        // Non-destructive drop. `REASSIGN OWNED` first hands every object the
        // role owns (tables, sequences, …) to the management role executing this
        // — so dropping a user NEVER deletes its data, unlike a bare
        // `DROP OWNED`/cascade. `DROP OWNED` then clears what's left: the role's
        // granted privileges and the `ALTER DEFAULT PRIVILEGES` entries a preset
        // created (it now owns no objects, so nothing is destroyed). `DROP ROLE`
        // then succeeds instead of failing with "objects depend on it".
        // Transactional so a partial drop can't leave a half-removed role.
        self.query_in_instance_command(
            &format!(
                "BEGIN;\nREASSIGN OWNED BY {ident} TO CURRENT_USER;\nDROP OWNED BY {ident};\nDROP ROLE {ident};\nCOMMIT;"
            ),
            None,
        )
    }

    fn list_roles_command(&self) -> std::result::Result<String, ProviderError> {
        // `-tA` (tuples-only, unaligned) → clean JSON on stdout. `left(rolname,3)`
        // filters system `pg_*` roles without LIKE-escape fragility; the private
        // `guepard-admin` management role is never listed.
        const DELIM: &str = "GFS_SQL_EOF";
        // Exclude system `pg_*` roles and the platform's management/bootstrap
        // supers (`guepard-admin`, `postgres`) — neither is a client role, and
        // surfacing the connection superuser invites a wedging `drop` (see
        // `reject_reserved_role`).
        // `preset` is read back from the role comment (`gfs-preset:<name>`) set at
        // create/apply time; NULL when the role carries no preset comment.
        let sql = "SELECT COALESCE(json_agg(json_build_object('username', rolname, 'can_login', rolcanlogin, 'is_superuser', rolsuper, 'preset', substring(shobj_description(oid, 'pg_authid') FROM '^gfs-preset:(.*)$')) ORDER BY rolname), '[]'::json) \
                   FROM pg_roles WHERE left(rolname, 3) <> 'pg_' AND rolname NOT IN ('guepard-admin', 'postgres');";
        let body = gfs_domain::utils::shell::sql_heredoc_body(DELIM, sql)?;
        Ok(format!(
            r#"PGPASSWORD="${{POSTGRES_PASSWORD:-postgres}}" psql -h 127.0.0.1 -U "${{POSTGRES_USER:-postgres}}" -d "${{POSTGRES_DB:-postgres}}" -tA -v ON_ERROR_STOP=1 -c "{body}""#
        ))
    }

    fn apply_preset_command(
        &self,
        username: &str,
        preset: RolePreset,
        default_privileges_owner: Option<&str>,
    ) -> std::result::Result<String, ProviderError> {
        let ident = pg_quote_ident(username)?;
        let owner = default_privileges_owner.map(pg_quote_ident).transpose()?;
        // Declarative, not additive: reset the role's schema-public privileges
        // first so a *lower* preset (e.g. readwrite -> readonly) actually removes
        // the higher grants + default-ACL entries, instead of leaving them (a
        // downgrade that silently keeps write access). The reset + new grants run
        // in one transaction. `create_role` does not need this (a fresh role has
        // nothing to reset).
        let reset = pg_preset_reset_sql(&ident, owner.as_deref());
        self.query_in_instance_command(
            &format!(
                "BEGIN;\n{reset}\n{}\nCOMMENT ON ROLE {ident} IS 'gfs-preset:{}';\nCOMMIT;",
                pg_preset_sql(&ident, preset, owner.as_deref()),
                preset.as_str()
            ),
            None,
        )
    }

    fn bootstrap_deploy_env_command(
        &self,
        spec: &DeployEnvSpec,
    ) -> std::result::Result<String, ProviderError> {
        let owner = pg_quote_ident(&spec.owner)?;
        let group = pg_quote_ident(&spec.group)?;
        let database = pg_quote_ident(&spec.database)?;
        // RFC 009 §5.1, hardened + transactional. The owner is LOGIN NOSUPERUSER
        // NOCREATEROLE NOCREATEDB and is NOT made the database owner — it keeps
        // `public` (explicit USAGE,CREATE + CONNECT, since roles don't inherit
        // CONNECT once PUBLIC's default is revoked) but cannot DROP DATABASE or
        // escalate. `public` itself is left at the engine default (PG15+ already
        // denies CREATE to PUBLIC — no manual REVOKE). Future owner objects flow
        // to the group via role-scoped default privileges (fixes v2 gap R5).
        let sql = format!(
            "BEGIN;\n\
             CREATE ROLE {group} NOLOGIN;\n\
             GRANT USAGE ON SCHEMA public TO {group};\n\
             CREATE ROLE {owner} WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE PASSWORD '{pw}';\n\
             GRANT CONNECT ON DATABASE {database} TO {owner};\n\
             GRANT USAGE, CREATE ON SCHEMA public TO {owner};\n\
             GRANT {group} TO {owner};\n\
             ALTER DEFAULT PRIVILEGES FOR ROLE {owner} IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO {group};\n\
             ALTER DEFAULT PRIVILEGES FOR ROLE {owner} IN SCHEMA public GRANT USAGE, SELECT ON SEQUENCES TO {group};\n\
             COMMIT;",
            pw = sql_lit(&spec.owner_password),
        );
        let bootstrap = self.query_in_instance_command(&sql, None)?;
        // Confine the management superuser (`${POSTGRES_USER}`) to the container's
        // loopback exec seam. It is the platform's management root and is never
        // handed to a client, so even a leaked credential must not reach the
        // database over the exposed endpoint. pg_hba is not settable via SQL, so
        // rewrite it in the running instance: prepend loopback allow rules + a
        // non-loopback reject for the management role ahead of the catch-all
        // (pg_hba is first-match), preserving the file's owner/perms via an
        // in-place truncate, then reload. Client roles (the owner + created users)
        // fall through to the catch-all and keep authenticating over the endpoint.
        // Idempotent (guarded by a marker) and fail-closed (`set -e`).
        let reload = self.query_in_instance_command("SELECT pg_reload_conf();", None)?;
        Ok(format!(
            "set -e\n{bootstrap}\n{RESTRICT_MGMT_ROLE_TO_LOOPBACK}\n{reload}\n"
        ))
    }

    fn grant_command(&self, spec: &GrantSpec) -> std::result::Result<String, ProviderError> {
        let stmts = pg_privilege_change_sql(
            PrivilegeChange::Grant,
            &spec.role,
            &spec.object,
            &spec.privileges,
            spec.with_grant_option,
            false,
            spec.apply_to_future.as_deref(),
        )?;
        // Transactional so a multi-statement grant (+ optional default-privileges
        // line) is atomic — no partial grant on failure (fixes v2 gap).
        self.query_in_instance_command(&format!("BEGIN;\n{stmts}\nCOMMIT;"), None)
    }

    fn revoke_command(&self, spec: &RevokeSpec) -> std::result::Result<String, ProviderError> {
        let stmts = pg_privilege_change_sql(
            PrivilegeChange::Revoke,
            &spec.role,
            &spec.object,
            &spec.privileges,
            false,
            spec.cascade,
            None,
        )?;
        self.query_in_instance_command(&format!("BEGIN;\n{stmts}\nCOMMIT;"), None)
    }

    fn list_privileges_command(&self, role: &str) -> std::result::Result<String, ProviderError> {
        // Validate the identifier even though the query filters by a quoted
        // literal — defence-in-depth (rejects anything outside the ident set).
        pg_quote_ident(role)?;
        const DELIM: &str = "GFS_SQL_EOF";
        // Live read from the engine catalog (authoritative; no CP mirror). Table
        // + sequence grants come from `information_schema.role_*_grants`; schema
        // + database grants are expanded from their ACLs via `aclexplode`. `-tA`
        // + `json_agg` → clean JSON parsed into `Vec<ObjectPrivilege>`. Never a
        // secret. `grantee` is matched against a quoted literal.
        let grantee = sql_lit(role);
        let sql = format!(
            "SELECT COALESCE(json_agg(row_to_json(p) ORDER BY p.object_type, p.object_name, p.privilege), '[]'::json) FROM (\n\
               SELECT 'table'::text AS object_type, table_schema || '.' || table_name AS object_name, lower(privilege_type) AS privilege, (is_grantable = 'YES') AS grantable \
                 FROM information_schema.role_table_grants WHERE grantee = '{grantee}'\n\
             UNION ALL\n\
               SELECT 'sequence'::text, object_schema || '.' || object_name, lower(privilege_type), (is_grantable = 'YES') \
                 FROM information_schema.role_usage_grants WHERE grantee = '{grantee}' AND object_type = 'SEQUENCE'\n\
             UNION ALL\n\
               SELECT 'schema'::text, n.nspname, lower(a.privilege_type), a.is_grantable \
                 FROM pg_namespace n, aclexplode(n.nspacl) a JOIN pg_roles r ON r.oid = a.grantee \
                 WHERE r.rolname = '{grantee}'\n\
             UNION ALL\n\
               SELECT 'database'::text, d.datname, lower(a.privilege_type), a.is_grantable \
                 FROM pg_database d, aclexplode(d.datacl) a JOIN pg_roles r ON r.oid = a.grantee \
                 WHERE r.rolname = '{grantee}' AND d.datname = current_database()\n\
             ) p;"
        );
        let body = gfs_domain::utils::shell::sql_heredoc_body(DELIM, &sql)?;
        Ok(format!(
            r#"PGPASSWORD="${{POSTGRES_PASSWORD:-postgres}}" psql -h 127.0.0.1 -U "${{POSTGRES_USER:-postgres}}" -d "${{POSTGRES_DB:-postgres}}" -tA -v ON_ERROR_STOP=1 -c "{body}""#
        ))
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
        //
        // The trailing `pg_dump … || true` is deliberate: it is the LAST command
        // in this `sh -c` script (no `set -e`), so its exit status becomes the
        // task's exit status. The metadata queries above are tolerant; `pg_dump`
        // is the fragile step (client/server version skew, large schemas, partial
        // permissions). Without `|| true`, a dump failure would propagate a
        // non-zero exit and make `ExtractSchemaUseCase` discard the metadata it
        // already captured — leaving the commit with no stored schema (the exact
        // bug this path fixes). `|| true` degrades a dump failure to an empty DDL
        // while keeping metadata-driven `schema show`/`schema diff` working.
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
)"
echo "GFS_SCHEMA_DDL"
PGPASSWORD="{password}" pg_dump -h {host} -p {port} -U {user} -d {db} --schema-only --no-owner --no-privileges || true"#,
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

/// The copy-on-read bootstrap template, kept as a real `.sql` file (proper syntax
/// highlighting / linting). `__PLACEHOLDER__` sentinels are substituted by
/// `build_clone_bootstrap_sql`.
const CLONE_BOOTSTRAP_TMPL: &str = include_str!("clone_bootstrap.sql");

/// Escape a value for use inside a single-quoted SQL string literal.
fn sql_lit(s: &str) -> String {
    s.replace('\'', "''")
}

/// Validate a database identifier and wrap it in double quotes for SQL.
/// Rejects anything outside `[A-Za-z0-9_]{1,63}` — closes the identifier
/// injection surface (v2 gap #3). Non-empty, ASCII alnum + underscore only.
fn pg_quote_ident(ident: &str) -> std::result::Result<String, ProviderError> {
    let valid = !ident.is_empty()
        && ident.len() <= 63
        && ident
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if valid {
        Ok(format!("\"{ident}\""))
    } else {
        Err(ProviderError::InvalidParams(format!(
            "invalid database identifier: {ident:?}"
        )))
    }
}

/// Revoke every privilege a preset could have granted the already-quoted role
/// `ident` on schema `public`, so [`pg_preset_sql`] can re-apply a preset
/// declaratively (the role ends at exactly the new preset, never the union of
/// old + new). Covers ALL table/sequence privileges, `CREATE` on the schema, and
/// the `ALTER DEFAULT PRIVILEGES` entries — both connecting-role-scoped and, when
/// `owner` is set, `FOR ROLE owner`. `USAGE ON SCHEMA public` is left alone (every
/// preset re-grants it). Semicolon-terminated; the caller wraps it in the same
/// transaction as the re-grant.
fn pg_preset_reset_sql(ident: &str, owner: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA public FROM {ident};\n"
    ));
    s.push_str(&format!(
        "REVOKE ALL PRIVILEGES ON ALL SEQUENCES IN SCHEMA public FROM {ident};\n"
    ));
    s.push_str(&format!("REVOKE CREATE ON SCHEMA public FROM {ident};\n"));
    s.push_str(&format!(
        "ALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE ALL ON TABLES FROM {ident};\n"
    ));
    s.push_str(&format!(
        "ALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE ALL ON SEQUENCES FROM {ident};"
    ));
    if let Some(owner) = owner {
        s.push_str(&format!(
            "\nALTER DEFAULT PRIVILEGES FOR ROLE {owner} IN SCHEMA public REVOKE ALL ON TABLES FROM {ident};\n"
        ));
        s.push_str(&format!(
            "ALTER DEFAULT PRIVILEGES FOR ROLE {owner} IN SCHEMA public REVOKE ALL ON SEQUENCES FROM {ident};"
        ));
    }
    s
}

/// The allow-listed grant bundle for `preset`, applied to the already-quoted
/// role `ident` on schema `public`. Semicolon-terminated; the caller wraps the
/// bundle in a transaction.
///
/// `owner` (already quoted) is the role whose FUTURE objects the
/// `ALTER DEFAULT PRIVILEGES` lines cover — the customer's `owner` role in a
/// deploy, so a preset user sees the tables the customer creates later. When
/// `None` the defaults are role-scoped to the connecting role (single-node
/// gfs). Without this, presets only cover the connecting admin's future tables
/// — never the customer's — so they behave as one-time snapshots (RFC 007
/// hardening; matches the RFC 009 bootstrap's `FOR ROLE owner`).
fn pg_preset_sql(ident: &str, preset: RolePreset, owner: Option<&str>) -> String {
    // `FOR ROLE "owner" ` (trailing space) when a deploy owner is supplied.
    let for_role = owner.map(|o| format!("FOR ROLE {o} ")).unwrap_or_default();
    let mut s = format!("GRANT USAGE ON SCHEMA public TO {ident};\n");
    match preset {
        RolePreset::Readonly => {
            s.push_str(&format!(
                "GRANT SELECT ON ALL TABLES IN SCHEMA public TO {ident};\n"
            ));
            s.push_str(&format!(
                "ALTER DEFAULT PRIVILEGES {for_role}IN SCHEMA public GRANT SELECT ON TABLES TO {ident};"
            ));
        }
        RolePreset::Readwrite => {
            s.push_str(&format!(
                "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO {ident};\n"
            ));
            s.push_str(&format!(
                "GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO {ident};\n"
            ));
            s.push_str(&format!(
                "ALTER DEFAULT PRIVILEGES {for_role}IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO {ident};\n"
            ));
            s.push_str(&format!(
                "ALTER DEFAULT PRIVILEGES {for_role}IN SCHEMA public GRANT USAGE, SELECT ON SEQUENCES TO {ident};"
            ));
        }
        RolePreset::Admin => {
            s.push_str(&format!("GRANT CREATE ON SCHEMA public TO {ident};\n"));
            s.push_str(&format!(
                "GRANT ALL PRIVILEGES ON ALL TABLES IN SCHEMA public TO {ident};\n"
            ));
            s.push_str(&format!(
                "GRANT ALL PRIVILEGES ON ALL SEQUENCES IN SCHEMA public TO {ident};\n"
            ));
            s.push_str(&format!(
                "ALTER DEFAULT PRIVILEGES {for_role}IN SCHEMA public GRANT ALL PRIVILEGES ON TABLES TO {ident};\n"
            ));
            s.push_str(&format!(
                "ALTER DEFAULT PRIVILEGES {for_role}IN SCHEMA public GRANT ALL PRIVILEGES ON SEQUENCES TO {ident};"
            ));
        }
    }
    s
}

/// Whether a privilege change is a GRANT or a REVOKE.
#[derive(Clone, Copy)]
enum PrivilegeChange {
    Grant,
    Revoke,
}

/// The uppercase SQL keyword for a single privilege. `All` renders as the full
/// `ALL PRIVILEGES` (though [`pg_privilege_list`] handles the ALL case first).
fn pg_privilege_keyword(p: Privilege) -> &'static str {
    match p {
        Privilege::Select => "SELECT",
        Privilege::Insert => "INSERT",
        Privilege::Update => "UPDATE",
        Privilege::Delete => "DELETE",
        Privilege::Truncate => "TRUNCATE",
        Privilege::References => "REFERENCES",
        Privilege::Trigger => "TRIGGER",
        Privilege::Usage => "USAGE",
        Privilege::Create => "CREATE",
        Privilege::Connect => "CONNECT",
        Privilege::Temporary => "TEMPORARY",
        Privilege::All => "ALL PRIVILEGES",
    }
}

/// Render a privilege set for a `GRANT`/`REVOKE`. If any element is `All`, the
/// whole set collapses to `ALL PRIVILEGES` (PostgreSQL forbids mixing `ALL`
/// with named privileges).
fn pg_privilege_list(privileges: &[Privilege]) -> String {
    if privileges.iter().any(|p| matches!(p, Privilege::All)) {
        return "ALL PRIVILEGES".to_string();
    }
    privileges
        .iter()
        .map(|p| pg_privilege_keyword(*p))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `ON …` object clause for the non-`Database` variants, with every
/// identifier validated + double-quoted via [`pg_quote_ident`]. `Database` is
/// assembled separately (its name is resolved at runtime).
fn pg_grant_object_clause(obj: &GrantableObject) -> std::result::Result<String, ProviderError> {
    Ok(match obj {
        GrantableObject::Database => {
            return Err(ProviderError::InvalidParams(
                "database object clause is assembled at runtime, not here".into(),
            ));
        }
        GrantableObject::Schema { schema } => format!("ON SCHEMA {}", pg_quote_ident(schema)?),
        GrantableObject::Table { schema, name } => {
            format!(
                "ON TABLE {}.{}",
                pg_quote_ident(schema)?,
                pg_quote_ident(name)?
            )
        }
        GrantableObject::AllTablesInSchema { schema } => {
            format!("ON ALL TABLES IN SCHEMA {}", pg_quote_ident(schema)?)
        }
        GrantableObject::Sequence { schema, name } => format!(
            "ON SEQUENCE {}.{}",
            pg_quote_ident(schema)?,
            pg_quote_ident(name)?
        ),
        GrantableObject::AllSequencesInSchema { schema } => {
            format!("ON ALL SEQUENCES IN SCHEMA {}", pg_quote_ident(schema)?)
        }
    })
}

/// Build the (non-transaction-wrapped) `GRANT`/`REVOKE` statement(s) for a
/// privilege change. Every privilege is re-checked against the object type
/// (allow-list, reject-not-warn), every identifier is `quote_ident`-quoted, and
/// the `Database` scope resolves to the instance's own database at runtime. When
/// `apply_to_future` is `Some(grantor)` (grant, all-in-schema scopes only) a
/// role-scoped `ALTER DEFAULT PRIVILEGES FOR ROLE` line is appended.
fn pg_privilege_change_sql(
    action: PrivilegeChange,
    role: &str,
    object: &GrantableObject,
    privileges: &[Privilege],
    with_grant_option: bool,
    cascade: bool,
    apply_to_future: Option<&str>,
) -> std::result::Result<String, ProviderError> {
    if privileges.is_empty() {
        return Err(ProviderError::InvalidParams(
            "at least one privilege is required".into(),
        ));
    }
    for p in privileges {
        if !p.is_valid_for(object) {
            return Err(ProviderError::InvalidParams(format!(
                "privilege {} is not valid for the target object type",
                pg_privilege_keyword(*p)
            )));
        }
    }
    let role_ident = pg_quote_ident(role)?;
    let privs = pg_privilege_list(privileges);
    let (verb, dir) = match action {
        PrivilegeChange::Grant => ("GRANT", "TO"),
        PrivilegeChange::Revoke => ("REVOKE", "FROM"),
    };
    let suffix = match action {
        PrivilegeChange::Grant if with_grant_option => " WITH GRANT OPTION",
        PrivilegeChange::Revoke if cascade => " CASCADE",
        _ => "",
    };

    let mut sql = match object {
        GrantableObject::Database => format!(
            "DO $$ BEGIN EXECUTE '{verb} {privs} ON DATABASE ' || quote_ident(current_database()) || ' {dir} {role_ident}{suffix}'; END $$;"
        ),
        other => {
            let clause = pg_grant_object_clause(other)?;
            format!("{verb} {privs} {clause} {dir} {role_ident}{suffix};")
        }
    };

    if let Some(grantor) = apply_to_future {
        if matches!(action, PrivilegeChange::Revoke) {
            return Err(ProviderError::InvalidParams(
                "apply_to_future is only valid for grant".into(),
            ));
        }
        let (schema, kind) = match object {
            GrantableObject::AllTablesInSchema { schema } => (schema, "TABLES"),
            GrantableObject::AllSequencesInSchema { schema } => (schema, "SEQUENCES"),
            _ => {
                return Err(ProviderError::InvalidParams(
                    "apply_to_future is only valid for all-tables/all-sequences-in-schema scopes"
                        .into(),
                ));
            }
        };
        let grantor_ident = pg_quote_ident(grantor)?;
        let schema_ident = pg_quote_ident(schema)?;
        sql.push('\n');
        sql.push_str(&format!(
            "ALTER DEFAULT PRIVILEGES FOR ROLE {grantor_ident} IN SCHEMA {schema_ident} GRANT {privs} ON {kind} TO {role_ident};"
        ));
    }

    Ok(sql)
}

/// Build the bootstrap SQL run inside the local GFS database to set up a lazy
/// (copy-on-read) clone of `remote`.
///
/// Installs `postgres_fdw` + `dblink`, imports the remote schema as foreign
/// tables (`gfs_remote_*`), and registers each cloned table with the `gfs`
/// copy-on-read extension. Each cloned table is a REAL local heap (carrying the
/// source's indexes); the extension's `planner_hook` inspects every query's cold
/// plan and, per scan on a registered table, serves it locally, hydrates the
/// matching key range / selective slice, whole-copies a small table, or federates
/// the query to the source. No data is copied at bootstrap time.
///
/// Correctness is the extension's responsibility (it fetches a query's real rows
/// before execution); a source table without a usable unique key is refused
/// loudly at registration rather than served empty. See
/// `crates/extensions/gfs/README.md` for the mechanism — the original overlay-view
/// design in `docs/rfcs/008-remote-clone.md` was superseded by this planner hook.
fn build_clone_bootstrap_sql(remote: &RemoteSource) -> String {
    // dblink connection string, used only for read-only introspection of the
    // remote (schema/table/key discovery).
    let mut conn = format!(
        "host={} port={} dbname={} user={} password={}",
        remote.host, remote.port, remote.dbname, remote.user, remote.password,
    );
    if let Some(sslmode) = &remote.sslmode {
        conn.push_str(&format!(" sslmode={sslmode}"));
    }

    let sslmode_opt = remote
        .sslmode
        .as_ref()
        .map(|m| format!(", sslmode '{}'", sql_lit(m)))
        .unwrap_or_default();

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
        .replace("__RSSLMODE_OPT__", &sslmode_opt)
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
        // First tuning value binds the listener so the container is reachable via
        // the k8s Service/NodePort (a fresh initdb otherwise defaults to localhost).
        assert_eq!(def.args.get(1), Some(&"listen_addresses=*".to_string()));
        assert!(def.args.contains(&"shared_buffers=32MB".to_string()));
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
    fn query_in_instance_command_uses_loopback_and_heredoc() {
        let provider = PostgresqlProvider::new();
        let cmd = provider
            .query_in_instance_command("SELECT 1;", None)
            .expect("query command");
        assert!(cmd.contains("127.0.0.1"));
        assert!(cmd.contains("POSTGRES_USER:-postgres"));
        assert!(cmd.contains(r#"-d "${POSTGRES_DB:-postgres}""#));
        assert!(cmd.contains("GFS_SQL_EOF"));
        assert!(cmd.contains("SELECT 1;"));

        // An explicit database override targets that DB literally.
        let cmd_db = provider
            .query_in_instance_command("SELECT 1;", Some("myapp"))
            .expect("query command");
        assert!(cmd_db.contains("-d 'myapp'"));
        assert!(!cmd_db.contains("${POSTGRES_DB"));
        assert!(!cmd.starts_with("psql postgresql://"));
    }

    #[test]
    fn create_role_command_is_hardened() {
        use gfs_domain::model::db_user::{RolePreset, RoleSpec};
        let provider = PostgresqlProvider::new();
        let spec = RoleSpec {
            username: "app_rw".into(),
            password: "p'wd".into(),
            preset: Some(RolePreset::Readwrite),
            default_privileges_owner: None,
        };
        let cmd = provider.create_role_command(&spec).expect("cmd");
        assert!(
            cmd.contains(r#"CREATE ROLE "app_rw" WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE"#)
        );
        assert!(
            cmd.contains("PASSWORD 'p''wd'"),
            "password literal must double the quote"
        );
        assert!(
            cmd.contains("BEGIN;"),
            "create+grants must be transactional"
        );
        assert!(
            cmd.contains("GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public")
        );
    }

    #[test]
    fn preset_default_privileges_are_role_scoped_to_owner() {
        use gfs_domain::model::db_user::{RolePreset, RoleSpec};
        let provider = PostgresqlProvider::new();
        // With a deploy owner, the ALTER DEFAULT PRIVILEGES lines must be
        // FOR ROLE "owner" so the preset covers the owner's FUTURE objects.
        let spec = RoleSpec {
            username: "reader".into(),
            password: "pw".into(),
            preset: Some(RolePreset::Readonly),
            default_privileges_owner: Some("owner".into()),
        };
        let cmd = provider.create_role_command(&spec).expect("cmd");
        assert!(
            cmd.contains(
                r#"ALTER DEFAULT PRIVILEGES FOR ROLE "owner" IN SCHEMA public GRANT SELECT ON TABLES TO "reader""#
            ),
            "preset defaults must be role-scoped to the deploy owner; got:\n{cmd}"
        );

        // Without an owner (single-node), no FOR ROLE clause is emitted.
        let spec_none = RoleSpec {
            default_privileges_owner: None,
            ..spec
        };
        let cmd_none = provider.create_role_command(&spec_none).expect("cmd");
        assert!(
            !cmd_none.contains("FOR ROLE"),
            "single-node preset must not name a deploy owner; got:\n{cmd_none}"
        );
        assert!(
            cmd_none.contains(
                r#"ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO "reader""#
            ),
            "single-node preset keeps role-scoped-to-self defaults; got:\n{cmd_none}"
        );
    }

    #[test]
    fn apply_preset_is_declarative_resets_before_granting() {
        use gfs_domain::model::db_user::RolePreset;
        let provider = PostgresqlProvider::new();
        // apply_preset must REVOKE the role's existing schema-public privileges
        // (incl. FOR ROLE owner default-ACLs) BEFORE granting the new preset, so a
        // downgrade actually reduces privileges.
        let cmd = provider
            .apply_preset_command("app_rw", RolePreset::Readonly, Some("owner"))
            .expect("cmd");
        assert!(
            cmd.contains(r#"REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA public FROM "app_rw""#),
            "must revoke existing table privileges first; got:\n{cmd}"
        );
        assert!(
            cmd.contains(
                r#"ALTER DEFAULT PRIVILEGES FOR ROLE "owner" IN SCHEMA public REVOKE ALL ON TABLES FROM "app_rw""#
            ),
            "must revoke owner-scoped default privileges first; got:\n{cmd}"
        );
        // The reset precedes the new preset's grant (declarative order).
        let revoke_at = cmd.find("REVOKE ALL PRIVILEGES ON ALL TABLES").unwrap();
        let grant_at = cmd.find("GRANT SELECT ON ALL TABLES").unwrap();
        assert!(revoke_at < grant_at, "reset must run before the re-grant");
    }

    #[test]
    fn create_role_rejects_bad_username() {
        use gfs_domain::model::db_user::RoleSpec;
        let provider = PostgresqlProvider::new();
        for bad in ["bad name", "drop;--", "", "a\"b", "x'; DROP ROLE y; --"] {
            let spec = RoleSpec {
                username: bad.into(),
                password: "x".into(),
                preset: None,
                default_privileges_owner: None,
            };
            assert!(
                provider.create_role_command(&spec).is_err(),
                "must reject username {bad:?}"
            );
        }
    }

    #[test]
    fn drop_and_alter_password_quote_identifier_and_literal() {
        let provider = PostgresqlProvider::new();
        let drop = provider.drop_role_command("app_ro").unwrap();
        assert!(
            drop.contains(r#"REASSIGN OWNED BY "app_ro" TO CURRENT_USER;"#),
            "must reassign owned objects before dropping so no data is destroyed"
        );
        assert!(
            drop.contains(r#"DROP OWNED BY "app_ro";"#),
            "must clean up grants after reassigning objects"
        );
        assert!(drop.contains(r#"DROP ROLE "app_ro";"#));
        // REASSIGN must precede DROP OWNED, else owned tables would be destroyed.
        assert!(
            drop.find("REASSIGN OWNED").unwrap() < drop.find("DROP OWNED").unwrap(),
            "REASSIGN must run before DROP OWNED"
        );
        assert!(provider.drop_role_command("bad name").is_err());
        let alter = provider.alter_password_command("app_ro", "new'pw").unwrap();
        assert!(alter.contains(r#"ALTER ROLE "app_ro" WITH PASSWORD 'new''pw';"#));
    }

    #[test]
    fn bootstrap_deploy_env_command_emits_hardened_sql() {
        let provider = PostgresqlProvider::new();
        let spec = DeployEnvSpec {
            owner: "app_owner".into(),
            owner_password: "s3cret'pw".into(),
            group: "developers".into(),
            database: "appdb".into(),
        };
        let cmd = provider.bootstrap_deploy_env_command(&spec).unwrap();
        // group NOLOGIN + least-privileged owner; identifiers quoted, password escaped.
        assert!(cmd.contains(r#"CREATE ROLE "developers" NOLOGIN;"#));
        assert!(cmd.contains(
            r#"CREATE ROLE "app_owner" WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE PASSWORD 's3cret''pw';"#
        ));
        // owner keeps `public` (explicit CONNECT + USAGE,CREATE).
        assert!(cmd.contains(r#"GRANT CONNECT ON DATABASE "appdb" TO "app_owner";"#));
        assert!(cmd.contains(r#"GRANT USAGE, CREATE ON SCHEMA public TO "app_owner";"#));
        // membership + role-scoped default privileges (future owner objects → group).
        assert!(cmd.contains(r#"GRANT "developers" TO "app_owner";"#));
        assert!(cmd.contains(
            r#"ALTER DEFAULT PRIVILEGES FOR ROLE "app_owner" IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO "developers";"#
        ));
        // transactional; owner is NOT the DB owner; no manual `public` REVOKE.
        assert!(cmd.contains("BEGIN;") && cmd.contains("COMMIT;"));
        assert!(
            !cmd.contains("ALTER DATABASE"),
            "owner must not become DB owner"
        );
        assert!(
            !cmd.to_uppercase().contains("REVOKE"),
            "no manual public lock"
        );
        // management super confined to loopback: pg_hba rewrite (loopback allow +
        // non-loopback reject for ${POSTGRES_USER}) ahead of the catch-all, then
        // reload — fail-closed under `set -e`.
        assert!(cmd.starts_with("set -e"), "bootstrap must be fail-closed");
        assert!(cmd.contains("pg_hba.conf"), "must rewrite pg_hba");
        assert!(
            cmd.contains(r#"host all "%s" all reject"#)
                && cmd.contains(r#""${POSTGRES_USER:-postgres}""#),
            "management role must be rejected off-loopback"
        );
        assert!(
            cmd.contains(r#"host all "%s" 127.0.0.1/32 trust"#),
            "management role must stay allowed on loopback"
        );
        assert!(
            cmd.contains("pg_reload_conf()"),
            "pg_hba change must be reloaded"
        );
    }

    #[test]
    fn bootstrap_deploy_env_command_rejects_bad_identifiers() {
        let provider = PostgresqlProvider::new();
        let bad = DeployEnvSpec {
            owner: "bad name".into(),
            owner_password: "x".into(),
            group: "developers".into(),
            database: "appdb".into(),
        };
        assert!(provider.bootstrap_deploy_env_command(&bad).is_err());
    }

    #[test]
    fn list_roles_command_uses_ta_json_and_excludes_admin() {
        let provider = PostgresqlProvider::new();
        let cmd = provider.list_roles_command().expect("cmd");
        assert!(
            cmd.contains("-tA"),
            "list must use tuples-only unaligned output"
        );
        assert!(cmd.contains("json_agg"));
        assert!(cmd.contains("rolname NOT IN ('guepard-admin', 'postgres')"));
        assert!(cmd.contains("left(rolname, 3) <> 'pg_'"));
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
        assert_eq!(spec.definition.image, "gfs-postgres:17");
        assert!(spec.command.contains("GFS_SCHEMA_VERSION"));
        assert!(spec.command.contains("GFS_SCHEMA_SCHEMAS"));
        assert!(spec.command.contains("GFS_SCHEMA_TABLES"));
        assert!(spec.command.contains("GFS_SCHEMA_COLUMNS"));
        assert!(spec.command.contains("psql"));
        assert!(spec.command.contains("-h 172.17.0.2"));
        assert!(spec.command.contains("-U myuser"));
        assert!(spec.command.contains("-d mydb"));
        // The DDL must be dumped to STDOUT (no `-f <file>`) so it survives
        // runtimes where the sidecar runs on a different host than gfs (k8s).
        assert!(spec.command.contains("GFS_SCHEMA_DDL"));
        assert!(spec.command.contains("pg_dump"));
        assert!(spec.command.contains("--schema-only"));
        assert!(
            !spec
                .command
                .contains("--schema-only --no-owner --no-privileges -f"),
            "schema DDL dump must not write to a file"
        );
        // The dump must be non-fatal: a pg_dump failure must not nuke the
        // already-captured metadata (which keeps `schema diff` working).
        assert!(
            spec.command.contains("--no-privileges || true"),
            "schema DDL dump must be non-fatal (|| true)"
        );
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
            sslmode: None,
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
        // FDW server + mapping carry the remote connection params (plus the
        // pushdown/batching knobs: use_remote_estimate + fetch_size).
        assert!(sql.contains(
            "OPTIONS (host 'rds.example.com', port '5432', dbname 'shop',\n           \
             use_remote_estimate 'true', fetch_size '10000')"
        ));
        assert!(sql.contains("OPTIONS (user 'reader', password 'p@ss')"));
        // Requested schemas become an array literal driving the per-schema import.
        assert!(sql.contains("ARRAY['public']::text[]"));
        // Per-table import (LIMIT TO) so one bad table cannot abort the clone.
        assert!(
            sql.contains(
                "IMPORT FOREIGN SCHEMA %I LIMIT TO (%I) FROM SERVER gfs_remote_srv INTO %I"
            )
        );
        // Resilience hooks: skip un-importable tables.
        assert!(sql.contains("CREATE EXTENSION IF NOT EXISTS %I"));
        assert!(sql.contains("to_regclass(fq_remote) IS NULL"));
        // dblink introspection connection string is present.
        assert!(
            sql.contains("host=rds.example.com port=5432 dbname=shop user=reader password=p@ss")
        );
        // No leftover placeholders — every `__…__` substitution sentinel is gone.
        for ph in [
            "__RHOST__",
            "__RPORT__",
            "__RDB__",
            "__RUSER__",
            "__RPASS__",
            "__RSSLMODE_OPT__",
            "__CONN__",
            "__SCHEMAS_ARRAY__",
        ] {
            assert!(!sql.contains(ph), "leftover placeholder: {ph}");
        }
    }

    #[test]
    fn clone_bootstrap_sql_propagates_sslmode() {
        let mut remote = sample_remote();
        remote.sslmode = Some("require".into());
        let sql = build_clone_bootstrap_sql(&remote);
        assert!(sql.contains(", sslmode 'require'"));
        assert!(sql.contains("password=p@ss sslmode=require"));
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
    fn clone_bootstrap_sql_requires_gfs_no_overlay_fallback() {
        let sql = build_clone_bootstrap_sql(&sample_remote());

        // The copy-on-read extension (gfs) is REQUIRED — there is no overlay
        // fallback. It is created unconditionally (so it aborts under \set
        // ON_ERROR_STOP if the image lacks it), NOT in a best-effort wrapper.
        assert!(sql.contains("CREATE EXTENSION IF NOT EXISTS gfs;"));
        assert!(!sql.contains("$gfstam$")); // not the old best-effort wrapper
        assert!(!sql.contains("using the overlay")); // not the old fallback notice
        // The clone logic is a planner hook in the extension's shared library; it
        // must be preloaded on every connection to this database.
        assert!(sql.contains("SET session_preload_libraries"));
        // clone() builds copy-on-read tables only: the faithful table stays a plain
        // heap table (NO custom access method) and we register its source.
        assert!(!sql.contains("SET ACCESS METHOD gfs"));
        assert!(sql.contains("gfs.register_clone(store_fq::regclass, fq_remote, p_keycols[1])"));
        assert!(sql.contains("PERFORM gfs_sync.build_clone(rec.nsp, rec.tab, rec.keycols)"));
        // Foreign keys are dropped so lazy per-table copy-on-read never trips RI.
        assert!(sql.contains("contype = 'f'"));
        assert!(sql.contains("DROP CONSTRAINT"));
        // No fallback: clone() never calls build_overlay and installs no shim.
        assert!(!sql.contains("PERFORM gfs_sync.build_overlay"));
        assert!(!sql.contains("ALTER DATABASE %I SET search_path"));
        // A probe other components can read to know this is a gfs clone.
        assert!(sql.contains("CREATE OR REPLACE FUNCTION gfs_sync.clone_tam()"));
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
    fn render_param_overrides_emits_dash_c_pairs() {
        let provider = PostgresqlProvider::new();
        let mut params = std::collections::BTreeMap::new();
        params.insert("max_connections".to_string(), "200".to_string());
        params.insert("shared_buffers".to_string(), "256MB".to_string());
        let args = provider.render_param_overrides(&params);
        assert_eq!(args.len(), 2);
        // BTreeMap iterates in sorted key order: max_connections before shared_buffers.
        assert_eq!(args[0].name, "-c");
        assert_eq!(args[0].value, "max_connections=200");
        assert_eq!(args[1].value, "shared_buffers=256MB");
    }

    #[test]
    fn definition_with_overrides_appends_after_defaults_so_override_wins() {
        let provider = PostgresqlProvider::new();
        let base = provider.definition();
        let mut params = std::collections::BTreeMap::new();
        params.insert("max_connections".to_string(), "200".to_string());
        let def = provider.definition_with_overrides(&params);
        // Override is appended after the default args (last `-c` wins in PostgreSQL).
        assert_eq!(def.args.len(), base.args.len() + 2);
        assert_eq!(def.args.last(), Some(&"max_connections=200".to_string()));
        let last_default = base
            .args
            .iter()
            .rposition(|a| a == "max_connections=10")
            .expect("default max_connections present");
        let override_pos = def
            .args
            .iter()
            .rposition(|a| a == "max_connections=200")
            .expect("override present");
        assert!(
            override_pos > last_default,
            "override must come after default"
        );
    }

    #[test]
    fn definition_with_overrides_empty_params_is_noop() {
        let provider = PostgresqlProvider::new();
        let empty = std::collections::BTreeMap::new();
        assert_eq!(
            provider.definition_with_overrides(&empty).args,
            provider.definition().args
        );
    }

    #[test]
    fn clone_bootstrap_spec_wraps_sql_in_local_psql_heredoc() {
        let provider = PostgresqlProvider::new();
        let spec = provider
            .clone_bootstrap_spec(&local_params(), &sample_remote())
            .unwrap();
        // Connects to the LOCAL database.
        assert!(
            spec.command
                .contains("psql -h 172.17.0.2 -p 5432 -U postgres -d gfs")
        );
        assert!(spec.command.contains("<<'GFS_CLONE_BOOTSTRAP'"));
        // Local password is supplied via PGPASSWORD on the sidecar.
        assert!(
            spec.definition
                .env
                .iter()
                .any(|e| e.name == "PGPASSWORD" && e.default.as_deref() == Some("localpw"))
        );
        assert_eq!(spec.definition.image, provider.definition().image);
        // The v17-only `transaction_timeout` GUC is stripped from the dump before
        // replay so a pre-v17 local server doesn't choke on it.
        assert!(
            spec.command
                .contains("sed -i '/^SET transaction_timeout/d' /tmp/gfs_faithful.sql")
        );
    }

    // -----------------------------------------------------------------------
    // Object-level grant / revoke / list-privileges (phase 2) — exact SQL
    // -----------------------------------------------------------------------

    #[test]
    fn grant_command_quotes_and_wraps_in_txn() {
        use gfs_domain::model::db_user::{GrantSpec, GrantableObject, Privilege};
        let provider = PostgresqlProvider::new();
        let spec = GrantSpec {
            role: "app_ro".into(),
            object: GrantableObject::Table {
                schema: "public".into(),
                name: "orders".into(),
            },
            privileges: vec![Privilege::Select, Privilege::Insert],
            with_grant_option: false,
            apply_to_future: None,
        };
        let cmd = provider.grant_command(&spec).expect("cmd");
        assert!(
            cmd.contains(r#"GRANT SELECT, INSERT ON TABLE "public"."orders" TO "app_ro";"#),
            "got: {cmd}"
        );
        assert!(
            cmd.contains("BEGIN;") && cmd.contains("COMMIT;"),
            "must be transactional"
        );
    }

    #[test]
    fn grant_command_collapses_all_and_appends_grant_option() {
        use gfs_domain::model::db_user::{GrantSpec, GrantableObject, Privilege};
        let provider = PostgresqlProvider::new();
        let spec = GrantSpec {
            role: "app_admin".into(),
            object: GrantableObject::Schema {
                schema: "public".into(),
            },
            privileges: vec![Privilege::All],
            with_grant_option: true,
            apply_to_future: None,
        };
        let cmd = provider.grant_command(&spec).unwrap();
        assert!(
            cmd.contains(
                r#"GRANT ALL PRIVILEGES ON SCHEMA "public" TO "app_admin" WITH GRANT OPTION;"#
            ),
            "got: {cmd}"
        );
    }

    #[test]
    fn grant_command_apply_to_future_emits_role_scoped_default_privileges() {
        use gfs_domain::model::db_user::{GrantSpec, GrantableObject, Privilege};
        let provider = PostgresqlProvider::new();
        let spec = GrantSpec {
            role: "developers".into(),
            object: GrantableObject::AllTablesInSchema {
                schema: "public".into(),
            },
            privileges: vec![Privilege::Select],
            with_grant_option: false,
            apply_to_future: Some("owner".into()),
        };
        let cmd = provider.grant_command(&spec).unwrap();
        assert!(cmd.contains(r#"GRANT SELECT ON ALL TABLES IN SCHEMA "public" TO "developers";"#));
        assert!(
            cmd.contains(r#"ALTER DEFAULT PRIVILEGES FOR ROLE "owner" IN SCHEMA "public" GRANT SELECT ON TABLES TO "developers";"#),
            "got: {cmd}"
        );
    }

    #[test]
    fn grant_command_database_scope_resolves_name_at_runtime() {
        use gfs_domain::model::db_user::{GrantSpec, GrantableObject, Privilege};
        let provider = PostgresqlProvider::new();
        let spec = GrantSpec {
            role: "app_ro".into(),
            object: GrantableObject::Database,
            privileges: vec![Privilege::Connect],
            with_grant_option: false,
            apply_to_future: None,
        };
        let cmd = provider.grant_command(&spec).unwrap();
        assert!(
            cmd.contains("quote_ident(current_database())"),
            "database name must resolve at runtime (no caller-named DB); got: {cmd}"
        );
        assert!(cmd.contains("GRANT CONNECT ON DATABASE"));
    }

    #[test]
    fn grant_command_rejects_out_of_matrix_privilege() {
        use gfs_domain::model::db_user::{GrantSpec, GrantableObject, Privilege};
        let provider = PostgresqlProvider::new();
        // INSERT is not a valid privilege on a sequence.
        let spec = GrantSpec {
            role: "app_ro".into(),
            object: GrantableObject::Sequence {
                schema: "public".into(),
                name: "q".into(),
            },
            privileges: vec![Privilege::Insert],
            with_grant_option: false,
            apply_to_future: None,
        };
        assert!(
            provider.grant_command(&spec).is_err(),
            "INSERT on a sequence must be rejected before SQL"
        );
    }

    #[test]
    fn grant_command_rejects_apply_to_future_on_single_object() {
        use gfs_domain::model::db_user::{GrantSpec, GrantableObject, Privilege};
        let provider = PostgresqlProvider::new();
        let spec = GrantSpec {
            role: "app_ro".into(),
            object: GrantableObject::Table {
                schema: "public".into(),
                name: "t".into(),
            },
            privileges: vec![Privilege::Select],
            with_grant_option: false,
            apply_to_future: Some("owner".into()),
        };
        assert!(
            provider.grant_command(&spec).is_err(),
            "apply_to_future is only valid for all-in-schema scopes"
        );
    }

    #[test]
    fn revoke_command_defaults_restrict_and_supports_cascade() {
        use gfs_domain::model::db_user::{GrantableObject, Privilege, RevokeSpec};
        let provider = PostgresqlProvider::new();
        let spec = |cascade| RevokeSpec {
            role: "app_rw".into(),
            object: GrantableObject::Table {
                schema: "public".into(),
                name: "orders".into(),
            },
            privileges: vec![Privilege::Insert],
            cascade,
        };
        let restrict = provider.revoke_command(&spec(false)).unwrap();
        assert!(
            restrict.contains(r#"REVOKE INSERT ON TABLE "public"."orders" FROM "app_rw";"#),
            "got: {restrict}"
        );
        assert!(!restrict.contains("CASCADE"), "default is RESTRICT");
        let cascade = provider.revoke_command(&spec(true)).unwrap();
        assert!(
            cascade.contains(r#"FROM "app_rw" CASCADE;"#),
            "got: {cascade}"
        );
    }

    #[test]
    fn list_privileges_command_uses_ta_json_filtered_to_role() {
        let provider = PostgresqlProvider::new();
        let cmd = provider.list_privileges_command("app_ro").unwrap();
        assert!(cmd.contains("-tA"), "tuples-only JSON read");
        assert!(cmd.contains("json_agg"));
        assert!(
            cmd.contains("grantee = 'app_ro'"),
            "must filter by the role literal"
        );
        assert!(cmd.contains("role_table_grants") && cmd.contains("aclexplode"));
    }

    #[test]
    fn list_privileges_command_rejects_bad_role() {
        let provider = PostgresqlProvider::new();
        assert!(
            provider
                .list_privileges_command("bad; DROP ROLE x; --")
                .is_err()
        );
    }

    // -----------------------------------------------------------------------
    // EMPIRICAL: emitted SQL against a live PostgreSQL (Docker-gated).
    // Proves grant/revoke actually change `has_table_privilege` — not just that
    // our SQL string equals an expected string. Skips (does not fail) w/o Docker.
    // -----------------------------------------------------------------------

    fn docker_available() -> bool {
        std::process::Command::new("docker")
            .arg("info")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn grant_revoke_flip_has_table_privilege_on_live_pg() {
        use gfs_domain::model::db_user::{GrantSpec, GrantableObject, Privilege, RevokeSpec};
        if !docker_available() {
            eprintln!("SKIP grant_revoke_flip_has_table_privilege_on_live_pg: docker unavailable");
            return;
        }
        let provider = PostgresqlProvider::new();
        let cn = format!("gfs-grant-live-{}", std::process::id());
        let docker = |args: &[&str]| {
            std::process::Command::new("docker")
                .args(args)
                .output()
                .expect("docker")
        };
        let exec_sql = |cn: &str, sql: &str| {
            std::process::Command::new("docker")
                .args(["exec", cn, "psql", "-U", "postgres", "-tAc", sql])
                .output()
                .expect("docker exec psql")
        };
        // Run an emitted (full shell) command inside the container via `sh -c`.
        let exec_shell = |cn: &str, cmd: &str| {
            std::process::Command::new("docker")
                .args(["exec", cn, "sh", "-c", cmd])
                .output()
                .expect("docker exec sh")
        };

        let _ = docker(&["rm", "-f", &cn]);
        let run = docker(&[
            "run",
            "-d",
            "--rm",
            "--name",
            &cn,
            "-e",
            "POSTGRES_PASSWORD=x",
            "postgres:17",
        ]);
        assert!(
            run.status.success(),
            "docker run: {}",
            String::from_utf8_lossy(&run.stderr)
        );

        let mut ready = false;
        for _ in 0..30 {
            if docker(&["exec", &cn, "pg_isready", "-U", "postgres"])
                .status
                .success()
            {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        exec_sql(&cn, "CREATE ROLE app_ro; CREATE TABLE public.t(id int);");

        let grant = provider
            .grant_command(&GrantSpec {
                role: "app_ro".into(),
                object: GrantableObject::Table {
                    schema: "public".into(),
                    name: "t".into(),
                },
                privileges: vec![Privilege::Select],
                with_grant_option: false,
                apply_to_future: None,
            })
            .unwrap();
        let revoke = provider
            .revoke_command(&RevokeSpec {
                role: "app_ro".into(),
                object: GrantableObject::Table {
                    schema: "public".into(),
                    name: "t".into(),
                },
                privileges: vec![Privilege::Select],
                cascade: false,
            })
            .unwrap();
        let priv_q = "SELECT has_table_privilege('app_ro','public.t','SELECT')";

        let g = exec_shell(&cn, &grant);
        let after_grant = String::from_utf8_lossy(&exec_sql(&cn, priv_q).stdout)
            .trim()
            .to_string();
        let r = exec_shell(&cn, &revoke);
        let after_revoke = String::from_utf8_lossy(&exec_sql(&cn, priv_q).stdout)
            .trim()
            .to_string();

        // Clean up BEFORE asserting so a failed assert never leaks the container.
        let _ = docker(&["rm", "-f", &cn]);

        assert!(ready, "postgres never became ready");
        assert!(
            g.status.success(),
            "emitted GRANT failed: {}",
            String::from_utf8_lossy(&g.stderr)
        );
        assert_eq!(
            after_grant, "t",
            "SELECT must be granted after the emitted GRANT"
        );
        assert!(
            r.status.success(),
            "emitted REVOKE failed: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        assert_eq!(
            after_revoke, "f",
            "SELECT must be gone after the emitted REVOKE"
        );
    }
}
