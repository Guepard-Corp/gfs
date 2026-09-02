//! SQLite provider: a file-backed database that needs no compute instance.
//!
//! Unlike postgres, mysql and clickhouse, SQLite is not a server. There is no
//! process to provision, no port to bind, and between commands nothing holds
//! the database open. [`SqliteProvider::requires_compute`] therefore returns
//! `false`, and every operation GFS performs runs in this process against the
//! file, through the SQLite amalgamation linked in by `rusqlite`'s `bundled`
//! feature.
//!
//! Linking the engine rather than shelling out to a `sqlite3` binary is a
//! deliberate choice, for two reasons:
//!
//! * **Reproducibility.** Schema extraction records `sqlite_version()` into the
//!   commit's metadata. A host binary's version varies per developer, so the
//!   same schema would hash to different metadata on different machines. The
//!   bundled engine is pinned by `Cargo.lock`.
//! * **No host dependency.** `gfs init`, `commit`, `schema` and `checkout` work
//!   on a machine with no SQLite installed at all.
//!
//! One property is worth stating plainly: because the writer of a SQLite
//! database is whatever process opened the file — typically the user's own
//! application — no container could freeze it even if one existed. Snapshot
//! consistency comes from checkpointing the WAL (see
//! [`LocalEngine::prepare_for_snapshot`]) and from the storage layer
//! taking an atomic copy-on-write clone, never from pausing a compute instance.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gfs_domain::ports::compute::{ComputeDefinition, EnvVar, PortMapping};
use gfs_domain::ports::database_provider::{
    ConnectionParams, DataFormat, DatabaseProvider, DatabaseProviderArg, DatabaseProviderRegistry,
    ExportSpec, ImportSpec, LOCAL_DATA_DIR_ENV, LocalEngine, ProviderError, Result,
    SupportedFeature,
};

const NAME: &str = "sqlite";

/// Placeholder image. Never pulled: `requires_compute()` is `false`, so nothing
/// provisions a container from this definition. The tag still matters because
/// `init` rewrites it to `<base>:<database_version>` and `version_from_image`
/// reads the version back out of it.
const DEFAULT_IMAGE: &str = "sqlite:3";

/// Filename of the database inside the workspace data directory.
pub const DB_FILENAME: &str = "db.sqlite";

/// Optional override carrying an absolute path to the database file, for
/// pointing at a database outside the workspace. When absent the file is
/// resolved as [`LOCAL_DATA_DIR_ENV`] joined with [`DB_FILENAME`].
pub const ENV_DB_PATH: &str = "SQLITE_DB_PATH";

/// Directory the export/import artifact contract writes to and reads from.
const ARTIFACT_DIR: &str = "/data";

/// Single namespace SQLite exposes. Mirrors postgres's `public`.
const MAIN_SCHEMA: &str = "main";

/// Subquery assigning each user table a stable id.
///
/// Used verbatim by both the tables and the columns query so a column's
/// `table_id` refers to the same table the tables section reported. `ORDER BY
/// name` makes the numbering deterministic across runs, which keeps the schema
/// hash stable when nothing has actually changed.
const TABLE_IDS: &str = "SELECT ROW_NUMBER() OVER (ORDER BY name) AS id, name \
     FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite~_%' ESCAPE '~'";

/// Schema-only DDL, in place of the `sqlite3` shell's `.schema` dot-command
/// (which is a feature of that client, not of the engine).
const DDL_QUERY: &str = "SELECT coalesce(group_concat(sql, ';\n'), '') || ';' \
     FROM sqlite_master \
     WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite~_%' ESCAPE '~';";

