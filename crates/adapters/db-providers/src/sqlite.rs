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
//! consistency therefore comes from SQLite's own locking:
//! [`LocalEngine::prepare_for_snapshot`] holds the write lock for as long as the
//! storage layer is copying, which stops writers cross-process in a way pausing
//! a compute instance never could.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gfs_domain::ports::database_provider::{
    ConnectionParams, DataFormat, DatabaseProvider, DatabaseProviderRegistry, LOCAL_DATA_DIR_ENV,
    LocalEngine, ProviderError, Result, SnapshotGuard, SupportedFeature,
};

const NAME: &str = "sqlite";

/// Filename of the database inside the workspace data directory.
pub const DB_FILENAME: &str = "db.sqlite";

/// Escape hatch naming the database file outright.
///
/// Read from [`ConnectionParams`] first and then from the process environment,
/// so it is settable by an operator and not only by a caller — it was
/// previously documented as an override while being reachable from nothing but
/// a test helper. Use it when the workspace holds more than one database, or
/// when the file lives somewhere discovery will not look.
pub const ENV_DB_PATH: &str = "GFS_SQLITE_DB_PATH";

/// Single namespace SQLite exposes. Mirrors postgres's `public`.
const MAIN_SCHEMA: &str = "main";

/// Subquery assigning each user table a stable id.
///
/// Sourced from `pragma_table_list` rather than `sqlite_master` so SQLite can
/// classify the rows for us: an FTS5 table contributes five `shadow` tables
/// (`_data`, `_idx`, `_content`, `_docsize`, `_config`) that are implementation
/// detail, not user schema. `virtual` is kept — the FTS5 table itself is
/// something the user created and expects to see.
///
/// Used verbatim by both the tables and the columns query so a column's
/// `table_id` refers to the same table the tables section reported. `ORDER BY
/// name` makes the numbering deterministic across runs, which keeps the schema
/// hash stable when nothing has actually changed.
const TABLE_IDS: &str = "SELECT ROW_NUMBER() OVER (ORDER BY name) AS id, name \
     FROM pragma_table_list \
     WHERE schema = 'main' AND type IN ('table', 'virtual') \
       AND name NOT LIKE 'sqlite~_%' ESCAPE '~'";

/// Schema-only DDL, in place of the `sqlite3` shell's `.schema` dot-command
/// (which is a feature of that client, not of the engine).
const DDL_QUERY: &str = "SELECT coalesce(group_concat(sql, ';\n'), '') || ';' \
     FROM sqlite_master \
     WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite~_%' ESCAPE '~' \
       AND name NOT IN (SELECT name FROM pragma_table_list WHERE type = 'shadow');";

/// DDL for the objects that must exist BEFORE the data is replayed.
///
/// Tables only — including virtual tables, whose `CREATE` builds an empty
/// index that the inserts below repopulate. Shadow tables are excluded: they
/// are rebuilt from those inserts.
const TABLE_DDL_QUERY: &str = "SELECT coalesce(group_concat(sql, ';\n'), '') \
     FROM sqlite_master \
     WHERE type = 'table' AND sql IS NOT NULL \
       AND name NOT LIKE 'sqlite~_%' ESCAPE '~' \
       AND name NOT IN (SELECT name FROM pragma_table_list WHERE type = 'shadow');";

/// DDL for the objects that must come AFTER the data.
///
/// Triggers above all: replaying inserts with an `AFTER INSERT` trigger already
/// installed fires it for every row, so a restore silently gained rows the
/// source never had. Real `sqlite3 .dump` emits views, triggers and indexes
/// last for exactly this reason, and building the indexes after the bulk load
/// is faster besides.
const POST_DATA_DDL_QUERY: &str = "SELECT coalesce(group_concat(sql, ';\n'), '') \
     FROM sqlite_master \
     WHERE type IN ('view', 'trigger', 'index') AND sql IS NOT NULL \
       AND name NOT LIKE 'sqlite~_%' ESCAPE '~';";

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
/// is the whole key. A single-column `UNIQUE` index qualifies too — but only
/// one that constrains every row. `CREATE UNIQUE INDEX ... WHERE deleted = 0`
/// leaves duplicates genuinely storable outside the matching rows.
///
/// `WHERE <col> IS NOT NULL` is the exception, and the reason
/// [`predicate_is_only_not_null`] exists: SQLite already permits any number of
/// NULLs in a unique index, so excluding the null rows removes exactly the rows
/// the index was ignoring anyway. Verified by inserting into both forms — a
/// partial `IS NOT NULL` index and a plain one accept and reject exactly the
/// same rows.
///
/// NOT covered, deliberately: a unique index on an EXPRESSION,
/// `CREATE UNIQUE INDEX ix ON t(lower(a))`, does make `a` unique — uniqueness
/// of a function of `a` is a stronger constraint than uniqueness of `a` — and
/// is reported as non-unique here. It is indistinguishable through the pragmas
/// from `CREATE UNIQUE INDEX ix ON t(a || b)`, which makes NEITHER column
/// unique: both report one key column with `cid = -2` and no name. Telling them
/// apart means tokenising the index DDL to find which of the table's columns
/// the expression mentions, and a tokeniser that misses a reference would claim
/// a column is unique when it is not. Under-claiming loses a fact; over-claiming
/// invents one, so this stays under-claimed until it is worth doing exactly.
const IS_UNIQUE: &str = "CASE \
     WHEN p.pk = 1 AND (SELECT max(pk) FROM pragma_table_info(t.name)) = 1 THEN 'true' \
     WHEN EXISTS (SELECT 1 FROM pragma_index_list(t.name) il \
                  WHERE il.\"unique\" = 1 \
                    AND (il.partial = 0 \
                         OR gfs_predicate_is_only_not_null(\
                              (SELECT m.sql FROM sqlite_master m \
                                WHERE m.type = 'index' AND m.name = il.name), \
                              p.name)) \
                    AND (SELECT count(*) FROM pragma_index_info(il.name)) = 1 \
                    AND (SELECT ii.name FROM pragma_index_info(il.name) ii) = p.name) THEN 'true' \
     ELSE 'false' END";

/// Whether `index_sql` is a partial index whose predicate is exactly
/// `<column> IS NOT NULL`.
///
/// Such a predicate cannot exclude a row the index would otherwise constrain,
/// because a unique index already tolerates any number of NULLs. So the column
/// is unique, and reporting otherwise loses a true fact.
///
/// Only that one shape is recognised, in SQLite's four identifier quotings.
/// Anything else — a real filter, or a spelling this does not match — returns
/// false and the column is reported non-unique. That is the safe direction, and
/// it is why this compares against a closed list of forms rather than parsing.
fn predicate_is_only_not_null(index_sql: &str, column: &str) -> bool {
    // Collapse runs of whitespace and lower-case, so the comparison does not
    // depend on how the DDL was typed. SQLite stores it verbatim.
    let mut normalised = String::with_capacity(index_sql.len());
    let mut pending_space = false;
    for ch in index_sql.chars() {
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !normalised.is_empty() {
            normalised.push(' ');
        }
        pending_space = false;
        normalised.extend(ch.to_lowercase());
    }

    let column = column.to_lowercase();
    ["{}", "\"{}\"", "[{}]", "`{}`"].iter().any(|quoting| {
        let quoted = quoting.replace("{}", &column);
        normalised.ends_with(&format!(" where {quoted} is not null"))
            || normalised.ends_with(&format!(" where ({quoted} is not null)"))
    })
}

/// Make [`predicate_is_only_not_null`] callable from the schema queries.
///
/// The question needs the index DDL, which no pragma exposes, and answering it
/// in SQL string functions would be a worse tokeniser than a Rust one. The
/// function is registered on the connection that runs those queries, which is
/// also the only place they run: `schema_extraction_queries` for this provider
/// has exactly one consumer, `extract_schema`, a few lines below.
fn register_helpers(conn: &rusqlite::Connection) -> std::result::Result<(), ProviderError> {
    use rusqlite::functions::FunctionFlags;
    conn.create_scalar_function(
        "gfs_predicate_is_only_not_null",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            // An auto-index created for a UNIQUE constraint has no DDL of its
            // own, so NULL here is normal and simply means "not that shape".
            let (Some(sql), Some(column)) =
                (ctx.get_raw(0).as_str().ok(), ctx.get_raw(1).as_str().ok())
            else {
                return Ok(false);
            };
            Ok(predicate_is_only_not_null(sql, column))
        },
    )
    .map_err(|e| ProviderError::InvalidParams(format!("cannot register schema helper: {e}")))
}

/// Whether the workspace already holds a database, and where.
///
/// Kept distinct so "nothing has been written yet" — a legitimate state for a
/// fresh repository — cannot be confused with "there is a database and I did
/// not find it", which must never be treated as success.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Located {
    Existing(PathBuf),
    Absent(PathBuf),
}

impl Located {
    fn into_path(self) -> PathBuf {
        match self {
            Located::Existing(p) | Located::Absent(p) => p,
        }
    }
}

