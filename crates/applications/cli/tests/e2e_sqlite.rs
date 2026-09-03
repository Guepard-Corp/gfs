//! End-to-end tests for the file-based SQLite provider.
//!
//! The point of these tests is that none of them start a container. Every one
//! runs with `DOCKER_HOST` pointed at a socket that does not exist, so a test
//! passing here proves the command genuinely reached the database without a
//! runtime, rather than quietly finding a daemon that happened to be running on
//! the developer's machine.
//!
//! Runs the CLI in-process via `gfs_cli::run()` so coverage is captured.
//!
//! Unix-only, not macOS-only, and the difference is the point. The snapshot
//! guard matters MOST where the copy is NOT atomic: on Linux `storage-file`
//! uses `cp --reflink=auto`, which silently degrades to a deep copy on ext4,
//! and a deep copy of a database being written to is exactly where an
//! unquiesced snapshot tears. GitHub's ubuntu runner is ext4, so running here
//! is the non-copy-on-write variant — no flag or special filesystem needed.
//!
//! Windows is excluded because one test shells out to `chmod`; nothing else
//! here is platform-specific.

#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::cli_runner;
use tempfile::tempdir;

/// Point the container runtime at a socket that cannot exist.
///
/// Any command that still tries to reach Docker fails loudly instead of
/// silently succeeding against a daemon the developer happens to be running.
fn forbid_container_runtime() {
    // SAFETY: integration tests in this binary run single-threaded with respect
    // to this variable — it is set once, before any command runs, and never read
    // concurrently with a write.
    unsafe {
        std::env::set_var("DOCKER_HOST", "unix:///nonexistent/gfs-test-docker.sock");
    }
}

fn workspace_data_dir(repo_path: &Path) -> PathBuf {
    let s = fs::read_to_string(repo_path.join(".gfs/WORKSPACE")).expect("read WORKSPACE");
    PathBuf::from(s.trim())
}

fn init_sqlite(repo_path: &Path) {
    forbid_container_runtime();
    let (ok, _, stderr) = cli_runner::run_gfs([
        "gfs",
        "init",
        repo_path.to_str().unwrap(),
        "--database-provider",
        "sqlite",
        "--database-version",
        "3",
    ]);
    assert!(
        ok,
        "init with provider sqlite should not need a container runtime; stderr: {stderr}"
    );
}

fn query(repo_path: &Path, sql: &str) -> (bool, String, String) {
    cli_runner::run_gfs(["gfs", "query", "--path", repo_path.to_str().unwrap(), sql])
}

fn commit(repo_path: &Path, message: &str) -> (bool, String, String) {
    cli_runner::run_gfs([
        "gfs",
        "commit",
        "-m",
        message,
        "--path",
        repo_path.to_str().unwrap(),
    ])
}

fn head_commit(repo_path: &Path) -> String {
    fs::read_to_string(repo_path.join(".gfs/refs/heads/main"))
        .expect("read main ref")
        .trim()
        .to_string()
}

fn row_count(repo_path: &Path, table: &str) -> i64 {
    let db = workspace_data_dir(repo_path).join("db.sqlite");
    let conn = rusqlite::Connection::open(db).expect("open workspace database");
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .expect("count rows")
}

fn exec_sql(repo_path: &Path, sql: &str) {
    exec_sql_at(&workspace_data_dir(repo_path).join("db.sqlite"), sql);
}