/// Whether a column can actually hold NULL.
///
/// `pragma_table_info.notnull` alone is not enough, because SQLite reports `0`
/// for a primary key it nonetheless refuses to store NULL in. The distinguishing
/// fact is whether the primary key is backed by an index:
///
/// * `INTEGER PRIMARY KEY` (any case, with or without `AUTOINCREMENT`) is an
///   alias for the rowid. SQLite creates no index for it, and an inserted NULL
///   is replaced by a generated rowid — so a NULL can never be read back.
/// * Every other primary key form gets an `origin='pk'` index, and SQLite's
///   long-standing behaviour is to permit NULLs in it. That includes
///   `INTEGER PRIMARY KEY DESC` and `INT PRIMARY KEY` — neither is a rowid
///   alias, and both genuinely store NULLs.
/// * `WITHOUT ROWID` tables need no special case: SQLite already reports
///   `notnull = 1` for their key columns and enforces it.
///
/// Verified by inserting NULL into each form and reading it back; the index test
/// predicts the outcome in every case, without parsing declared type names.
const IS_NULLABLE: &str = "CASE \
     WHEN p.\"notnull\" = 1 THEN 'false' \
     WHEN p.pk > 0 AND NOT EXISTS (\
         SELECT 1 FROM pragma_index_list(t.name) il WHERE il.origin = 'pk') THEN 'false' \
     ELSE 'true' END";

/// Whether a column is unique on its own.
///
/// A column of a composite primary key is not: `PRIMARY KEY(a, b)` constrains
/// the pair, and `a` may repeat. So a primary key column qualifies only when it
/// is the whole key. A single-column `UNIQUE` index qualifies too.
const IS_UNIQUE: &str = "CASE \
     WHEN p.pk = 1 AND (SELECT max(pk) FROM pragma_table_info(t.name)) = 1 THEN 'true' \
     WHEN EXISTS (SELECT 1 FROM pragma_index_list(t.name) il \
                  WHERE il.\"unique\" = 1 \
                    AND (SELECT count(*) FROM pragma_index_info(il.name)) = 1 \
                    AND (SELECT ii.name FROM pragma_index_info(il.name) ii) = p.name) THEN 'true' \
     ELSE 'false' END";

/// SQLite database provider. Implements [`DatabaseProvider`] without requiring
/// a compute instance.
#[derive(Debug, Default)]
pub struct SqliteProvider;

impl SqliteProvider {
    pub fn new() -> Self {
        Self
    }

    fn definition_impl() -> ComputeDefinition {
        ComputeDefinition {
            labels: Default::default(),
            image: DEFAULT_IMAGE.to_string(),
            env: vec![EnvVar {
                name: ENV_DB_PATH.to_string(),
                default: None,
            }],
            // `ports` is documented as mandatory and callers iterate over it, so
            // the list must be non-empty. Port 0 signals "no listener"; nothing
            // binds it because SQLite has no server to reach.
            ports: vec![PortMapping {
                compute_port: 0,
                host_port: None,
            }],
            data_dir: PathBuf::from(ARTIFACT_DIR),
            host_data_dir: None,
            user: None,
            logs_dir: None,
            conf_dir: None,
            args: vec![],
        }
    }

    /// Absolute path of the database file.
    ///
    /// Normally the workspace data directory the caller supplied, joined with
    /// this provider's filename — the caller deliberately does not know the
    /// filename. [`ENV_DB_PATH`] overrides it outright.
    fn db_path(params: &ConnectionParams) -> std::result::Result<PathBuf, ProviderError> {
        if let Some(explicit) = params.get_env(ENV_DB_PATH) {
            return Ok(PathBuf::from(explicit));
        }
        let dir = params
            .get_env(LOCAL_DATA_DIR_ENV)
            .ok_or_else(|| ProviderError::MissingEnvVar(LOCAL_DATA_DIR_ENV.to_string()))?;
        Ok(Path::new(dir).join(DB_FILENAME))
    }

    /// Single-quote `path` for POSIX `sh`.
    ///
    /// The export and import specs embed the database path in a shell string, so
    /// a path containing a quote, a space or a metacharacter must not be able to
    /// terminate the argument. The standard POSIX idiom closes the quote, emits
    /// an escaped quote, and reopens: `it's` becomes `'it'\''s'`.
    fn shell_quote(path: &str) -> String {
        format!("'{}'", path.replace('\'', r"'\''"))
    }

    fn open(path: &Path) -> std::result::Result<rusqlite::Connection, ProviderError> {
        rusqlite::Connection::open(path)
            .map_err(|e| ProviderError::InvalidParams(format!("cannot open '{path:?}': {e}")))
    }

    /// Run a query returning a single text value.
    fn scalar(
        conn: &rusqlite::Connection,
        sql: &str,
    ) -> std::result::Result<String, ProviderError> {
        conn.query_row(sql, [], |row| row.get::<_, Option<String>>(0))
            .map(|v| v.unwrap_or_default())
            .map_err(|e| ProviderError::InvalidParams(format!("schema query failed: {e}")))
    }
}