/// Whether `path` is a symbolic link, without following it.
fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Resolve `p` as far as the filesystem allows, and lexically for the rest.
///
/// `canonicalize` fails outright on a path that does not exist — which is the
/// normal case for a database GFS has not seen written yet. Falling back to the
/// raw path then compares an unresolved candidate against a resolved root, and
/// on macOS, where `/tmp` and `/var` are symlinks into `/private`, a file
/// genuinely inside the workspace reads as outside it.
///
/// So the longest existing ANCESTOR is canonicalised and the remaining names
/// are appended. `.` and `..` are removed first, because a path that cannot be
/// canonicalised keeps them, and `<workspace>/../etc/passwd` starts with
/// `<workspace>` as a string while naming somewhere else entirely.
fn resolved(p: &Path) -> PathBuf {
    if let Ok(real) = p.canonicalize() {
        return real;
    }

    let mut lexical = PathBuf::new();
    for component in p.components() {
        match component {
            std::path::Component::ParentDir => {
                // At the root this is a no-op, which is what POSIX does too.
                lexical.pop();
            }
            std::path::Component::CurDir => {}
            other => lexical.push(other.as_os_str()),
        }
    }

    let mut trailing: Vec<std::ffi::OsString> = Vec::new();
    let mut current: &Path = &lexical;
    while let (Some(parent), Some(name)) = (current.parent(), current.file_name()) {
        trailing.push(name.to_os_string());
        if let Ok(real) = parent.canonicalize() {
            let mut out = real;
            out.extend(trailing.iter().rev());
            return out;
        }
        current = parent;
    }
    lexical
}

/// Whether `candidate` lies inside `root`.
///
/// Both sides go through [`resolved`], so `..`, a symlinked workspace, a
/// database that does not exist yet and differing but equivalent spellings all
/// compare correctly.
fn is_within(root: &Path, candidate: &Path) -> bool {
    resolved(candidate).starts_with(resolved(root))
}

/// Collect every SQLite database under `dir`, recursively.
///
/// Recursion is what finds a Rails-style `storage/development.sqlite3`. Depth
/// is capped and symlinked directories are not followed, so a cyclic or
/// pathological tree cannot hang a commit. Errors are ignored deliberately:
/// an unreadable subdirectory should narrow the search, not fail it — the
/// ambiguity check below still refuses when the result is not a single file.
fn collect_sqlite_databases(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    const MAX_DEPTH: usize = 16;
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_symlink(&path) {
            // Not followed, but a symlink that IS the database is reported so
            // the caller can refuse rather than silently skip it.
            if entry.metadata().is_ok_and(|m| m.is_file()) && is_sqlite_database(&path) {
                out.push(path);
            }
            continue;
        }
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect_sqlite_databases(&path, depth + 1, out),
            Ok(t) if t.is_file() && is_sqlite_database(&path) => out.push(path),
            _ => {}
        }
    }
}

/// Whether `path` is a SQLite database, by its header rather than its name.
///
/// Every SQLite database begins with the 16 bytes `SQLite format 3\0`. Testing
/// the content means a sidecar `-wal`, `-shm` or `-journal` — which have their
/// own distinct headers — is never mistaken for the database itself, and a
/// database under any name is still found.
fn is_sqlite_database(path: &Path) -> bool {
    use std::io::Read;
    if !path.is_file() {
        return false;
    }
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut header = [0u8; 16];
    f.read_exact(&mut header).is_ok() && &header == b"SQLite format 3\0"
}

/// SQLite database provider. Implements [`DatabaseProvider`] without requiring
/// a compute instance.
#[derive(Debug, Clone)]
pub struct SqliteProvider {
    /// How long to wait for the write lock before reporting contention.
    ///
    /// Injectable so a test can assert real contention against a really-held
    /// lock without paying the production budget. Weakening the assertion
    /// instead — asserting on a message, or not holding a lock at all — would
    /// stop testing the thing that matters.
    lock_timeout: std::time::Duration,
}

impl Default for SqliteProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SqliteProvider {
    pub fn new() -> Self {
        Self {
            lock_timeout: LOCK_TIMEOUT,
        }
    }

    /// A provider that gives up on the write lock after `timeout`.
    pub fn with_lock_timeout(timeout: std::time::Duration) -> Self {
        Self {
            lock_timeout: timeout,
        }
    }

