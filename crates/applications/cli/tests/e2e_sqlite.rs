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
//! macOS-only, like the sibling commit/checkout suites, because commit goes
//! through the platform storage backend and these tests have only been
//! exercised against APFS.
//!
//! That gating has a real cost worth naming: the snapshot guard matters MOST on
//! Linux, where `storage-file` uses `cp --reflink=auto` and silently degrades
//! to a deep copy on ext4 — the one case where an unquiesced snapshot tears.
//! Nothing here runs there. The assertions are about logical behaviour rather
//! than APFS semantics, so they are expected to pass on Linux; enabling them,
//! together with a non-copy-on-write variant of the concurrency test, belongs
//! to the CI task rather than being flipped on unverified.

#![cfg(target_os = "macos")]

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
    let db = workspace_data_dir(repo_path).join("db.sqlite");
    let conn = rusqlite::Connection::open(db).expect("open workspace database");
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