/// Same, against a database at a path of the caller's choosing.
fn exec_sql_at(db: &Path, sql: &str) {
    let conn = rusqlite::Connection::open(db).expect("open database");
    conn.execute_batch(sql).expect("apply sql");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn init_writes_environment_config_and_no_runtime_section() {
    let tmp = tempdir().expect("temp dir");
    let repo = tmp.path();
    init_sqlite(repo);

    let config = fs::read_to_string(repo.join(".gfs/config.toml")).expect("read config");
    assert!(config.contains("database_provider = \"sqlite\""));
    assert!(
        !config.contains("[runtime]"),
        "an embedded provider must leave RuntimeConfig absent — that is the signal \
         every downstream guard reads as 'no container to manage':\n{config}"
    );
    assert!(
        workspace_data_dir(repo).exists(),
        "the workspace data directory must exist so the first write has somewhere to land"
    );
}

#[test]
fn query_runs_against_the_workspace_database() {
    let tmp = tempdir().expect("temp dir");
    let repo = tmp.path();
    init_sqlite(repo);

    let (ok, _, stderr) = query(repo, "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);");
    assert!(
        ok,
        "query should not need a container runtime; stderr: {stderr}"
    );

    let (ok, _, stderr) = query(repo, "INSERT INTO t(v) VALUES('a'),('b');");
    assert!(ok, "insert failed: {stderr}");
    assert_eq!(row_count(repo, "t"), 2);
}

#[test]
fn commit_captures_schema_from_the_linked_engine() {
    let tmp = tempdir().expect("temp dir");
    let repo = tmp.path();
    init_sqlite(repo);
    exec_sql(
        repo,
        "CREATE TABLE author(id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT UNIQUE);
         CREATE TABLE book(id INTEGER PRIMARY KEY, author_id INTEGER REFERENCES author(id));",
    );

    let (ok, _, stderr) = commit(repo, "initial schema");
    assert!(ok, "commit failed: {stderr}");

    // Verified through a subprocess: `gag` does not reliably capture stdout
    // under the test harness, and the assertions below are about stdout content.
    let (ok, stdout, stderr) = cli_runner::run_gfs_subprocess([
        "gfs",
        "schema",
        "show",
        &head_commit(repo),
        "--path",
        repo.to_str().unwrap(),
    ]);
    assert!(ok, "schema show failed: {stderr}");

    // The recorded version comes from the bundled amalgamation, not a host
    // binary, so it is identical on every machine.
    assert!(
        stdout.contains(rusqlite::version()),
        "expected the linked engine version {}; got:\n{stdout}",
        rusqlite::version()
    );
    assert!(
        stdout.contains("\"name\": \"author\""),
        "missing author table:\n{stdout}"
    );
    assert!(
        stdout.contains("\"name\": \"book\""),
        "missing book table:\n{stdout}"
    );
    assert!(
        stdout.contains("\"target_table_name\": \"author\""),
        "the foreign key from book to author should be captured:\n{stdout}"
    );
}

#[test]
fn commit_before_any_write_succeeds() {
    let tmp = tempdir().expect("temp dir");
    let repo = tmp.path();
    init_sqlite(repo);

    // No database file exists yet. Quiescing and schema extraction must both
    // treat that as "nothing to do" rather than as a failure.
    let (ok, _, stderr) = commit(repo, "empty");
    assert!(
        ok,
        "the first commit of a fresh repository must succeed: {stderr}"
    );
}

#[test]
fn branches_isolate_writes_and_checkout_restores_them() {
    let tmp = tempdir().expect("temp dir");
    let repo = tmp.path();
    init_sqlite(repo);
    exec_sql(
        repo,
        "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t(v) VALUES('a');",
    );
    assert!(commit(repo, "one row").0, "base commit");

    let (ok, _, stderr) = cli_runner::run_gfs([
        "gfs",
        "checkout",
        "-b",
        "feature",
        "--path",
        repo.to_str().unwrap(),
    ]);
    assert!(
        ok,
        "checkout -b should not need a container runtime: {stderr}"
    );

    exec_sql(repo, "INSERT INTO t(v) VALUES('b');");
    assert!(commit(repo, "second row").0, "branch commit");
    assert_eq!(row_count(repo, "t"), 2, "feature has both rows");

    let (ok, _, stderr) =
        cli_runner::run_gfs(["gfs", "checkout", "main", "--path", repo.to_str().unwrap()]);
    assert!(ok, "checkout main failed: {stderr}");
    assert_eq!(
        row_count(repo, "t"),
        1,
        "main must not see the write made on feature"
    );

    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "checkout",
        "feature",
        "--path",
        repo.to_str().unwrap(),
    ]);
    assert!(ok);
    assert_eq!(
        row_count(repo, "t"),
        2,
        "switching back restores the branch state"
    );
}