    /// Absolute path of the database file.
    ///
    /// Normally the workspace data directory the caller supplied, joined with
    /// this provider's filename — the caller deliberately does not know the
    /// filename. [`ENV_DB_PATH`] overrides it outright.
    /// Where this workspace's database is, and whether it exists yet.
    ///
    /// Resolution order: an explicit override, then the conventional
    /// [`DB_FILENAME`], then a single SQLite file discovered anywhere under the
    /// workspace, then "none yet".
    ///
    /// Discovery matters because GFS does not create the database — the user's
    /// application does, under whatever name and in whatever directory it
    /// likes. Rails 7.1 keeps it at `storage/development.sqlite3`, a
    /// SUBDIRECTORY, which is why the search recurses: a top-level-only scan
    /// resolved that layout to "nothing here", so the snapshot guard never ran
    /// and a mid-transaction copy was recorded as a commit, with an empty
    /// schema attached, as though it were the truth.
    ///
    /// Everything this returns must lie inside the directory the storage layer
    /// is about to copy. A path outside it would be locked and inspected but
    /// never snapshotted, so the commit would describe a file it does not
    /// contain. That holds for every way in, the explicit override included:
    /// the workspace directory is required precisely so there is always
    /// something to check containment against.
    fn resolve(params: &ConnectionParams) -> std::result::Result<Located, ProviderError> {
        // Required, including when an explicit override is supplied. Making it
        // optional there left one entry shape with no containment check at all:
        // a caller passing only the override got a database locked, read and
        // schema-captured from outside whatever directory was being
        // snapshotted. The guard was not wrong in that shape, it was absent.
        let dir = params
            .get_env(LOCAL_DATA_DIR_ENV)
            .map(Path::new)
            .ok_or_else(|| ProviderError::MissingEnvVar(LOCAL_DATA_DIR_ENV.to_string()))?;

        // A path that exists but is not a regular file is a mistake worth
        // reporting. Treating it as "nothing here yet" would reintroduce the
        // silent no-op this resolver exists to remove.
        let at = |p: PathBuf| -> std::result::Result<Located, ProviderError> {
            if is_symlink(&p) {
                // `cp` copies the link, not the target, so a symlinked database
                // would be committed as a live pointer: mutate the target and
                // the "snapshot" changes; delete it and the snapshot is
                // unreadable.
                return Err(ProviderError::InvalidParams(format!(
                    "'{}' is a symbolic link. GFS snapshots the workspace by copying it, \
                     and a link would be copied instead of the database it points at, so \
                     the commit would not contain any data. Move the database into the \
                     workspace",
                    p.display()
                )));
            }
            if p.is_file() {
                Ok(Located::Existing(p))
            } else if p.exists() {
                Err(ProviderError::InvalidParams(format!(
                    "'{}' is not a regular file, so it cannot be a SQLite database",
                    p.display()
                )))
            } else {
                Ok(Located::Absent(p))
            }
        };

        // An explicit choice wins, but is still checked for the one property
        // the caller cannot waive: it has to be in what gets snapshotted. This
        // also catches a stale value, since the workspace directory changes on
        // every checkout while an absolute override does not.
        if let Some(explicit) = params
            .get_env(ENV_DB_PATH)
            .map(str::to_string)
            .or_else(|| std::env::var(ENV_DB_PATH).ok())
            .filter(|v| !v.trim().is_empty())
        {
            let chosen = PathBuf::from(explicit);
            if !is_within(dir, &chosen) {
                return Err(ProviderError::InvalidParams(format!(
                    "{ENV_DB_PATH} points at '{}', which is outside the workspace '{}'. \
                     Only files inside the workspace are snapshotted, so that database \
                     would be locked and read but never committed. If this value is left \
                     over from an earlier checkout, unset it",
                    chosen.display(),
                    dir.display()
                )));
            }
            return at(chosen);
        }

        let conventional = dir.join(DB_FILENAME);
        if conventional.exists() || is_symlink(&conventional) {
            return at(conventional);
        }

        let mut found = Vec::new();
        collect_sqlite_databases(dir, 0, &mut found);
        found.sort();

        match found.len() {
            0 => Ok(Located::Absent(conventional)),
            1 => Ok(Located::Existing(found.remove(0))),
            _ => {
                let names: Vec<_> = found
                    .iter()
                    .map(|p| {
                        p.strip_prefix(dir)
                            .unwrap_or(p)
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect();
                Err(ProviderError::InvalidParams(format!(
                    "the workspace holds {} SQLite databases ({}); GFS cannot tell which one \
                     to version. Keep one in the workspace, or set {ENV_DB_PATH} to choose",
                    names.len(),
                    names.join(", ")
                )))
            }
        }
    }

    /// The path, existing or not. Callers that only need somewhere to point a
    /// client use this; callers that must not invent an empty database check
    /// [`Located`] instead.
    fn db_path(params: &ConnectionParams) -> std::result::Result<PathBuf, ProviderError> {
        Ok(Self::resolve(params)?.into_path())
    }

    /// Tables whose rows should be dumped: real and virtual, never shadow.
    ///
    /// Virtual tables are included because `CREATE VIRTUAL TABLE` builds an
    /// EMPTY index — the content has to be re-inserted, which re-indexes it.
    /// Excluding them silently dropped every row of an FTS5 table.
    fn dumpable_tables(
        conn: &rusqlite::Connection,
    ) -> std::result::Result<Vec<String>, ProviderError> {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM pragma_table_list \
                 WHERE schema = 'main' AND type IN ('table', 'virtual') \
                   AND name NOT LIKE 'sqlite~_%' ESCAPE '~' ORDER BY name",
            )
            .map_err(|e| ProviderError::InvalidParams(format!("listing tables: {e}")))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| ProviderError::InvalidParams(format!("listing tables: {e}")))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| ProviderError::InvalidParams(format!("listing tables: {e}")))
    }

    /// Columns that carry stored values. Generated columns (hidden 2 and 3) are
    /// recomputed on replay, and hidden 1 belongs to a virtual table.
    fn insertable_columns(
        conn: &rusqlite::Connection,
        table: &str,
    ) -> std::result::Result<Vec<String>, ProviderError> {
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_xinfo(?1) WHERE hidden = 0 ORDER BY cid")
            .map_err(|e| ProviderError::InvalidParams(format!("columns of '{table}': {e}")))?;
        let rows = stmt
            .query_map([table], |r| r.get::<_, String>(0))
            .map_err(|e| ProviderError::InvalidParams(format!("columns of '{table}': {e}")))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| ProviderError::InvalidParams(format!("columns of '{table}': {e}")))
    }

    fn open(path: &Path) -> std::result::Result<rusqlite::Connection, ProviderError> {
        rusqlite::Connection::open(path)
            .map_err(|e| ProviderError::InvalidParams(format!("cannot open '{path:?}': {e}")))
    }

    /// Open without the ability to write.
    ///
    /// Schema extraction is documented as inspection, so it must not alter what
    /// a subsequent snapshot would capture. A read-write connection checkpoints
    /// and deletes the write-ahead log when the last handle closes, rewriting
    /// the main database and discarding the WAL.
    ///
    /// The guarantee this buys is precise: the main database stays byte-for-byte
    /// identical and a live WAL keeps its contents. It is NOT "touches no files
    /// at all" — reading a WAL database requires shared memory, so SQLite may
    /// create a `-shm` (and an empty `-wal`) if none exists. That also means a
    /// read-only *directory* cannot be inspected while the database is in WAL
    /// mode; `immutable=1` would lift that restriction but is unsafe here,
    /// since it promises nothing else is writing, which during a commit is
    /// exactly what we cannot promise.
    fn open_read_only(path: &Path) -> std::result::Result<rusqlite::Connection, ProviderError> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| ProviderError::InvalidParams(format!("cannot read '{path:?}': {e}")))?;
        register_helpers(&conn)?;
        Ok(conn)
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
                id: "export".into(),
                description: "Export the database as a replayable SQL dump".into(),
            },
            SupportedFeature {
                id: "import".into(),
                description: "Replay a SQL dump into the database".into(),
            },
        ]
    }

    fn query_client_command(
        &self,
        params: &ConnectionParams,
        query: Option<&str>,
    ) -> std::result::Result<std::process::Command, ProviderError> {
        // `gfs query` is the only thing that still needs a host `sqlite3` —
        // both the interactive shell and the one-shot form, which is how a
        // user writes to the database. Everything GFS does on its own behalf
        // (init, commit, schema, checkout, export, import) runs through the
        // linked engine and needs nothing installed.
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
                       'id', f.id, \
                       'constraint_name', 'fk_' || t.name || '_' || f.id, \
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
                   'is_generated', json(CASE WHEN p.hidden IN (2, 3) THEN 'true' ELSE 'false' END), \
                   'is_nullable', json({NULLABLE}), \
                   'is_updatable', json('true'), \
                   'is_unique', json({UNIQUE}), \
                   'check', null, \
                   'default_value', p.dflt_value, \
                   'enums', json('[]'), \
                   'comment', null \
                 )) FROM ({TABLE_IDS}) t JOIN pragma_table_xinfo(t.name) p \
                 WHERE p.hidden <> 1;",
                NULLABLE = IS_NULLABLE,
                UNIQUE = IS_UNIQUE,
            ),
        );

        queries
    }

    fn supported_export_formats(&self) -> Vec<DataFormat> {
        vec![DataFormat {
            id: "sql".into(),
            description: "SQL dump, replayable into an empty database".into(),
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
        let located = Self::resolve(params)?;
        let queries = self.schema_extraction_queries();
        let get = |key: &str| -> std::result::Result<String, ProviderError> {
            queries
                .get(key)
                .cloned()
                .ok_or_else(|| ProviderError::InvalidParams(format!("missing {key} query")))
        };

        // `rusqlite::version()` reports the linked amalgamation, so the version
        // is known even when there is no file to open.
        let Located::Existing(path) = located else {
            return Ok(render_sections(
                rusqlite::version(),
                &format!(r#"[{{"id":1,"name":"{MAIN_SCHEMA}","owner":"{MAIN_SCHEMA}"}}]"#),
                "[]",
                "[]",
                "",
            ));
        };

        let conn = Self::open_read_only(&path)?;
        let version = Self::scalar(&conn, &get("version")?)?;
        let schemas = Self::scalar(&conn, &get("schemas")?)?;
        let tables = Self::scalar(&conn, &get("tables")?)?;
        let columns = Self::scalar(&conn, &get("columns")?)?;
        let ddl = Self::scalar(&conn, DDL_QUERY)?;

        Ok(render_sections(&version, &schemas, &tables, &columns, &ddl))
    }

    /// Fold the write-ahead log back in, then hold the write lock so the files
    /// stop changing while they are copied.
    ///
    /// Two steps, and the order is load-bearing:
    ///
    /// 1. `PRAGMA wal_checkpoint(TRUNCATE)` writes committed frames back into
    ///    the main database and resets the WAL. This must run *before* the
    ///    transaction below: inside an open write transaction it fails with
    ///    "database table is locked". It is a compaction, not a correctness
    ///    step — a writer can append new frames in the window between the
    ///    checkpoint and the lock, so the snapshot may still carry a non-empty
    ///    WAL, which is fine because step 2 freezes both files together.
    /// 2. `BEGIN IMMEDIATE` takes SQLite's write lock. Other processes writing
    ///    this database — the user's application, not anything GFS controls —
    ///    block until the returned guard is dropped. Nothing is written under
    ///    the transaction; it exists only to hold the lock.
    ///
    /// Step 2 is what makes this correct: the file set stops changing for the
    /// duration of the copy.
    ///
    /// That matters on every filesystem, not only the ones that cannot clone.
    /// A copy-on-write clone is atomic per *file* — the APFS backend runs
    /// `cp -cRp`, which clones each file separately — so a WAL database, which
    /// is two or three files, is still not captured at a single instant without
    /// the lock. The window is small enough that tearing has not been
    /// reproduced on APFS in practice, but "small" is not "absent", and on a
    /// plain deep copy (`cp --reflink=auto` degrading on ext4) it is the
    /// difference between a restorable snapshot and a torn one.
    fn prepare_for_snapshot(
        &self,
        params: &ConnectionParams,
    ) -> std::result::Result<Option<Box<dyn SnapshotGuard>>, ProviderError> {
        let path = match Self::resolve(params)? {
            // Nothing has been written yet: no WAL to fold in, no writers to
            // exclude. Not an error — the first commit of a fresh repository
            // legitimately lands here.
            Located::Absent(_) => return Ok(None),
            Located::Existing(p) => p,
        };

        let conn = Self::open(&path)?;

        // A read-only database has no writer to exclude, so taking the lock
        // would prove nothing — and on some platforms it succeeds while
        // excluding nobody. Report honestly that there was nothing to quiesce
        // rather than claim a write lock we do not effectively hold.
        if conn.is_readonly("main").unwrap_or(false) {
            tracing::debug!(
                path = %path.display(),
                "database is read-only; nothing to quiesce"
            );
            return Ok(None);
        }

        // Compaction first, on its own short budget (see CHECKPOINT_TIMEOUT).
        conn.busy_timeout(CHECKPOINT_TIMEOUT)
            .map_err(|e| ProviderError::InvalidParams(format!("cannot set busy timeout: {e}")))?;
        if let Err(e) = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE") {
            tracing::debug!(error = %e, "wal checkpoint did not complete; snapshotting WAL as-is");
        }

        // The lock gets the full budget.
        conn.busy_timeout(self.lock_timeout)
            .map_err(|e| ProviderError::InvalidParams(format!("cannot set busy timeout: {e}")))?;

        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| classify_lock_failure(&path, e, self.lock_timeout))?;

        Ok(Some(Box::new(SqliteSnapshotGuard { conn })))
    }

    /// Write a SQL dump that recreates the database when replayed.
    ///
    /// Values are rendered by SQLite's own `quote()`, which is how the
    /// `sqlite3` shell builds `.dump`: it emits a correct literal for every
    /// storage class — `NULL`, integers, reals, `'escaped '' quotes'`, and
    /// blobs as `X'..'` — so the round trip does not depend on this code
    /// getting escaping right for each type.
    ///
    /// Order matters as much as escaping: tables first, then their rows, then
    /// views, triggers and indexes. A trigger installed before the rows are
    /// replayed fires for every one of them, so a restore gained rows the
    /// source never had.
    ///
    /// Generated columns are declared but never inserted; their values are
    /// recomputed on replay. Virtual tables ARE repopulated by insert, which
    /// rebuilds their index — `CREATE VIRTUAL TABLE` alone produces an empty
    /// one.
    fn export(
        &self,
        params: &ConnectionParams,
        format: &str,
        destination: &Path,
    ) -> std::result::Result<(), ProviderError> {
        if format != "sql" {
            return Err(ProviderError::UnsupportedFormat(format.to_string()));
        }
        let Located::Existing(path) = Self::resolve(params)? else {
            return Err(ProviderError::InvalidParams(
                "there is no database to export yet".to_string(),
            ));
        };
        let conn = Self::open_read_only(&path)?;

        let mut out = String::from("PRAGMA foreign_keys=OFF;\nBEGIN TRANSACTION;\n");
        let tables_ddl = Self::scalar(&conn, TABLE_DDL_QUERY)?;
        if !tables_ddl.is_empty() {
            out.push_str(&tables_ddl);
            out.push_str(";\n");
        }

        for table in Self::dumpable_tables(&conn)? {
            let cols = Self::insertable_columns(&conn, &table)?;
            if cols.is_empty() {
                continue;
            }
            let quoted: Vec<String> = cols
                .iter()
                .map(|c| format!("quote({})", ident(c)))
                .collect();
            // The whole `INSERT INTO "t" VALUES(` prefix is one SQL string
            // literal; embedding the quoted identifier on its own would end the
            // literal early and produce a syntax error.
            // Columns are named rather than positional: a table with generated
            // columns has fewer insertable columns than declared ones, and a
            // virtual table has hidden ones, so positional values would
            // misalign.
            let column_list = cols.iter().map(|c| ident(c)).collect::<Vec<_>>().join(",");
            let prefix = sql_string_literal(&format!(
                "INSERT INTO {}({}) VALUES(",
                ident(&table),
                column_list
            ));
            let sql = format!(
                "SELECT {} || {} || ');' FROM {}",
                prefix,
                quoted.join(" || ',' || "),
                ident(&table)
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| ProviderError::InvalidParams(format!("dump of '{table}': {e}")))?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|e| ProviderError::InvalidParams(format!("dump of '{table}': {e}")))?;
            for row in rows {
                out.push_str(&row.map_err(|e| {
                    ProviderError::InvalidParams(format!("dump of '{table}': {e}"))
                })?);
                out.push('\n');
            }
        }
        // Views, triggers and indexes last — see POST_DATA_DDL_QUERY.
        let post_ddl = Self::scalar(&conn, POST_DATA_DDL_QUERY)?;
        if !post_ddl.is_empty() {
            out.push_str(&post_ddl);
            out.push_str(";\n");
        }
        out.push_str("COMMIT;\n");

        std::fs::write(destination, out).map_err(|e| {
            ProviderError::InvalidParams(format!("cannot write '{}': {e}", destination.display()))
        })
    }

    /// Replay a SQL script into the database.
    fn import(
        &self,
        params: &ConnectionParams,
        format: &str,
        source: &Path,
    ) -> std::result::Result<(), ProviderError> {
        if format != "sql" {
            return Err(ProviderError::UnsupportedFormat(format.to_string()));
        }
        let script = std::fs::read_to_string(source).map_err(|e| {
            ProviderError::InvalidParams(format!("cannot read '{}': {e}", source.display()))
        })?;
        // Absent is fine: replaying a dump into a workspace with no database
        // yet is exactly how a restore starts.
        let path = Self::resolve(params)?.into_path();
        let conn = Self::open(&path)?;
        conn.execute_batch(&script)
            .map_err(|e| ProviderError::InvalidParams(format!("import failed: {e}")))
    }
}

