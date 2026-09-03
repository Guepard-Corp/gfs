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

/// A branch's working copy must not outlive the branch.
///
/// The workspace is keyed by branch NAME, and `branch -d` removed only the ref.
/// A later branch reusing the name inherited the dead branch's working copy —
/// uncommitted rows included — and checkout reported success while handing over
/// content that branch never contained. A commit taken from there records a
/// diff that never happened, and nothing in the tool can tell afterwards.
#[test]
fn a_deleted_branch_does_not_leave_its_working_copy_behind() {
    let tmp = tempdir().expect("temp dir");
    let repo = tmp.path();
    repo_with_alice(repo);

    let first = head_commit(repo);
    exec_sql(repo, "INSERT INTO users VALUES('carol');");
    assert!(commit(repo, "carol").0, "second commit");
    let second = head_commit(repo);

    // A branch at the first commit, with a commit of its own and some
    // uncommitted work on top.
    assert!(
        cli_runner::run_gfs([
            "gfs",
            "checkout",
            "--path",
            repo.to_str().unwrap(),
            "-b",
            "b1",
            &first,
        ])
        .0,
        "branch b1 at the first commit"
    );
    exec_sql(repo, "INSERT INTO users VALUES('dave');");
    assert!(commit(repo, "dave on b1").0, "commit on b1");
    exec_sql(repo, "INSERT INTO users VALUES('UNCOMMITTED');");

    assert!(
        cli_runner::run_gfs(["gfs", "checkout", "--path", repo.to_str().unwrap(), "main"]).0,
        "back to main"
    );
    assert!(
        cli_runner::run_gfs([
            "gfs",
            "branch",
            "--path",
            repo.to_str().unwrap(),
            "-d",
            "b1"
        ])
        .0,
        "delete b1"
    );
    assert!(
        !repo.join(".gfs/workspaces/b1").exists(),
        "the workspace must go with the branch"
    );

    // A new branch that happens to reuse the name, at a different commit.
    assert!(
        cli_runner::run_gfs([
            "gfs",
            "branch",
            "--path",
            repo.to_str().unwrap(),
            "b1",
            &second,
        ])
        .0,
        "recreate b1 at the second commit"
    );
    assert!(
        cli_runner::run_gfs(["gfs", "checkout", "--path", repo.to_str().unwrap(), "b1"]).0,
        "checkout the new b1"
    );
    assert_eq!(
        users(repo),
        "alice,carol",
        "the new b1 must hold its own commit's content, not the deleted branch's working copy"
    );
}

/// A branch workspace still keeps uncommitted work across a round trip.
///
/// This is what the reuse is FOR, and the fix above must not cost it.
#[test]
fn a_branch_keeps_uncommitted_work_across_a_round_trip() {
    let tmp = tempdir().expect("temp dir");
    let repo = tmp.path();
    repo_with_alice(repo);

    assert!(
        cli_runner::run_gfs([
            "gfs",
            "checkout",
            "--path",
            repo.to_str().unwrap(),
            "-b",
            "feature",
        ])
        .0,
        "create feature"
    );
    exec_sql(repo, "INSERT INTO users VALUES('wip');");

    assert!(
        cli_runner::run_gfs(["gfs", "checkout", "--path", repo.to_str().unwrap(), "main"]).0,
        "to main"
    );
    assert_eq!(users(repo), "alice", "main is unaffected by feature's work");

    assert!(
        cli_runner::run_gfs([
            "gfs",
            "checkout",
            "--path",
            repo.to_str().unwrap(),
            "feature"
        ])
        .0,
        "back to feature"
    );
    assert_eq!(users(repo), "alice,wip", "uncommitted work must survive");
}

/// A commit hash names exactly one content state.
///
/// A detached workspace is named by the commit hash, and it was reused as-is,
/// so mutating one and returning to it gave you something other than what
/// `Switched to <hash>` says you got.
#[test]
fn returning_to_a_detached_commit_gives_that_commit_not_what_was_left_there() {
    let tmp = tempdir().expect("temp dir");
    let repo = tmp.path();
    repo_with_alice(repo);

    let first = head_commit(repo);
    exec_sql(repo, "INSERT INTO users VALUES('bob');");
    assert!(commit(repo, "bob").0, "second commit");

    assert!(
        cli_runner::run_gfs(["gfs", "checkout", "--path", repo.to_str().unwrap(), &first]).0,
        "detach at the first commit"
    );
    assert_eq!(users(repo), "alice");
    exec_sql(repo, "INSERT INTO users VALUES('POISON');");

    // Re-checking-out where you already are must NOT throw away what you are
    // doing — the directory is only stale once you have left it.
    assert!(
        cli_runner::run_gfs(["gfs", "checkout", "--path", repo.to_str().unwrap(), &first]).0,
        "re-checkout the same commit"
    );
    assert_eq!(users(repo), "POISON,alice", "still where you were");

    assert!(
        cli_runner::run_gfs(["gfs", "checkout", "--path", repo.to_str().unwrap(), "main"]).0,
        "leave"
    );
    assert!(
        cli_runner::run_gfs(["gfs", "checkout", "--path", repo.to_str().unwrap(), &first]).0,
        "and come back"
    );
    assert_eq!(
        users(repo),
        "alice",
        "the commit's content, not what was left in its directory"
    );
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

/// A commit on a detached HEAD must stay reachable.
///
/// It used to write the object and update nothing — no ref, not even HEAD — so
/// the commit was unreachable the moment it was created, reported as a success
/// whose branch field was the full 64-character hash. There is no reflog.
#[test]
fn a_commit_on_a_detached_head_is_reachable_afterwards() {
    let tmp = tempdir().expect("temp dir");
    let repo = tmp.path();
    repo_with_alice(repo);

    let first = head_commit(repo);
    exec_sql(repo, "INSERT INTO users VALUES('bob');");
    assert!(commit(repo, "bob").0, "second commit");

    assert!(
        cli_runner::run_gfs(["gfs", "checkout", "--path", repo.to_str().unwrap(), &first]).0,
        "detach"
    );
    exec_sql(repo, "INSERT INTO users VALUES('detached');");
    // A subprocess, because `gag` does not reliably capture stdout under the
    // test harness and the success LINE is part of what is being asserted here.
    let (ok, stdout, stderr) = cli_runner::run_gfs_subprocess([
        "gfs",
        "commit",
        "-m",
        "on a detached head",
        "--path",
        repo.to_str().unwrap(),
    ]);
    assert!(ok, "the commit itself succeeds: {stderr}");
    assert!(
        stdout.contains("detached HEAD"),
        "the success line must name the situation, not print a 64-hex 'branch': {stdout}"
    );
    assert!(
        stderr.contains("on no branch"),
        "and say the commit is not on a branch: {stderr}"
    );

    let head = fs::read_to_string(repo.join(".gfs/HEAD")).expect("read HEAD");
    let head = head.trim();
    assert!(
        !head.starts_with("ref:") && head != first,
        "HEAD must have advanced to the new commit, not stayed at {first}: {head}"
    );
    assert!(
        repo.join(".gfs/objects")
            .join(&head[..2])
            .join(&head[2..])
            .exists(),
        "and HEAD must name a commit object that exists"
    );
}