#[test]
fn snapshot_is_consistent_and_matches_the_committed_state() {
    let tmp = tempdir().expect("temp dir");
    let repo = tmp.path();
    init_sqlite(repo);
    exec_sql(
        repo,
        "PRAGMA journal_mode=WAL;
         CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);",
    );
    for _ in 0..500 {
        exec_sql(repo, "INSERT INTO t(v) VALUES('x');");
    }
    let expected = row_count(repo, "t");
    assert!(commit(repo, "many rows").0, "commit");

    // Resolve the snapshot through the commit object. Snapshot directories are
    // made read-only at creation and share an mtime, so ordering them by time
    // does not work.
    let head = head_commit(repo);
    let obj = repo.join(".gfs/objects").join(&head[..2]).join(&head[2..]);
    let commit_obj: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(obj).expect("read commit object"))
            .expect("parse commit object");
    let snap = commit_obj["snapshot_hash"].as_str().expect("snapshot_hash");
    let snap_dir = repo
        .join(".gfs/snapshots")
        .join(&snap[..2])
        .join(&snap[2..]);

    // Copy out before opening: the snapshot tree is read-only, and opening a
    // WAL database in place would try to create shared-memory files in it.
    let out = tmp.path().join("restored");
    fs::create_dir_all(&out).unwrap();
    for entry in fs::read_dir(&snap_dir).expect("read snapshot dir") {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("db.sqlite") {
            fs::copy(entry.path(), out.join(name)).expect("copy snapshot file");
        }
    }

    let conn = rusqlite::Connection::open(out.join("db.sqlite")).expect("open snapshot");
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .expect("integrity check");
    assert_eq!(integrity, "ok", "the snapshot must be a valid database");
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .expect("count");
    assert_eq!(
        rows, expected,
        "the snapshot must hold exactly the state that was committed"
    );
}

#[test]
fn a_container_backed_provider_still_reports_the_runtime_failure() {
    let tmp = tempdir().expect("temp dir");
    let repo = tmp.path();
    forbid_container_runtime();

    let (ok, _, stderr) = cli_runner::run_gfs([
        "gfs",
        "init",
        repo.to_str().unwrap(),
        "--database-provider",
        "postgres",
        "--database-version",
        "17",
    ]);
    assert!(
        !ok,
        "postgres needs a container; it must not silently succeed without one"
    );
    assert!(
        stderr.contains("Docker") || stderr.contains("Podman"),
        "the failure should still point at the container runtime, not something obscure: {stderr}"
    );
}

/// Neither command may point at the other.
///
/// `gfs user` on a SQLite repo used to answer "no container configured (run
/// gfs compute start)", and `gfs compute start` then answered "no
/// container_name in repo config" — a closed loop of two commands, neither
/// able to succeed, and the second an invitation to hand-edit the config into
/// a state the repo cannot support.
#[test]
fn user_and_compute_explain_themselves_instead_of_advising_each_other() {
    let tmp = tempdir().expect("temp dir");
    let repo = tmp.path();
    init_sqlite(repo);

    for args in [
        vec!["gfs", "user", "list", "--path", repo.to_str().unwrap()],
        vec![
            "gfs",
            "user",
            "create",
            "bob",
            "--path",
            repo.to_str().unwrap(),
        ],
    ] {
        let (ok, _, stderr) = cli_runner::run_gfs(args.clone());
        assert!(!ok, "user management cannot work here: {args:?}");
        assert!(
            stderr.contains("embedded database") && stderr.contains("no roles"),
            "should explain why there are no users: {stderr}"
        );
        assert!(
            !stderr.contains("gfs compute start"),
            "must not advise a command that cannot succeed: {stderr}"
        );
    }

    for action in ["start", "status", "stop"] {
        let (ok, _, stderr) =
            cli_runner::run_gfs(["gfs", "compute", "--path", repo.to_str().unwrap(), action]);
        assert!(!ok, "there is no container to {action}");
        assert!(
            stderr.contains("embedded database") && stderr.contains("no container"),
            "should say there is nothing to run: {stderr}"
        );
        assert!(
            !stderr.contains("container_name"),
            "must not invite editing the config: {stderr}"
        );
    }
}