/// How long to wait for the write lock before giving up and letting the caller
/// decide whether an unquiesced snapshot is acceptable.
const LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long to wait for the WAL checkpoint.
///
/// Deliberately short, and separate from [`LOCK_TIMEOUT`]. A `TRUNCATE`
/// checkpoint waits for readers to drain, so a single long-lived read
/// transaction — routine for an ORM connection pool — would otherwise burn the
/// entire lock budget before giving up, after which the write lock is taken
/// instantly. The checkpoint is compaction, not correctness: failing it costs a
/// larger WAL in the snapshot, nothing more.
const CHECKPOINT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Holds SQLite's write lock open for the duration of a storage snapshot.
struct SqliteSnapshotGuard {
    conn: rusqlite::Connection,
}

impl SnapshotGuard for SqliteSnapshotGuard {
    fn describe(&self) -> String {
        "sqlite write lock (BEGIN IMMEDIATE) after WAL checkpoint".to_string()
    }
}

impl Drop for SqliteSnapshotGuard {
    fn drop(&mut self) {
        // The transaction only ever held the lock, so rolling back and
        // committing are equivalent; rollback is the honest description. A
        // failure here is not actionable — closing the connection releases the
        // lock regardless.
        if let Err(e) = self.conn.execute_batch("ROLLBACK") {
            tracing::debug!(error = %e, "releasing sqlite write lock");
        }
    }
}

/// Distinguish "another process holds this database" from every other reason
/// the write lock could not be taken.
///
/// The distinction is load-bearing, not cosmetic: the caller may proceed
/// without quiescing a merely *busy* database, but must never do so when the
/// database could not be opened or read — reporting a corrupt file as "another
/// process is writing" previously invited the operator to set
/// `GFS_ALLOW_UNFROZEN_SNAPSHOT`, which recorded the corrupt file as a snapshot.
fn classify_lock_failure(
    path: &Path,
    err: rusqlite::Error,
    waited: std::time::Duration,
) -> ProviderError {
    let busy = matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if e.code == rusqlite::ErrorCode::DatabaseBusy
                || e.code == rusqlite::ErrorCode::DatabaseLocked
    );
    if busy {
        ProviderError::Busy(format!(
            "could not acquire the SQLite write lock on '{}' within {:?}: {err}",
            path.display(),
            waited
        ))
    } else {
        ProviderError::InvalidParams(format!(
            "cannot prepare '{}' for snapshot: {err}",
            path.display()
        ))
    }
}