impl DatabaseProvider for SqliteProvider {
    fn name(&self) -> &str {
        NAME
    }

    /// SQLite runs in-process against a file. No container, no VM.
    fn local_engine(&self) -> Option<&dyn LocalEngine> {
        Some(self)
    }

    fn definition(&self) -> ComputeDefinition {
        Self::definition_impl()
    }

    fn default_port(&self) -> u16 {
        0
    }

    fn default_args(&self) -> Vec<DatabaseProviderArg> {
        vec![]
    }

    fn connection_string(
        &self,
        params: &ConnectionParams,
    ) -> std::result::Result<String, ProviderError> {
        // `sqlite:///` plus an absolute path yields the four-slash form
        // (`sqlite:////var/db.sqlite`) that SQLAlchemy, Diesel and the Rails
        // sqlite3 adapter all accept for an absolute file URL.
        Ok(format!("sqlite:///{}", Self::db_path(params)?.display()))
    }

    fn supported_versions(&self) -> Vec<String> {
        vec!["3".into()]
    }

    fn supported_features(&self) -> Vec<SupportedFeature> {
        vec![
            SupportedFeature {
                id: "schema".into(),
                description: "Schema and DDL inspection".into(),
            },
            SupportedFeature {
                id: "import".into(),
                description: "Import a SQL dump into the database".into(),
            },
            SupportedFeature {
                id: "export".into(),
                description: "Export the database as a SQL dump".into(),
            },
        ]
    }

    /// Nothing for a compute runtime to execute.
    ///
    /// SQLite has no instance to `exec` into; the equivalent preparation runs in
    /// this process via [`LocalEngine::prepare_for_snapshot`].
    fn prepare_for_snapshot(&self, _params: &ConnectionParams) -> Result<Vec<String>> {
        Ok(vec![])
    }

    fn query_client_command(
        &self,
        params: &ConnectionParams,
        query: Option<&str>,
    ) -> std::result::Result<std::process::Command, ProviderError> {
        // The interactive shell is the one place a host `sqlite3` is still used:
        // it is a REPL, not something this crate reimplements. Everything GFS
        // itself does runs through the linked engine instead.
        let mut cmd = std::process::Command::new("sqlite3");
        cmd.arg(Self::db_path(params)?);
        if let Some(q) = query {
            cmd.arg(q);
        }
        Ok(cmd)
    }