/// A wrong provider name must not send the user to debug Docker.
///
/// The name was validated only after the container client had been built, so
/// `--database-provider sqlite3` — a plausible typo, since the binary is called
/// `sqlite3` — reported "GFS was not able to connect to Docker/Podman" and a
/// list of daemon troubleshooting steps for a database that needs no daemon.
#[test]
fn a_mistyped_provider_name_names_the_provider_not_the_daemon() {
    let tmp = tempdir().expect("temp dir");
    forbid_container_runtime();

    let (ok, _, stderr) = cli_runner::run_gfs([
        "gfs",
        "init",
        tmp.path().to_str().unwrap(),
        "--database-provider",
        "sqlite3",
        "--database-version",
        "3",
    ]);
    assert!(!ok, "'sqlite3' is not a provider name");
    assert!(
        stderr.contains("unknown database provider") && stderr.contains("sqlite"),
        "should name the mistake and list the real names: {stderr}"
    );
    assert!(
        !stderr.contains("Docker") && !stderr.contains("Podman"),
        "no container runtime is involved: {stderr}"
    );
}

/// The recorded version has to be one the provider actually supports.
///
/// It is written to `.gfs/config.toml` permanently, and for an embedded
/// provider it describes an engine that is linked in rather than chosen — so
/// `--database-version 4` was accepted and then contradicted by every later
/// report of the real version.
#[test]
fn an_unsupported_version_is_refused_rather_than_recorded() {
    let tmp = tempdir().expect("temp dir");
    forbid_container_runtime();

    let (ok, _, stderr) = cli_runner::run_gfs([
        "gfs",
        "init",
        tmp.path().to_str().unwrap(),
        "--database-provider",
        "sqlite",
        "--database-version",
        "4",
    ]);
    assert!(!ok, "there is no SQLite 4");
    assert!(
        stderr.contains("not a supported") && stderr.contains("Supported: 3"),
        "should say what is supported: {stderr}"
    );
    assert!(
        !tmp.path().join(".gfs/config.toml").exists(),
        "nothing should be written for a repo that was refused"
    );
}

/// The provider's message names the actual problem; two commands replaced it
/// with four words.
///
/// `gfs commit` and `gfs export` reported "the workspace holds 2 SQLite
/// databases (a.db, b.db) … set GFS_SQLITE_DB_PATH to choose", which tells the
/// user exactly what to do. `gfs schema extract` said "schema extraction
/// failed" and `gfs query` said "failed to build query command" — anyhow's
/// context Display prints only the context string.
#[test]
fn an_ambiguous_workspace_is_explained_by_every_command_that_hits_it() {
    let tmp = tempdir().expect("temp dir");
    let repo = tmp.path();
    init_sqlite(repo);

    let data = workspace_data_dir(repo);
    for name in ["a.db", "b.db"] {
        exec_sql_at(&data.join(name), "CREATE TABLE t(x)");
    }

    for args in [
        vec!["gfs", "commit", "-m", "x", "--path", repo.to_str().unwrap()],
        vec!["gfs", "schema", "extract", "--path", repo.to_str().unwrap()],
        vec!["gfs", "query", "--path", repo.to_str().unwrap(), "SELECT 1"],
    ] {
        let (ok, _, stderr) = cli_runner::run_gfs(args.clone());
        assert!(!ok, "an ambiguous workspace cannot be resolved: {args:?}");
        assert!(
            stderr.contains("2 SQLite databases") && stderr.contains("GFS_SQLITE_DB_PATH"),
            "every command should carry the actionable message, {args:?} gave: {stderr}"
        );
    }
}

// ---------------------------------------------------------------------------
// Core checkout/commit behaviour (TASK-8.17)
//
// These are not SQLite-specific — they live in the shared checkout and commit
// path and hold for every provider. They are asserted here because SQLite is
// the only provider that runs without a container, so this is the only suite
// that can exercise them without a daemon.
// ---------------------------------------------------------------------------

/// Rows in `users`, sorted, as one string.
fn users(repo_path: &Path) -> String {
    let db = workspace_data_dir(repo_path).join("db.sqlite");
    let conn = rusqlite::Connection::open(db).expect("open workspace database");
    let mut names: Vec<String> = conn
        .prepare("SELECT name FROM users")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect");
    names.sort();
    names.join(",")
}