/// Quote a SQLite identifier: wrap in double quotes, doubling any inside.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Quote a SQL string literal: wrap in single quotes, doubling any inside.
fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
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

    /// An explicit database path, plus the workspace it lives in.
    ///
    /// Both, always — the pair is the only shape `local_connection_params`
    /// produces, and the only shape the resolver accepts. Passing the override
    /// alone used to be legal and skipped the containment check entirely.
    fn params_with_path(path: &str) -> ConnectionParams {
        let parent = Path::new(path)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"));
        ConnectionParams {
            host: String::new(),
            port: 0,
            env: vec![
                (
                    LOCAL_DATA_DIR_ENV.to_string(),
                    parent.to_string_lossy().into_owned(),
                ),
                (ENV_DB_PATH.to_string(), path.to_string()),
            ],
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

    /// Every advertised capability must have a code path that RUNS — not merely
    /// exist. Export and import were once listed here while resolving to
    /// container commands that could never execute, so `gfs providers` promised
    /// what it could not do. Asserting the list alone would not have caught it,
    /// so each advertised capability is exercised.
    #[test]
    fn every_advertised_capability_actually_runs() {
        let dir = tempfile::tempdir().unwrap();
        let params = seeded_db(dir.path());
        let p = SqliteProvider::new();

        let features: Vec<String> = p.supported_features().into_iter().map(|f| f.id).collect();
        assert_eq!(features, ["schema", "export", "import"]);

        for f in &features {
            match f.as_str() {
                "schema" => {
                    p.extract_schema(&params).expect("schema is advertised");
                }
                "export" => {
                    let out = dir.path().join("adv.sql");
                    let fmt = &p.supported_export_formats()[0].id;
                    p.export(&params, fmt, &out).expect("export is advertised");
                    assert!(out.metadata().unwrap().len() > 0);
                }
                "import" => {
                    // Into a fresh workspace: replaying a dump over the tables
                    // it came from would collide, which says nothing about
                    // whether import works.
                    let fresh = tempfile::tempdir().unwrap();
                    let out = dir.path().join("adv.sql");
                    let fmt = &p.supported_import_formats()[0].id;
                    p.import(&params_for_dir(fresh.path()), fmt, &out)
                        .expect("import is advertised");
                }
                other => panic!("unexercised capability advertised: {other}"),
            }
        }
    }

    #[test]
    fn registers_as_sqlite_and_needs_no_compute() {
        let p = SqliteProvider::new();
        assert_eq!(p.name(), "sqlite");
        assert!(p.local_engine().is_some(), "sqlite runs in this process");
        assert!(
            p.container().is_none(),
            "there is no container half to fabricate a definition from"
        );
        assert!(!p.requires_compute(), "derived from container()");
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
    fn snapshot_guard_checkpoints_the_wal_and_excludes_other_writers() {
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

        let guard = LocalEngine::prepare_for_snapshot(&SqliteProvider::new(), &params)
            .unwrap()
            .expect("an existing database yields a guard");
        assert!(guard.describe().contains("write lock"));
        assert_eq!(
            std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0),
            0,
            "TRUNCATE must reset the WAL to zero length"
        );

        // While the guard is alive another connection must not be able to write.
        let contender = rusqlite::Connection::open(&db).unwrap();
        contender
            .busy_timeout(std::time::Duration::from_millis(250))
            .unwrap();
        let blocked = contender.execute_batch("BEGIN IMMEDIATE");
        assert!(
            blocked.is_err(),
            "a second writer must block while the snapshot guard is held"
        );

        // Dropping it hands the database back.
        drop(guard);
        contender
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
            .expect("write lock released with the guard");
    }

    /// A corrupt database is not lock contention. Reporting it as "another
    /// process is writing" previously invited the operator to set
    /// GFS_ALLOW_UNFROZEN_SNAPSHOT, which recorded the corrupt file as a
    /// snapshot — so the classification, not just the wording, is the fix.
    #[test]
    fn a_corrupt_database_is_not_reported_as_busy() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(DB_FILENAME);
        std::fs::write(&db, b"this is definitely not a sqlite database").unwrap();

        let err = match LocalEngine::prepare_for_snapshot(
            &SqliteProvider::new(),
            &params_with_path(db.to_str().unwrap()),
        ) {
            Ok(_) => panic!("a corrupt database must not yield a guard"),
            Err(e) => e,
        };

        assert!(
            !matches!(err, ProviderError::Busy(_)),
            "corruption must not be classified as contention, or the unfrozen \
             override would rescue it: {err}"
        );
        let msg = err.to_string();
        assert!(
            !msg.contains("Another process") && !msg.contains("another process"),
            "the message must not blame a competing writer: {msg}"
        );
    }

    /// A path that is a directory is likewise not contention.
    #[test]
    fn a_directory_where_the_database_should_be_is_not_reported_as_busy() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(DB_FILENAME);
        std::fs::create_dir(&db).unwrap();

        let err = match LocalEngine::prepare_for_snapshot(
            &SqliteProvider::new(),
            &params_with_path(db.to_str().unwrap()),
        ) {
            Ok(_) => panic!("a directory must not yield a guard"),
            Err(e) => e,
        };
        assert!(!matches!(err, ProviderError::Busy(_)), "got: {err}");
    }

    /// Genuine contention still classifies as busy, so the override keeps working
    /// for the case it was designed for.
    #[test]
    fn a_database_held_by_another_writer_is_reported_as_busy() {
        let dir = tempfile::tempdir().unwrap();
        let params = seeded_db(dir.path());
        let db = dir.path().join(DB_FILENAME);

        let holder = rusqlite::Connection::open(&db).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();

        let provider = SqliteProvider::with_lock_timeout(std::time::Duration::from_millis(200));
        let err = match LocalEngine::prepare_for_snapshot(&provider, &params) {
            Ok(_) => panic!("the lock is held elsewhere"),
            Err(e) => e,
        };
        assert!(
            matches!(err, ProviderError::Busy(_)),
            "real contention must be classified as busy: {err}"
        );
    }

    /// A read-only database has no writer to exclude, so claiming a write lock
    /// would be a false statement about what is guaranteed.
    #[test]
    fn a_read_only_database_yields_no_guard_rather_than_a_false_claim() {
        let dir = tempfile::tempdir().unwrap();
        let params = seeded_db(dir.path());
        let db = dir.path().join(DB_FILENAME);
        let mut perms = std::fs::metadata(&db).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&db, perms).unwrap();

        let guard = LocalEngine::prepare_for_snapshot(&SqliteProvider::new(), &params).unwrap();
        assert!(
            guard.is_none(),
            "a read-only database must not report a held write lock"
        );
    }

    /// The checkpoint must not consume the lock budget. One open read
    /// transaction previously cost the full 10s per commit, because TRUNCATE
    /// waits for readers to drain and the timeout was raised before it ran.
    #[test]
    fn an_open_reader_does_not_cost_the_whole_lock_budget() {
        let dir = tempfile::tempdir().unwrap();
        let params = seeded_db(dir.path());
        let db = dir.path().join(DB_FILENAME);

        let writer = rusqlite::Connection::open(&db).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer
            .execute("INSERT INTO author (name) VALUES ('ada')", [])
            .unwrap();

        let reader = rusqlite::Connection::open(&db).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        reader
            .query_row("SELECT count(*) FROM author", [], |r| r.get::<_, i64>(0))
            .unwrap();

        let started = std::time::Instant::now();
        let guard = LocalEngine::prepare_for_snapshot(&SqliteProvider::new(), &params).unwrap();
        let elapsed = started.elapsed();

        assert!(guard.is_some(), "the write lock is still free");
        assert!(
            elapsed < LOCK_TIMEOUT,
            "a blocked checkpoint must not burn the lock budget: took {elapsed:?}"
        );
    }

    #[test]
    fn checkpoint_on_a_repository_with_no_database_yet_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let params = params_with_path(dir.path().join(DB_FILENAME).to_str().unwrap());
        assert!(
            LocalEngine::prepare_for_snapshot(&SqliteProvider::new(), &params)
                .unwrap()
                .is_none(),
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

    /// Helper: the columns section, parsed.
    fn columns_of(dir: &std::path::Path) -> serde_json::Value {
        let out = SqliteProvider::new()
            .extract_schema(&params_with_path(dir.join(DB_FILENAME).to_str().unwrap()))
            .unwrap();
        let section = out
            .split("GFS_SCHEMA_COLUMNS\n")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        serde_json::from_str(&section).unwrap()
    }

    fn tables_of(dir: &std::path::Path) -> serde_json::Value {
        let out = SqliteProvider::new()
            .extract_schema(&params_with_path(dir.join(DB_FILENAME).to_str().unwrap()))
            .unwrap();
        let section = out
            .split("GFS_SCHEMA_TABLES\n")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        serde_json::from_str(&section).unwrap()
    }

    /// A PARTIAL unique index constrains only the rows matching its predicate,
    /// so duplicates are genuinely storable outside it. Ground-truthed by
    /// storing three.
    #[test]
    fn a_partial_unique_index_does_not_make_a_column_unique() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(DB_FILENAME);
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE p(id INTEGER PRIMARY KEY, email TEXT, deleted INT);
             CREATE UNIQUE INDEX ix ON p(email) WHERE deleted = 0;
             CREATE TABLE q(id INTEGER PRIMARY KEY, email TEXT UNIQUE);
             INSERT INTO p(email, deleted) VALUES('a@x',1),('a@x',1),('a@x',1);",
        )
        .unwrap();
        let stored: i64 = conn
            .query_row("SELECT count(*) FROM p WHERE email='a@x'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, 3, "SQLite really does store the duplicates");
        drop(conn);

        let cols = columns_of(dir.path());
        let cols = cols.as_array().unwrap();
        let find = |t: &str, n: &str| {
            cols.iter()
                .find(|c| c["table"] == t && c["name"] == n)
                .unwrap()
                .clone()
        };
        assert_eq!(
            find("p", "email")["is_unique"],
            false,
            "a partial index must not be treated as proof of uniqueness"
        );
        assert_eq!(
            find("q", "email")["is_unique"],
            true,
            "a total unique index still counts"
        );
    }

    /// `pragma_table_info` omits generated columns entirely, so they vanished
    /// from the metadata and `schema diff` was blind to adding one.
    #[test]
    fn generated_columns_are_reported_with_is_generated() {
        let dir = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(dir.path().join(DB_FILENAME)).unwrap();
        conn.execute_batch(
            "CREATE TABLE g(id INTEGER PRIMARY KEY, p REAL, q INT,
                 virt REAL GENERATED ALWAYS AS (p*q) VIRTUAL,
                 stor REAL GENERATED ALWAYS AS (p+q) STORED);",
        )
        .unwrap();
        drop(conn);

        let cols = columns_of(dir.path());
        let cols = cols.as_array().unwrap();
        let names: Vec<_> = cols.iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"virt"), "VIRTUAL column missing: {names:?}");
        assert!(names.contains(&"stor"), "STORED column missing: {names:?}");

        let get = |n: &str| cols.iter().find(|c| c["name"] == n).unwrap().clone();
        assert_eq!(get("virt")["is_generated"], true);
        assert_eq!(get("stor")["is_generated"], true);
        assert_eq!(get("p")["is_generated"], false, "a plain column is not");
    }

    /// An FTS5 table contributes five shadow tables that are implementation
    /// detail. The table the user created should still appear.
    #[test]
    fn fts5_shadow_tables_are_not_reported_as_user_tables() {
        let dir = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(dir.path().join(DB_FILENAME)).unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE docs USING fts5(body);
             CREATE TABLE plain(id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        drop(conn);

        let tables = tables_of(dir.path());
        let names: Vec<_> = tables
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"docs".to_string()), "got {names:?}");
        assert!(names.contains(&"plain".to_string()), "got {names:?}");
        for shadow in [
            "docs_data",
            "docs_idx",
            "docs_content",
            "docs_docsize",
            "docs_config",
        ] {
            assert!(
                !names.contains(&shadow.to_string()),
                "shadow table {shadow} leaked into the schema: {names:?}"
            );
        }
        // The virtual table's hidden columns are implementation detail too.
        let cols = columns_of(dir.path());
        let doc_cols: Vec<_> = cols
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["table"] == "docs")
            .map(|c| c["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(doc_cols, vec!["body".to_string()], "got {doc_cols:?}");
    }

    /// A composite foreign key is ONE constraint over two columns. Reported as
    /// two rows with an empty constraint_name it was indistinguishable from two
    /// independent single-column keys.
    #[test]
    fn a_composite_foreign_key_is_distinguishable_from_two_single_ones() {
        let dir = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(dir.path().join(DB_FILENAME)).unwrap();
        conn.execute_batch(
            "CREATE TABLE parent(a INT, b INT, PRIMARY KEY(a,b));
             CREATE TABLE other(id INTEGER PRIMARY KEY);
             CREATE TABLE child(x INT, y INT, z INT,
                 FOREIGN KEY(x,y) REFERENCES parent(a,b),
                 FOREIGN KEY(z) REFERENCES other(id));",
        )
        .unwrap();
        drop(conn);

        let tables = tables_of(dir.path());
        let child = tables
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "child")
            .unwrap()
            .clone();
        let rels = child["relationships"].as_array().unwrap();
        assert_eq!(rels.len(), 3, "two columns of one key plus one single key");

        let names: Vec<_> = rels
            .iter()
            .map(|r| r["constraint_name"].as_str().unwrap().to_string())
            .collect();
        assert!(
            names.iter().all(|n| !n.is_empty()),
            "every relationship must name its constraint: {names:?}"
        );

        let composite: Vec<_> = rels
            .iter()
            .filter(|r| r["target_table_name"] == "parent")
            .collect();
        assert_eq!(composite.len(), 2);
        assert_eq!(
            composite[0]["constraint_name"], composite[1]["constraint_name"],
            "both columns of a composite key share one constraint name"
        );
        let single = rels
            .iter()
            .find(|r| r["target_table_name"] == "other")
            .unwrap();
        assert_ne!(
            single["constraint_name"], composite[0]["constraint_name"],
            "a separate key must not share the composite key's name"
        );
    }

    /// Extraction must not alter what a snapshot would capture: a read-write
    /// handle checkpoints and discards the WAL when it closes.
    #[test]
    fn schema_extraction_leaves_the_database_and_its_wal_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let params = seeded_db(dir.path());
        let db = dir.path().join(DB_FILENAME);
        let wal = dir.path().join(format!("{DB_FILENAME}-wal"));

        // A live writer, so there is a real WAL to consume — held open, because
        // a clean close would checkpoint it away and there would be nothing to
        // protect.
        let writer = rusqlite::Connection::open(&db).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer
            .execute("INSERT INTO author (name) VALUES ('ada')", [])
            .unwrap();

        let read = |p: &std::path::Path| std::fs::read(p).unwrap();
        let db_before = read(&db);
        let wal_before = read(&wal);
        assert!(!wal_before.is_empty(), "the writer should have left frames");

        SqliteProvider::new().extract_schema(&params).unwrap();

        assert_eq!(
            db_before,
            read(&db),
            "the main database must be byte-identical after inspection"
        );
        assert_eq!(
            wal_before,
            read(&wal),
            "a live WAL must survive inspection — consuming it changes what the \
             next snapshot captures"
        );
    }

    /// Params pointing at a workspace directory, the way the domain supplies
    /// them — no filename, because the caller does not know one.
    fn params_for_dir(dir: &std::path::Path) -> ConnectionParams {
        ConnectionParams {
            host: String::new(),
            port: 0,
            env: vec![(
                LOCAL_DATA_DIR_ENV.to_string(),
                dir.to_string_lossy().into_owned(),
            )],
        }
    }

    /// The failure this resolver exists to remove. A Rails project keeps its
    /// database as `development.sqlite3`; assuming `db.sqlite` meant
    /// the guard took its "nothing here" path and schema capture recorded an
    /// EMPTY schema as a successful commit, with a hash, while the real
    /// database sat beside it.
    #[test]
    fn a_database_under_another_name_is_found_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("development.sqlite3");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY); INSERT INTO t VALUES(1);")
            .unwrap();
        drop(conn);

        let params = params_for_dir(dir.path());
        let provider = SqliteProvider::new();

        let out = provider.extract_schema(&params).unwrap();
        assert!(
            out.contains("\"name\":\"t\""),
            "the real schema must be captured, not an empty one:\n{out}"
        );

        let guard = LocalEngine::prepare_for_snapshot(&provider, &params).unwrap();
        assert!(
            guard.is_some(),
            "a database that exists must be quiesced, whatever it is called"
        );
    }

    /// Rails 7.1 keeps its database at `storage/development.sqlite3` — a
    /// SUBDIRECTORY. A top-level-only scan resolved that to "nothing here", so
    /// no write lock was taken, no refusal fired, and a mid-transaction copy was
    /// recorded as a commit with an empty schema attached. The doc comment cited
    /// this very layout while the code could not handle it.
    #[test]
    fn a_database_in_a_subdirectory_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("storage");
        std::fs::create_dir_all(&nested).unwrap();
        let conn = rusqlite::Connection::open(nested.join("development.sqlite3")).unwrap();
        conn.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY); INSERT INTO t VALUES(1);")
            .unwrap();
        drop(conn);

        let params = params_for_dir(dir.path());
        let provider = SqliteProvider::new();

        let out = provider.extract_schema(&params).unwrap();
        assert!(
            out.contains("\"name\":\"t\""),
            "a database one directory down must be found:\n{out}"
        );
        assert!(
            LocalEngine::prepare_for_snapshot(&provider, &params)
                .unwrap()
                .is_some(),
            "and it must be quiesced — this is the path that produced torn snapshots"
        );
    }

    /// The storage layer copies the workspace, and `cp` copies a link rather
    /// than its target, so a symlinked database would be committed as a live
    /// pointer: mutating the target changes the "snapshot", deleting it makes
    /// the snapshot unreadable.
    #[test]
    fn a_symlinked_database_is_refused_rather_than_committed_as_a_pointer() {
        let outside = tempfile::tempdir().unwrap();
        let real = outside.path().join("real.db");
        let conn = rusqlite::Connection::open(&real).unwrap();
        conn.execute_batch("CREATE TABLE s(v INT); INSERT INTO s VALUES(9);")
            .unwrap();
        drop(conn);

        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join(DB_FILENAME)).unwrap();

        let err = match SqliteProvider::new().extract_schema(&params_for_dir(dir.path())) {
            Ok(_) => panic!("a symlink must not be accepted as the database"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("symbolic link"), "{err}");
    }

    /// The workspace directory changes on every checkout while an absolute
    /// override does not, so a value left over from an earlier branch would be
    /// locked and read while a different database was snapshotted.
    #[test]
    fn an_override_outside_the_workspace_is_refused() {
        let elsewhere = tempfile::tempdir().unwrap();
        let stale = elsewhere.path().join("stale.db");
        let conn = rusqlite::Connection::open(&stale).unwrap();
        conn.execute_batch("CREATE TABLE decoy(a)").unwrap();
        drop(conn);

        let workspace = tempfile::tempdir().unwrap();
        let mut params = params_for_dir(workspace.path());
        params.env.push((
            ENV_DB_PATH.to_string(),
            stale.to_string_lossy().into_owned(),
        ));

        let err = match SqliteProvider::new().extract_schema(&params) {
            Ok(_) => panic!("an override outside the snapshotted volume must be refused"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("outside the workspace"), "{err}");

        // Inside the workspace it is still honoured.
        let inside = workspace.path().join("chosen.db");
        let conn = rusqlite::Connection::open(&inside).unwrap();
        conn.execute_batch("CREATE TABLE picked(a)").unwrap();
        drop(conn);
        let mut ok_params = params_for_dir(workspace.path());
        ok_params.env.push((
            ENV_DB_PATH.to_string(),
            inside.to_string_lossy().into_owned(),
        ));
        let out = SqliteProvider::new().extract_schema(&ok_params).unwrap();
        assert!(out.contains("picked"), "{out}");
    }

    /// A predicate that cannot exclude a row does not make the index partial in
    /// any way that matters.
    ///
    /// Established by insertion, not by reading the manual: a partial
    /// `IS NOT NULL` unique index and a plain one accept and reject exactly the
    /// same rows, because SQLite already tolerates any number of NULLs in a
    /// unique index. A predicate that CAN exclude rows must still report
    /// non-unique.
    #[test]
    fn a_partial_index_that_excludes_nothing_still_makes_the_column_unique() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(DB_FILENAME);
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE t(total TEXT, filtered TEXT, expr TEXT, pair_a TEXT, pair_b TEXT);
             CREATE UNIQUE INDEX ix_total    ON t(total)    WHERE total IS NOT NULL;
             CREATE UNIQUE INDEX ix_filtered ON t(filtered) WHERE filtered <> 'skip';
             CREATE UNIQUE INDEX ix_expr     ON t(lower(expr));
             CREATE UNIQUE INDEX ix_pair     ON t(pair_a || pair_b);",
        )
        .unwrap();

        // What SQLite actually enforces, for the two predicate forms.
        conn.execute("INSERT INTO t(total) VALUES('x')", [])
            .unwrap();
        assert!(
            conn.execute("INSERT INTO t(total) VALUES('x')", [])
                .is_err(),
            "a duplicate must be rejected despite the predicate"
        );
        conn.execute("INSERT INTO t(total) VALUES(NULL)", [])
            .unwrap();
        conn.execute("INSERT INTO t(total) VALUES(NULL)", [])
            .expect("nulls were already unconstrained, with or without the predicate");

        conn.execute("INSERT INTO t(filtered) VALUES('y')", [])
            .unwrap();
        conn.execute("INSERT INTO t(filtered) VALUES('skip')", [])
            .unwrap();
        conn.execute("INSERT INTO t(filtered) VALUES('skip')", [])
            .expect("this predicate genuinely excludes rows, so duplicates are storable");
        drop(conn);

        let out = SqliteProvider::new()
            .extract_schema(&params_for_dir(dir.path()))
            .unwrap();
        let unique_of = |column: &str| -> bool {
            let key = format!("\"name\":\"{column}\"");
            let at = out
                .find(&key)
                .unwrap_or_else(|| panic!("no {column} in {out}"));
            let tail = &out[at..];
            let end = tail.find('}').unwrap_or(tail.len());
            tail[..end].contains("\"is_unique\":true")
        };

        assert!(unique_of("total"), "IS NOT NULL excludes nothing: {out}");
        assert!(!unique_of("filtered"), "this one really is partial: {out}");
    }

    /// The recognised predicate is a closed list, and everything else is
    /// reported non-unique.
    #[test]
    fn only_an_is_not_null_predicate_counts_as_total() {
        for (sql, column, expected) in [
            (
                "CREATE UNIQUE INDEX ix ON t(a) WHERE a IS NOT NULL",
                "a",
                true,
            ),
            (
                "CREATE UNIQUE INDEX ix ON t(a) WHERE \"a\" IS NOT NULL",
                "a",
                true,
            ),
            (
                "CREATE UNIQUE INDEX ix ON t(a) WHERE [a] IS NOT NULL",
                "a",
                true,
            ),
            (
                "CREATE UNIQUE INDEX ix ON t(a) WHERE (a IS NOT NULL)",
                "a",
                true,
            ),
            (
                "create unique index ix on t(a)\n  where\n a\tis   not null",
                "a",
                true,
            ),
            (
                "CREATE UNIQUE INDEX ix ON t(a) WHERE A IS NOT NULL",
                "a",
                true,
            ),
            // A different column's predicate says nothing about this one.
            (
                "CREATE UNIQUE INDEX ix ON t(a) WHERE b IS NOT NULL",
                "a",
                false,
            ),
            ("CREATE UNIQUE INDEX ix ON t(a) WHERE a IS NULL", "a", false),
            (
                "CREATE UNIQUE INDEX ix ON t(a) WHERE deleted = 0",
                "a",
                false,
            ),
            (
                "CREATE UNIQUE INDEX ix ON t(a) WHERE a IS NOT NULL AND b > 0",
                "a",
                false,
            ),
            ("CREATE UNIQUE INDEX ix ON t(a)", "a", false),
        ] {
            assert_eq!(
                predicate_is_only_not_null(sql, column),
                expected,
                "{sql} / {column}"
            );
        }
    }

    /// There is no way in that skips the containment check.
    ///
    /// The override used to be accepted on its own, and the workspace directory
    /// was optional for exactly that shape — so a caller supplying only the
    /// override got no check at all, which is the stale-override bug with the
    /// guard absent rather than wrong. `local_connection_params` always
    /// supplies both, so this was a trap for an embedder of the public API
    /// rather than a live defect; it is now impossible either way.
    #[test]
    fn an_override_without_a_workspace_is_refused_rather_than_unchecked() {
        let elsewhere = tempfile::tempdir().unwrap();
        let stale = elsewhere.path().join("stale.db");
        let conn = rusqlite::Connection::open(&stale).unwrap();
        conn.execute_batch("CREATE TABLE decoy(a)").unwrap();
        drop(conn);

        let params = ConnectionParams {
            host: String::new(),
            port: 0,
            env: vec![(
                ENV_DB_PATH.to_string(),
                stale.to_string_lossy().into_owned(),
            )],
        };

        let err = match SqliteProvider::new().extract_schema(&params) {
            Ok(_) => panic!("an override with nothing to check it against must be refused"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains(LOCAL_DATA_DIR_ENV), "{err}");
    }

    /// Containment must hold for a database that does not exist yet.
    ///
    /// `canonicalize` fails on a path with no file behind it, and the old
    /// fallback compared the raw candidate against a resolved root. On macOS
    /// every temp directory is under `/var`, a symlink into `/private/var`, so
    /// `<workspace>/db.sqlite` — the conventional path, in the workspace, for a
    /// repository that has simply not been written to yet — resolved as
    /// OUTSIDE its own workspace and the first commit was refused.
    #[test]
    fn a_database_that_does_not_exist_yet_is_inside_its_own_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let not_yet = workspace.path().join(DB_FILENAME);
        assert!(!not_yet.exists());
        assert!(
            is_within(workspace.path(), &not_yet),
            "{} should be inside {}",
            not_yet.display(),
            workspace.path().display()
        );

        // And the provider agrees: an empty repository reports an empty schema
        // rather than an error.
        let mut params = params_for_dir(workspace.path());
        params.env.push((
            ENV_DB_PATH.to_string(),
            not_yet.to_string_lossy().into_owned(),
        ));
        let out = SqliteProvider::new().extract_schema(&params).unwrap();
        assert!(out.contains("GFS_SCHEMA_TABLES\n[]"), "{out}");
    }

    /// A traversal that no filesystem can resolve is still not contained.
    ///
    /// Neither side of this path exists, so both fall to the lexical form —
    /// which is precisely where a plain `starts_with` says yes, because
    /// `<workspace>/../elsewhere/db.sqlite` begins with `<workspace>`.
    #[test]
    fn a_parent_traversal_out_of_the_workspace_is_not_contained() {
        let workspace = tempfile::tempdir().unwrap();
        let escape = workspace
            .path()
            .join("..")
            .join("elsewhere")
            .join("db.sqlite");
        assert!(!escape.exists());
        assert!(
            !is_within(workspace.path(), &escape),
            "{} must not count as inside {}",
            escape.display(),
            workspace.path().display()
        );
    }

    /// Ambiguity must survive recursion: two databases at different depths are
    /// still two databases, and the message names them by relative path.
    #[test]
    fn two_databases_at_different_depths_are_still_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("storage")).unwrap();
        for rel in ["alpha.db", "storage/beta.sqlite3"] {
            let c = rusqlite::Connection::open(dir.path().join(rel)).unwrap();
            c.execute_batch("CREATE TABLE t(a)").unwrap();
        }
        let err = match SqliteProvider::new().extract_schema(&params_for_dir(dir.path())) {
            Ok(_) => panic!("two candidates must not resolve"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("alpha.db"), "{err}");
        assert!(
            err.contains("storage/beta.sqlite3"),
            "relative path expected: {err}"
        );
    }

    /// With nothing written yet there is genuinely nothing to find, and the
    /// first commit of a fresh repository must still succeed.
    #[test]
    fn an_empty_workspace_is_absent_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let params = params_for_dir(dir.path());
        let provider = SqliteProvider::new();

        assert!(
            LocalEngine::prepare_for_snapshot(&provider, &params)
                .unwrap()
                .is_none()
        );
        let out = provider.extract_schema(&params).unwrap();
        assert!(out.contains("GFS_SCHEMA_TABLES\n[]"));
    }

    /// Two databases and no way to know which is the one under version control.
    /// Guessing would version the wrong one silently.
    #[test]
    fn two_databases_in_a_workspace_are_reported_not_guessed() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["alpha.db", "beta.sqlite3"] {
            let c = rusqlite::Connection::open(dir.path().join(name)).unwrap();
            c.execute_batch("CREATE TABLE t(a)").unwrap();
        }

        let err = match SqliteProvider::new().extract_schema(&params_for_dir(dir.path())) {
            Ok(_) => panic!("two candidates must not resolve"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("alpha.db") && err.contains("beta.sqlite3"),
            "{err}"
        );
        assert!(
            err.contains(ENV_DB_PATH),
            "the message must say how to choose: {err}"
        );
    }

    /// Sidecar files are not databases, and must not count as candidates —
    /// otherwise a WAL database would look like three.
    #[test]
    fn wal_and_shm_sidecars_are_not_mistaken_for_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("app.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES(1);")
            .unwrap();
        // hold it open so -wal and -shm are on disk beside the database
        assert!(dir.path().join("app.db-wal").exists());

        let out = SqliteProvider::new()
            .extract_schema(&params_for_dir(dir.path()))
            .expect("one database, two sidecars — not three candidates");
        assert!(out.contains("\"name\":\"t\""), "{out}");
    }

    /// The override is reachable from the environment, not only from a caller —
    /// it was previously documented as an override while nothing but a test
    /// could set it.
    #[test]
    fn the_environment_override_selects_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let chosen = dir.path().join("chosen.db");
        let c = rusqlite::Connection::open(&chosen).unwrap();
        c.execute_batch("CREATE TABLE picked(a)").unwrap();
        drop(c);
        let other = rusqlite::Connection::open(dir.path().join("other.db")).unwrap();
        other.execute_batch("CREATE TABLE ignored(a)").unwrap();
        drop(other);

        // Ambiguous without help.
        assert!(
            SqliteProvider::new()
                .extract_schema(&params_for_dir(dir.path()))
                .is_err()
        );

        // Params carry the same key the environment would supply.
        let mut params = params_for_dir(dir.path());
        params.env.push((
            ENV_DB_PATH.to_string(),
            chosen.to_string_lossy().into_owned(),
        ));
        let out = SqliteProvider::new().extract_schema(&params).unwrap();
        assert!(out.contains("picked"), "{out}");
        assert!(!out.contains("ignored"), "{out}");
    }

    /// The only assertion that means anything for export: replay the dump into
    /// an empty database and compare what comes back. Asserting on the dump
    /// text — or on a command string, as the previous container-shaped specs
    /// did — would pass while the feature did not work.
    #[test]
    fn a_dump_round_trips_every_storage_class() {
        let src = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(src.path().join(DB_FILENAME)).unwrap();
        conn.execute_batch(
            r#"CREATE TABLE odd(
                   id INTEGER PRIMARY KEY,
                   txt TEXT,
                   num REAL,
                   blob BLOB,
                   gen INTEGER GENERATED ALWAYS AS (id * 2) STORED
               );
               CREATE TABLE "we'ird ""name"("co'l" TEXT);
               CREATE INDEX ix_odd ON odd(txt);
               CREATE VIEW v AS SELECT id FROM odd;"#,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO odd(txt, num, blob) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                "it's a \"quoted\" string\nwith a newline and Ünïcödé ✅",
                std::f64::consts::PI,
                vec![0u8, 159, 146, 150, 255]
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO odd(txt, num, blob) VALUES (NULL, NULL, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO \"we'ird \"\"name\" VALUES ('quote '' here')",
            [],
        )
        .unwrap();
        drop(conn);

        let dump = src.path().join("dump.sql");
        SqliteProvider::new()
            .export(&params_for_dir(src.path()), "sql", &dump)
            .expect("export");

        // Replay into a genuinely empty workspace.
        let dst = tempfile::tempdir().unwrap();
        SqliteProvider::new()
            .import(&params_for_dir(dst.path()), "sql", &dump)
            .expect("import");

        let a = rusqlite::Connection::open(src.path().join(DB_FILENAME)).unwrap();
        let b = rusqlite::Connection::open(dst.path().join(DB_FILENAME)).unwrap();

        /// One row of `odd`, spelled out so the comparison below is readable.
        type OddRow = (i64, Option<String>, Option<f64>, Option<Vec<u8>>, i64);

        let rows = |c: &rusqlite::Connection| -> Vec<OddRow> {
            let mut st = c
                .prepare("SELECT id, txt, num, blob, gen FROM odd ORDER BY id")
                .unwrap();
            st.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        };
        assert_eq!(
            rows(&a),
            rows(&b),
            "every value must survive the round trip"
        );

        let weird = |c: &rusqlite::Connection| -> String {
            c.query_row("SELECT \"co'l\" FROM \"we'ird \"\"name\"", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(weird(&a), weird(&b), "quoted identifiers must survive too");

        // Schema objects, not just rows.
        let objects = |c: &rusqlite::Connection| -> Vec<String> {
            let mut st = c
                .prepare("SELECT type || ':' || name FROM sqlite_master ORDER BY 1")
                .unwrap();
            st.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(
            objects(&a),
            objects(&b),
            "indexes and views must be recreated"
        );
    }

    /// A trigger installed before the rows are replayed fires for every one of
    /// them, so the restore gains rows the source never had. Real `.dump` emits
    /// triggers last for exactly this reason.
    #[test]
    fn triggers_do_not_fire_while_a_dump_is_replayed() {
        let src = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(src.path().join(DB_FILENAME)).unwrap();
        conn.execute_batch(
            "CREATE TABLE audit_src(id INTEGER PRIMARY KEY, v TEXT);
             CREATE TABLE audit_log(id INTEGER PRIMARY KEY, note TEXT);
             CREATE TRIGGER t AFTER INSERT ON audit_src
                 BEGIN INSERT INTO audit_log(note) VALUES('ins ' || NEW.v); END;
             INSERT INTO audit_src(v) VALUES('one'),('two');",
        )
        .unwrap();
        drop(conn);

        let dump = src.path().join("d.sql");
        let p = SqliteProvider::new();
        p.export(&params_for_dir(src.path()), "sql", &dump).unwrap();

        let dst = tempfile::tempdir().unwrap();
        p.import(&params_for_dir(dst.path()), "sql", &dump).unwrap();

        let b = rusqlite::Connection::open(dst.path().join(DB_FILENAME)).unwrap();
        let count = |t: &str| -> i64 {
            b.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count("audit_src"), 2);
        assert_eq!(
            count("audit_log"),
            2,
            "the trigger must not have fired during the replay"
        );

        // And it must still work afterwards.
        b.execute("INSERT INTO audit_src(v) VALUES('three')", [])
            .unwrap();
        assert_eq!(
            count("audit_log"),
            3,
            "the trigger must survive the restore"
        );
    }

    /// `CREATE VIRTUAL TABLE` builds an EMPTY index, so a dump that emits only
    /// the CREATE loses every row. Re-inserting the content rebuilds the index.
    #[test]
    fn virtual_table_content_survives_a_dump() {
        let src = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(src.path().join(DB_FILENAME)).unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE ft USING fts5(body, title);
             INSERT INTO ft(body, title) VALUES('alpha beta','one'),('gamma','two');",
        )
        .unwrap();
        drop(conn);

        let dump = src.path().join("d.sql");
        let p = SqliteProvider::new();
        p.export(&params_for_dir(src.path()), "sql", &dump).unwrap();

        let dst = tempfile::tempdir().unwrap();
        p.import(&params_for_dir(dst.path()), "sql", &dump).unwrap();

        let b = rusqlite::Connection::open(dst.path().join(DB_FILENAME)).unwrap();
        let rows: Vec<(String, String)> = b
            .prepare("SELECT body, title FROM ft ORDER BY title")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("alpha beta".to_string(), "one".to_string()),
                ("gamma".to_string(), "two".to_string())
            ]
        );

        // The index has to work, not merely the content be present.
        let hit: String = b
            .query_row("SELECT body FROM ft WHERE ft MATCH 'beta'", [], |r| {
                r.get(0)
            })
            .expect("full-text search must work on the restored table");
        assert_eq!(hit, "alpha beta");
    }

    /// A table with generated columns has fewer insertable columns than declared
    /// ones, so a positional INSERT would misalign.
    #[test]
    fn generated_columns_do_not_misalign_the_dump() {
        let src = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(src.path().join(DB_FILENAME)).unwrap();
        conn.execute_batch(
            "CREATE TABLE g(id INTEGER PRIMARY KEY, p REAL, q INT,
                 total REAL GENERATED ALWAYS AS (p*q) STORED);
             INSERT INTO g(p,q) VALUES(2.0, 3), (1.5, 4);",
        )
        .unwrap();
        drop(conn);

        let dump = src.path().join("d.sql");
        let p = SqliteProvider::new();
        p.export(&params_for_dir(src.path()), "sql", &dump).unwrap();
        let dst = tempfile::tempdir().unwrap();
        p.import(&params_for_dir(dst.path()), "sql", &dump).unwrap();

        let b = rusqlite::Connection::open(dst.path().join(DB_FILENAME)).unwrap();
        let totals: Vec<f64> = b
            .prepare("SELECT total FROM g ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(totals, vec![6.0, 6.0], "generated values recomputed");
    }

    #[test]
    fn export_rejects_a_format_it_does_not_advertise() {
        let dir = tempfile::tempdir().unwrap();
        seeded_db(dir.path());
        let out = dir.path().join("x.dump");
        assert!(matches!(
            SqliteProvider::new().export(&params_for_dir(dir.path()), "custom", &out),
            Err(ProviderError::UnsupportedFormat(_))
        ));
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
}