    /// SQL emitting each schema section as a JSON array.
    ///
    /// Nested arrays are wrapped in `json()` so they embed as arrays rather than
    /// as strings holding JSON, and booleans go through `json('true')` /
    /// `json('false')` because SQLite has no boolean literal.
    fn schema_extraction_queries(&self) -> std::collections::HashMap<String, String> {
        let mut queries = std::collections::HashMap::new();

        queries.insert("version".into(), "SELECT sqlite_version();".into());

        // SQLite has exactly one user namespace, so the schema list is a constant.
        queries.insert(
            "schemas".into(),
            format!(
                "SELECT json_array(json_object('id', 1, 'name', '{MAIN_SCHEMA}', 'owner', '{MAIN_SCHEMA}'));"
            ),
        );

        // Size and row-count statistics have no cheap SQLite equivalent, so they
        // are reported as zero rather than guessed.
        queries.insert(
            "tables".into(),
            format!(
                "SELECT json_group_array(json_object(\
                   'id', t.id, \
                   'schema', '{MAIN_SCHEMA}', \
                   'name', t.name, \
                   'rls_enabled', json('false'), \
                   'rls_forced', json('false'), \
                   'bytes', 0, \
                   'size', '0 bytes', \
                   'live_rows_estimate', 0, \
                   'dead_rows_estimate', 0, \
                   'comment', null, \
                   'primary_keys', json((SELECT json_group_array(json_object(\
                       'table_id', t.id, 'name', p.name, \
                       'schema', '{MAIN_SCHEMA}', 'table_name', t.name)) \
                     FROM pragma_table_info(t.name) p WHERE p.pk > 0)), \
                   'relationships', json((SELECT json_group_array(json_object(\
                       'id', f.id, 'constraint_name', '', \
                       'source_schema', '{MAIN_SCHEMA}', 'source_table_name', t.name, \
                       'source_column_name', f.\"from\", \
                       'target_table_schema', '{MAIN_SCHEMA}', 'target_table_name', f.\"table\", \
                       'target_column_name', coalesce(f.\"to\", ''))) \
                     FROM pragma_foreign_key_list(t.name) f)) \
                 )) FROM ({TABLE_IDS}) t;"
            ),
        );

        // `ordinal_position` is 1-based to match the postgres provider; SQLite's
        // `cid` is 0-based.
        queries.insert(
            "columns".into(),
            format!(
                "SELECT json_group_array(json_object(\
                   'id', t.name || '.' || p.name, \
                   'table_id', t.id, \
                   'schema', '{MAIN_SCHEMA}', \
                   'table', t.name, \
                   'name', p.name, \
                   'ordinal_position', p.cid + 1, \
                   'data_type', coalesce(nullif(p.type, ''), 'BLOB'), \
                   'format', coalesce(nullif(p.type, ''), 'BLOB'), \
                   'is_identity', json('false'), \
                   'identity_generation', null, \
                   'is_generated', json('false'), \
                   'is_nullable', json({NULLABLE}), \
                   'is_updatable', json('true'), \
                   'is_unique', json({UNIQUE}), \
                   'check', null, \
                   'default_value', p.dflt_value, \
                   'enums', json('[]'), \
                   'comment', null \
                 )) FROM ({TABLE_IDS}) t JOIN pragma_table_info(t.name) p;",
                NULLABLE = IS_NULLABLE,
                UNIQUE = IS_UNIQUE,
            ),
        );

        queries
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
            description: "SQL script".into(),
            file_extension: ".sql".into(),
        }]
    }

    /// Dump the database as SQL into the artifact directory.
    ///
    /// The output path is absolute. A redirect relative to the working directory
    /// would land outside the directory the caller collects the artifact from.
    fn export_spec(
        &self,
        params: &ConnectionParams,
        format: &str,
    ) -> std::result::Result<ExportSpec, ProviderError> {
        let (verb, filename) = match format {
            "sql" => (".dump", "export.sql"),
            "schema" => (".schema", "schema.sql"),
            other => return Err(ProviderError::UnsupportedFormat(other.to_string())),
        };

        Ok(ExportSpec {
            definition: Self::definition_impl(),
            command: format!(
                "sqlite3 {db} {verb} > {ARTIFACT_DIR}/{filename}",
                db = Self::shell_quote(&Self::db_path(params)?.to_string_lossy()),
            ),
            output_filename: filename.to_string(),
        })
    }

    /// Replay a SQL script into the database, reading from an absolute path.
    fn import_spec(
        &self,
        params: &ConnectionParams,
        format: &str,
        input_filename: &str,
    ) -> std::result::Result<ImportSpec, ProviderError> {
        if format != "sql" {
            return Err(ProviderError::UnsupportedFormat(format.to_string()));
        }

        Ok(ImportSpec {
            definition: Self::definition_impl(),
            command: format!(
                "sqlite3 {db} < {ARTIFACT_DIR}/{input_filename}",
                db = Self::shell_quote(&Self::db_path(params)?.to_string_lossy()),
            ),
            input_filename: input_filename.to_string(),
        })
    }
}