/// A repo on `main` with one commit holding `alice`.
fn repo_with_alice(repo: &Path) {
    init_sqlite(repo);
    exec_sql(
        repo,
        "CREATE TABLE users(name TEXT); INSERT INTO users VALUES('alice');",
    );
    assert!(commit(repo, "alice").0, "first commit");
}

/// A missing snapshot must fail, not seed an empty database.
///
/// The restore treated "this commit records no snapshot" and "this commit's
/// snapshot is gone" as the same thing. `schema show` still printed the real
/// DDL, so only a query noticed, and a commit taken from that state recorded
/// the emptiness as a legitimate breaking change.
#[test]
fn a_checkout_whose_snapshot_is_missing_fails_instead_of_emptying_the_database() {
    let tmp = tempdir().expect("temp dir");
    let repo = tmp.path();
    repo_with_alice(repo);

    let first = head_commit(repo);
    exec_sql(repo, "INSERT INTO users VALUES('bob');");
    assert!(commit(repo, "bob").0, "second commit");

    // Snapshots are the bulky part of a repo and the first thing a partial copy
    // leaves out.
    let snapshots = repo.join(".gfs/snapshots");
    for entry in fs::read_dir(&snapshots).expect("read snapshots") {
        let path = entry.expect("entry").path();
        let _ = std::process::Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(&path)
            .output();
        fs::remove_dir_all(&path).expect("remove snapshot tree");
    }

    let (ok, _, stderr) =
        cli_runner::run_gfs(["gfs", "checkout", "--path", repo.to_str().unwrap(), &first]);
    assert!(
        !ok,
        "a checkout that cannot restore must not report success"
    );
    assert!(
        stderr.contains("does not exist") && stderr.contains("empty database"),
        "should name the missing snapshot and say what it would have caused: {stderr}"
    );
    assert_eq!(
        users(repo),
        "alice,bob",
        "the workspace it refused to leave must be untouched"
    );
}

// ---------------------------------------------------------------------------
// The snapshot guard, under a writer that does not stop
// ---------------------------------------------------------------------------