/// In-process execution against the linked SQLite amalgamation.
impl LocalEngine for SqliteProvider {
    /// Run [`SqliteProvider::schema_extraction_queries`] against the linked
    /// engine and emit the delimited payload the shared parser consumes.
    ///
    /// A repository that has been initialised but never written to has no
    /// database file yet. That is reported as an empty schema rather than an
    /// error, so the first `gfs commit` succeeds.
    fn extract_schema(
        &self,
        params: &ConnectionParams,
    ) -> std::result::Result<String, ProviderError> {
        let path = Self::db_path(params)?;
        let queries = self.schema_extraction_queries();
        let get = |key: &str| -> std::result::Result<String, ProviderError> {
            queries
                .get(key)
                .cloned()
                .ok_or_else(|| ProviderError::InvalidParams(format!("missing {key} query")))
        };

        // `rusqlite::version()` reports the linked amalgamation, so the version
        // is known even when there is no file to open.
        if !path.exists() {
            return Ok(render_sections(
                rusqlite::version(),
                &format!(r#"[{{"id":1,"name":"{MAIN_SCHEMA}","owner":"{MAIN_SCHEMA}"}}]"#),
                "[]",
                "[]",
                "",
            ));
        }

        let conn = Self::open(&path)?;
        let version = Self::scalar(&conn, &get("version")?)?;
        let schemas = Self::scalar(&conn, &get("schemas")?)?;
        let tables = Self::scalar(&conn, &get("tables")?)?;
        let columns = Self::scalar(&conn, &get("columns")?)?;
        let ddl = Self::scalar(&conn, DDL_QUERY)?;

        Ok(render_sections(&version, &schemas, &tables, &columns, &ddl))
    }

    /// Collapse the write-ahead log into the main database before a snapshot.
    ///
    /// `TRUNCATE` (rather than `PASSIVE`) blocks until every committed frame has
    /// been written back, then resets the WAL to zero length. That reduces the
    /// on-disk database to a single file, so the storage layer captures one file
    /// at one instant instead of three files at three instants.
    ///
    /// Per the trait contract this does not freeze the database: the connection
    /// closes before the snapshot is taken. Excluding writers would need a
    /// connection held open across the storage operation, which the commit path
    /// offers no seam for.
    fn prepare_for_snapshot(
        &self,
        params: &ConnectionParams,
    ) -> std::result::Result<bool, ProviderError> {
        let path = Self::db_path(params)?;
        if !path.exists() {
            // Nothing has been written yet; there is no WAL to fold in.
            return Ok(false);
        }
        let conn = Self::open(&path)?;
        conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
            .map_err(|e| ProviderError::InvalidParams(format!("wal checkpoint failed: {e}")))?;
        Ok(true)
    }
}

/// Assemble the delimited schema payload the shared parser expects.
fn render_sections(version: &str, schemas: &str, tables: &str, columns: &str, ddl: &str) -> String {
    format!(
        "GFS_SCHEMA_VERSION\n{version}\n\
         GFS_SCHEMA_SCHEMAS\n{schemas}\n\
         GFS_SCHEMA_TABLES\n{tables}\n\
         GFS_SCHEMA_COLUMNS\n{columns}\n\
         GFS_SCHEMA_DDL\n{ddl}\n"
    )
}

/// Registers the SQLite provider in `registry` under the name `"sqlite"`.
pub fn register(registry: &impl DatabaseProviderRegistry) -> Result<()> {
    registry.register(Arc::new(SqliteProvider::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params_with_path(path: &str) -> ConnectionParams {
        ConnectionParams {
            host: String::new(),
            port: 0,
            env: vec![(ENV_DB_PATH.to_string(), path.to_string())],
        }
    }

    /// A database seeded with a parent/child pair, exercising primary keys,
    /// a foreign key, nullability and defaults.
    fn seeded_db(dir: &std::path::Path) -> ConnectionParams {
        let path = dir.join(DB_FILENAME);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE author (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             CREATE TABLE book (
                 id INTEGER PRIMARY KEY,
                 author_id INTEGER REFERENCES author(id),
                 title TEXT NOT NULL,
                 rating INTEGER DEFAULT 3
             );",
        )
        .unwrap();
        params_with_path(path.to_str().unwrap())
    }

    #[test]
    fn registers_as_sqlite_and_needs_no_compute() {
        let p = SqliteProvider::new();
        assert_eq!(p.name(), "sqlite");
        assert!(!p.requires_compute(), "derived from local_engine()");
        assert!(p.local_engine().is_some());
        assert_eq!(p.default_port(), 0);
    }

    #[test]
    fn definition_carries_labels_and_version_tag() {
        let p = SqliteProvider::new();
        let def = p.definition();
        assert!(def.labels.is_empty(), "labels field must be present");
        // `init` rewrites the tag to the configured version and reads it back
        // with `version_from_image`, so the base must be splittable on ':'.
        assert_eq!(def.image, "sqlite:3");
        assert_eq!(p.version_from_image(&def), "3");
    }

    #[test]
    fn connection_string_is_an_absolute_file_url() {
        let p = SqliteProvider::new();
        assert_eq!(
            p.connection_string(&params_with_path("/srv/data/db.sqlite"))
                .unwrap(),
            "sqlite:////srv/data/db.sqlite"
        );
    }

    #[test]
    fn missing_path_is_reported_as_a_missing_env_var() {
        let p = SqliteProvider::new();
        let empty = ConnectionParams::default();
        assert!(matches!(
            p.connection_string(&empty),
            Err(ProviderError::MissingEnvVar(_))
        ));
        assert!(matches!(
            p.query_client_command(&empty, None),
            Err(ProviderError::MissingEnvVar(_))
        ));
        assert!(matches!(
            LocalEngine::extract_schema(&p, &empty),
            Err(ProviderError::MissingEnvVar(_))
        ));
    }

    #[test]
    fn query_client_command_opens_a_shell_or_runs_one_statement() {
        let p = SqliteProvider::new();
        let params = params_with_path("/srv/db.sqlite");

        let interactive = p.query_client_command(&params, None).unwrap();
        assert_eq!(interactive.get_program(), "sqlite3");
        let args: Vec<_> = interactive.get_args().collect();
        assert_eq!(args, ["/srv/db.sqlite"]);

        let with_query = p.query_client_command(&params, Some("SELECT 1;")).unwrap();
        let args: Vec<_> = with_query.get_args().collect();
        assert_eq!(args, ["/srv/db.sqlite", "SELECT 1;"]);
    }

    #[test]
    fn nothing_is_delegated_to_a_compute_runtime() {
        let p = SqliteProvider::new();
        assert!(
            DatabaseProvider::prepare_for_snapshot(&p, &params_with_path("/srv/db.sqlite"))
                .unwrap()
                .is_empty(),
            "there is no instance to exec into"
        );
    }

    #[test]
    fn checkpoint_folds_the_wal_back_into_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let params = seeded_db(dir.path());
        let db = dir.path().join(DB_FILENAME);

        // The writer stays open for the whole test. A clean close would
        // checkpoint and delete the WAL by itself, which is exactly the case
        // this test must not measure: the interesting scenario is a snapshot
        // taken while an application still holds the database.
        let writer = rusqlite::Connection::open(&db).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer
            .execute("INSERT INTO author (name) VALUES ('ada')", [])
            .unwrap();

        let wal = dir.path().join(format!("{DB_FILENAME}-wal"));
        assert!(
            std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0) > 0,
            "an open writer should leave frames in the WAL"
        );

        assert!(
            LocalEngine::prepare_for_snapshot(&SqliteProvider::new(), &params).unwrap(),
            "checkpoint should report that it ran"
        );
        assert_eq!(
            std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0),
            0,
            "TRUNCATE must reset the WAL to zero length"
        );
    }

    #[test]
    fn checkpoint_on_a_repository_with_no_database_yet_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let params = params_with_path(dir.path().join(DB_FILENAME).to_str().unwrap());
        assert!(
            !LocalEngine::prepare_for_snapshot(&SqliteProvider::new(), &params).unwrap(),
            "a commit before the first write must not fail"
        );
    }

    #[test]
    fn schema_extraction_emits_every_section_the_parser_expects() {
        let dir = tempfile::tempdir().unwrap();
        let params = seeded_db(dir.path());
        let out = SqliteProvider::new()
            .extract_schema(&params)
            .expect("sqlite extracts schema locally");

        for delimiter in [
            "GFS_SCHEMA_VERSION",
            "GFS_SCHEMA_SCHEMAS",
            "GFS_SCHEMA_TABLES",
            "GFS_SCHEMA_COLUMNS",
            "GFS_SCHEMA_DDL",
        ] {
            assert!(out.contains(delimiter), "missing {delimiter} section");
        }
        assert!(out.contains("CREATE TABLE author"), "DDL must be captured");
    }

    /// The sections must deserialise into the very types the domain parser
    /// targets, or schema capture fails at runtime instead of here.
    #[test]
    fn extracted_sections_are_well_formed_json() {
        let dir = tempfile::tempdir().unwrap();
        let params = seeded_db(dir.path());
        let out = SqliteProvider::new().extract_schema(&params).unwrap();

        let section = |name: &str| -> String {
            out.split(&format!("{name}\n"))
                .nth(1)
                .unwrap()
                .lines()
                .next()
                .unwrap()
                .to_string()
        };

        let tables: serde_json::Value =
            serde_json::from_str(&section("GFS_SCHEMA_TABLES")).unwrap();
        let tables = tables.as_array().unwrap();
        assert_eq!(tables.len(), 2, "author and book");
        assert_eq!(tables[0]["name"], "author");
        assert_eq!(tables[0]["schema"], MAIN_SCHEMA);
        // Booleans must be JSON booleans, not the strings "true"/"false".
        assert!(tables[0]["rls_enabled"].is_boolean());
        // Nested arrays must embed as arrays, not as strings holding JSON.
        assert!(tables[0]["primary_keys"].is_array());
        assert_eq!(tables[0]["primary_keys"][0]["name"], "id");

        let book = &tables[1];
        assert_eq!(book["name"], "book");
        assert_eq!(book["relationships"][0]["target_table_name"], "author");
        assert_eq!(book["relationships"][0]["source_column_name"], "author_id");

        let columns: serde_json::Value =
            serde_json::from_str(&section("GFS_SCHEMA_COLUMNS")).unwrap();
        let columns = columns.as_array().unwrap();
        assert_eq!(columns.len(), 6);

        let title = columns
            .iter()
            .find(|c| c["name"] == "title" && c["table"] == "book")
            .unwrap();
        assert_eq!(title["data_type"], "TEXT");
        assert_eq!(title["is_nullable"], false, "declared NOT NULL");
        assert_eq!(title["ordinal_position"], 3, "1-based, like postgres");
        assert_eq!(title["id"], "book.title");

        let rating = columns.iter().find(|c| c["name"] == "rating").unwrap();
        assert_eq!(rating["default_value"], "3");
        assert_eq!(rating["is_nullable"], true);

        // A column's table_id must match the id reported for its table.
        let book_id = book["id"].as_i64().unwrap();
        assert_eq!(title["table_id"].as_i64().unwrap(), book_id);
    }

    /// Nullability and uniqueness must follow what SQLite will actually store,
    /// not what `pragma_table_info` reports on its own.
    ///
    /// Each expectation below was confirmed by inserting NULL into that form and
    /// reading it back. The surprising ones are `INTEGER PRIMARY KEY DESC` and
    /// `INT PRIMARY KEY`: neither is a rowid alias, and both really do store
    /// NULLs, so reporting every primary key as NOT NULL would be wrong.
    #[test]
    fn nullability_and_uniqueness_follow_real_sqlite_behaviour() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DB_FILENAME);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE rowid_alias   (id INTEGER PRIMARY KEY, v TEXT);
             CREATE TABLE autoinc       (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT);
             CREATE TABLE lower_ip      (id integer primary key, v TEXT);
             CREATE TABLE alias_desc    (id INTEGER PRIMARY KEY DESC, v TEXT);
             CREATE TABLE int_pk        (id INT PRIMARY KEY, v TEXT);
             CREATE TABLE text_pk       (id TEXT PRIMARY KEY, v TEXT);
             CREATE TABLE without_rowid (id TEXT PRIMARY KEY, v TEXT) WITHOUT ROWID;
             CREATE TABLE composite     (a INT, b INT, v TEXT, PRIMARY KEY(a, b));
             CREATE TABLE uniq_col      (id INTEGER PRIMARY KEY, email TEXT UNIQUE);
             CREATE TABLE uniq_multi    (id INTEGER PRIMARY KEY, x INT, y INT, UNIQUE(x, y));",
        )
        .unwrap();
        drop(conn);

        let out = SqliteProvider::new()
            .extract_schema(&params_with_path(path.to_str().unwrap()))
            .unwrap();
        let section = out
            .split("GFS_SCHEMA_COLUMNS\n")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        let columns: serde_json::Value = serde_json::from_str(&section).unwrap();
        let columns = columns.as_array().unwrap();

        let col = |table: &str, name: &str| -> &serde_json::Value {
            columns
                .iter()
                .find(|c| c["table"] == table && c["name"] == name)
                .unwrap_or_else(|| panic!("missing {table}.{name}"))
        };

        // Rowid aliases: an inserted NULL is replaced by a generated rowid, so a
        // NULL can never be read back.
        for table in ["rowid_alias", "autoinc", "lower_ip"] {
            assert_eq!(col(table, "id")["is_nullable"], false, "{table}.id");
            assert_eq!(col(table, "id")["is_unique"], true, "{table}.id");
        }

        // Not rowid aliases, and SQLite really does store NULLs in them.
        for table in ["alias_desc", "int_pk", "text_pk"] {
            assert_eq!(col(table, "id")["is_nullable"], true, "{table}.id");
        }

        // WITHOUT ROWID enforces NOT NULL, and reports it.
        assert_eq!(col("without_rowid", "id")["is_nullable"], false);

        // A composite key constrains the pair; neither column is unique alone.
        assert_eq!(col("composite", "a")["is_unique"], false);
        assert_eq!(col("composite", "b")["is_unique"], false);

        // A single-column UNIQUE index counts, without being a primary key.
        assert_eq!(col("uniq_col", "email")["is_unique"], true);
        assert_eq!(col("uniq_col", "email")["is_nullable"], true);
        assert_eq!(col("rowid_alias", "v")["is_unique"], false);

        // A multi-column UNIQUE index constrains the combination, so neither
        // column is unique on its own — `UNIQUE(x, y)` still admits two rows
        // sharing an `x`. Treating any covering unique index as proof would get
        // this wrong.
        assert_eq!(col("uniq_multi", "x")["is_unique"], false);
        assert_eq!(col("uniq_multi", "y")["is_unique"], false);
    }

    #[test]
    fn schema_extraction_before_the_first_write_reports_an_empty_schema() {
        let dir = tempfile::tempdir().unwrap();
        let params = params_with_path(dir.path().join(DB_FILENAME).to_str().unwrap());
        let out = SqliteProvider::new().extract_schema(&params).unwrap();
        assert!(out.contains("GFS_SCHEMA_VERSION"));
        assert!(out.contains(rusqlite::version()));
        assert!(out.contains("GFS_SCHEMA_TABLES\n[]"));
    }

    /// The recorded version must come from the linked engine, so two machines
    /// record the same value for the same schema.
    #[test]
    fn recorded_version_is_the_linked_engine_not_a_host_binary() {
        let dir = tempfile::tempdir().unwrap();
        let params = seeded_db(dir.path());
        let out = SqliteProvider::new().extract_schema(&params).unwrap();
        let reported = out.lines().nth(1).unwrap();
        assert_eq!(reported, rusqlite::version());
    }

    #[test]
    fn export_and_import_use_absolute_artifact_paths() {
        let p = SqliteProvider::new();
        let params = params_with_path("/srv/db.sqlite");

        let export = p.export_spec(&params, "sql").unwrap();
        assert_eq!(export.output_filename, "export.sql");
        assert!(
            export.command.contains("> /data/export.sql"),
            "redirect must be absolute, not relative to the working directory"
        );

        let schema_only = p.export_spec(&params, "schema").unwrap();
        assert!(schema_only.command.contains(".schema"));
        assert_eq!(schema_only.output_filename, "schema.sql");

        let import = p.import_spec(&params, "sql", "seed.sql").unwrap();
        assert!(import.command.contains("< /data/seed.sql"));
        assert_eq!(import.input_filename, "seed.sql");
    }

    #[test]
    fn shell_metacharacters_in_the_path_cannot_escape_the_quoting() {
        let p = SqliteProvider::new();
        let spec = p
            .export_spec(&params_with_path("/tmp/it's here; rm -rf /"), "sql")
            .unwrap();
        // The apostrophe is closed, escaped and reopened, so the `;` stays inside
        // the quoted argument instead of becoming a command separator.
        assert!(spec.command.contains(r"'/tmp/it'\''s here; rm -rf /'"));
    }

    #[test]
    fn unsupported_formats_are_rejected() {
        let p = SqliteProvider::new();
        let params = params_with_path("/srv/db.sqlite");
        assert!(matches!(
            p.export_spec(&params, "csv"),
            Err(ProviderError::UnsupportedFormat(_))
        ));
        assert!(matches!(
            p.import_spec(&params, "csv", "x.csv"),
            Err(ProviderError::UnsupportedFormat(_))
        ));
    }
}