/// Every commit must capture a state the database genuinely passed through.
///
/// This is the assertion the whole embedded design rests on, and the one that
/// could not be made anywhere but here: a container provider quiesces by
/// pausing its instance, but the process writing a SQLite file is the user's
/// own application, which nothing GFS controls can freeze. What replaces it is
/// SQLite's own write lock, taken before the snapshot and held across it.
///
/// Three properties are checked per commit, and the third is the one that
/// matters:
///
/// 1. `PRAGMA integrity_check` passes — the file is structurally sound;
/// 2. the row count lies between the counts observed immediately before and
///    after the commit — a state the database really was in;
/// 3. every transaction in the snapshot is WHOLE.
///
/// (3) is not implied by (1) or (2). A torn transaction leaves a database that
/// is structurally valid, so `integrity_check` calls it ok, and whose row count
/// can still land inside the window. Each transaction here writes `GROUP` rows
/// stamped with one increasing batch number, so a snapshot is whole only when
/// every batch it holds is complete and the batches run without a gap.
///
/// Commits run as a SUBPROCESS so the contention is genuinely between
/// processes — SQLite's file locking is what is under test, not a mutex.
///
/// A commit that REFUSES is not a failure: declining to snapshot a database it
/// could not quiesce is the correct outcome, and the branch's whole thesis. But
/// a run that is entirely refusals proves nothing, so at least one must succeed.
///
/// WHAT THIS DOES NOT ESTABLISH, measured rather than assumed. At this size —
/// 60k rows of 256 bytes, about 15 MB — it PASSES with the snapshot guard
/// removed, on APFS and on a non-copy-on-write HFS+ volume alike. The copy
/// finishes in milliseconds either way, so there is no window to tear in. It is
/// a regression test for the invariants, not a demonstration that the guard is
/// necessary.
///
/// The demonstration lives in `scripts/sqlite-snapshot-torture.py`, which runs
/// unbounded against a database large enough for the copy to take real time;
/// there, removing the guard produces `database disk image is malformed` and
/// snapshots of states the database was never in. Growing this test until it
/// could show the same thing would cost minutes and gigabytes on every CI run,
/// which is the wrong trade for a suite that runs on every push.
///
/// So read a pass here as "the invariants still hold", and the script as "and
/// here is why the guard has to be there".
#[test]
fn commits_under_a_concurrent_writer_capture_only_whole_transactions() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    /// Rows per transaction. Small enough to keep the run quick, large enough
    /// that a torn write lands mid-batch rather than exactly on a boundary.
    const GROUP: i64 = 50;
    const PAYLOAD: usize = 256;
    /// Row budget, so this stays a test rather than a disk-space hazard.
    const MAX_ROWS: i64 = 60_000;
    const ROUNDS: usize = 6;

    let tmp = tempdir().expect("temp dir");
    let repo = tmp.path();
    init_sqlite(repo);
    exec_sql(
        repo,
        "PRAGMA journal_mode=WAL;
         CREATE TABLE ledger(id INTEGER PRIMARY KEY, batch INTEGER NOT NULL, payload TEXT NOT NULL);
         CREATE INDEX ledger_batch ON ledger(batch);",
    );

    let db = workspace_data_dir(repo).join("db.sqlite");
    let stop = Arc::new(AtomicBool::new(false));
    let written = Arc::new(AtomicU64::new(0));

    let writer = {
        let (db, stop, written) = (db.clone(), stop.clone(), written.clone());
        std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(&db).expect("writer connection");
            conn.busy_timeout(std::time::Duration::from_secs(30))
                .expect("busy timeout");
            let pad = "y".repeat(PAYLOAD);
            let mut batch = 0i64;
            while !stop.load(Ordering::Relaxed) && (batch * GROUP) < MAX_ROWS {
                let next = batch + 1;
                // One explicit transaction per batch, built as a single script:
                // the whole point is that either all GROUP rows are in the
                // snapshot or none of them are.
                let mut script = String::from("BEGIN IMMEDIATE;");
                for _ in 0..GROUP {
                    script.push_str(&format!(
                        "INSERT INTO ledger(batch, payload) VALUES({next},'{pad}');"
                    ));
                }
                script.push_str("COMMIT;");
                match conn.execute_batch(&script) {
                    Ok(()) => {
                        batch = next;
                        written.fetch_add(GROUP as u64, Ordering::Relaxed);
                    }
                    Err(_) => {
                        // The committer holds the write lock. Roll back the
                        // half-open transaction before retrying, or the next
                        // BEGIN fails for a different reason and the writer
                        // silently stops writing anything at all.
                        let _ = conn.execute_batch("ROLLBACK");
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
            }
        })
    };

    // Let the writer get ahead so every commit lands mid-stream.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let count_rows = |path: &Path| -> i64 {
        let conn = rusqlite::Connection::open(path).expect("open");
        conn.busy_timeout(std::time::Duration::from_secs(30))
            .expect("busy timeout");
        conn.query_row("SELECT count(*) FROM ledger", [], |r| r.get(0))
            .expect("count")
    };

    let mut passed = 0usize;
    let mut refused = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for round in 0..ROUNDS {
        let before = count_rows(&db);
        let (ok, _, stderr) = cli_runner::run_gfs_subprocess([
            "gfs",
            "commit",
            "-m",
            &format!("round {round}"),
            "--path",
            repo.to_str().unwrap(),
        ]);
        let after = count_rows(&db);

        if !ok || stderr.contains("Refusing to commit") {
            refused += 1;
            continue;
        }

        // Resolve the snapshot through the commit object, not by listing
        // directories: snapshot trees are made read-only at creation and share
        // an mtime, so sorting them by time picks an arbitrary one.
        let head = fs::read_to_string(repo.join(".gfs/refs/heads/main"))
            .expect("read main ref")
            .trim()
            .to_string();
        let commit = gfs_domain::repo_utils::repo_layout::get_commit_from_hash(repo, &head)
            .expect("commit object");
        let snapshot = repo
            .join(".gfs/snapshots")
            .join(&commit.snapshot_hash[..2])
            .join(&commit.snapshot_hash[2..]);

        // Copy it out: the tree is read-only, and reading a WAL database needs
        // to create shared memory beside it.
        let scratch = tempdir().expect("scratch");
        let copied = scratch.path().join("snap");
        assert!(
            std::process::Command::new("cp")
                .args(["-R"])
                .arg(&snapshot)
                .arg(&copied)
                .status()
                .expect("cp")
                .success(),
            "copy the snapshot out"
        );
        let _ = std::process::Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(&copied)
            .output();

        let conn = rusqlite::Connection::open(copied.join("db.sqlite")).expect("open snapshot");
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .expect("integrity_check");
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM ledger", [], |r| r.get(0))
            .expect("count");
        let short_batch: Option<(i64, i64)> = conn
            .query_row(
                "SELECT batch, count(*) FROM ledger GROUP BY batch HAVING count(*) <> ?1 LIMIT 1",
                [GROUP],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let (distinct, highest): (i64, Option<i64>) = conn
            .query_row(
                "SELECT count(DISTINCT batch), max(batch) FROM ledger",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("batch summary");

        if integrity != "ok" {
            failures.push(format!("round {round}: integrity_check said {integrity}"));
        } else if let Some((batch, n)) = short_batch {
            failures.push(format!(
                "round {round}: torn transaction — batch {batch} has {n} of {GROUP} rows"
            ));
        } else if highest.is_some_and(|h| distinct != h) {
            failures.push(format!(
                "round {round}: batch gap — {distinct} batches present but highest is {}",
                highest.unwrap()
            ));
        } else if !(before..=after).contains(&rows) {
            failures.push(format!(
                "round {round}: snapshot holds {rows} rows, outside [{before}, {after}] — \
                 a state the database was never in"
            ));
        } else {
            passed += 1;
        }
    }

    stop.store(true, Ordering::Relaxed);
    writer.join().expect("writer thread");

    assert!(
        failures.is_empty(),
        "the snapshot guard did not hold ({} written): {}",
        written.load(Ordering::Relaxed),
        failures.join("; ")
    );
    assert!(
        passed > 0,
        "every one of {ROUNDS} commits refused, so nothing was actually verified \
         ({refused} refused)"
    );
}

/// `gfs status` reports the commit HEAD points at.
///
/// It did not, in any output mode, while the MCP `status` tool did — because
/// that tool injected a `head_commit` into its own payload rather than reading
/// a shared field. The two surfaces answered the same question differently and
/// the skill files documented both answers. The field now lives on
/// `StatusResponse`, so neither can drift.
#[test]
fn status_reports_the_head_commit_like_the_mcp_tool_does() {
    let tmp = tempdir().expect("temp dir");
    let repo = tmp.path();
    init_sqlite(repo);

    // Before the first commit there is no HEAD to report, and the sentinel "0"
    // must not be dressed up as one.
    let (ok, stdout, stderr) = cli_runner::run_gfs_subprocess([
        "gfs",
        "status",
        "--path",
        repo.to_str().unwrap(),
        "--output",
        "json",
    ]);
    assert!(ok, "status before any commit: {stderr}");
    let before: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert!(
        before.get("head_commit").is_none(),
        "no commits yet, so no HEAD: {before}"
    );

    exec_sql(repo, "CREATE TABLE t(a INTEGER PRIMARY KEY);");
    assert!(commit(repo, "one").0, "commit");
    let expected = head_commit(repo);

    let (ok, stdout, stderr) = cli_runner::run_gfs_subprocess([
        "gfs",
        "status",
        "--path",
        repo.to_str().unwrap(),
        "--output",
        "json",
    ]);
    assert!(ok, "status after a commit: {stderr}");
    let after: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(
        after.get("head_commit").and_then(|v| v.as_str()),
        Some(expected.as_str()),
        "status must report the commit HEAD points at: {after}"
    );

    // And the styled output shows it too, not just the machine-readable form.
    let (ok, stdout, _) =
        cli_runner::run_gfs_subprocess(["gfs", "status", "--path", repo.to_str().unwrap()]);
    assert!(ok);
    assert!(
        stdout.contains(&expected[..7]),
        "the styled output should show the short HEAD: {stdout}"
    );
}
