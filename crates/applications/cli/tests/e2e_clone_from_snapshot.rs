//! End-to-end tests for `gfs clone <source> <target> [--snapshot <id> | --branch <name>]`:
//! a new, independent database holding the data of one snapshot of another.
//!
//! The SQLite tests need no container runtime and run everywhere. The Postgres
//! test is macOS-only (commits go through the APFS storage backend, like the
//! other postgres e2e suites), needs Docker or Podman and the local
//! `gfs-postgres:17` image, and skips when the image is absent.

mod common;

use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::TempDir;

/// Runs the real binary; `true` only for exit status 0, so a refusal reads as one.
fn gfs(args: &[&str]) -> (bool, String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_gfs"))
        .args(args)
        .env("RUST_LOG", "off")
        .env("NO_COLOR", "1")
        .output()
        .expect("run gfs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The full hash for a short one, as gfs itself resolves it.
fn full_hash(repo: &Path, short: &str) -> String {
    gfs_domain::repo_utils::repo_layout::rev_parse(repo, short).expect("rev-parse")
}

fn gfs_json(args: &[&str]) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let (ok, out, err) = gfs(&full);
    assert!(ok, "gfs {args:?} failed: {err}");
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("gfs {args:?} not JSON ({e}): {out}"))
}

fn commits(repo: &Path) -> Vec<Value> {
    gfs_json(&["log", "--path", repo.to_str().unwrap()])["commits"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn head(repo: &Path) -> String {
    commits(repo)[0]["hash"].as_str().unwrap().to_string()
}

// ---------------------------------------------------------------------------
// SQLite: runs without a container runtime
// ---------------------------------------------------------------------------

fn sqlite_db(repo: &Path) -> PathBuf {
    let workspace = std::fs::read_to_string(repo.join(".gfs/WORKSPACE")).unwrap();
    let workspace = PathBuf::from(workspace.trim());
    let workspace = if workspace.is_absolute() {
        workspace
    } else {
        repo.join(workspace)
    };
    workspace.join("db.sqlite")
}

fn sqlite_exec(repo: &Path, sql: &str) {
    rusqlite::Connection::open(sqlite_db(repo))
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}

fn sqlite_values(repo: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(sqlite_db(repo)).unwrap();
    let mut stmt = conn.prepare("SELECT v FROM t ORDER BY rowid").unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// A SQLite source with two snapshots: s1 holds `before`, s2 `before` + `after`.
fn sqlite_source(root: &Path) -> (PathBuf, String, String) {
    let src = root.join("src");
    let s = src.to_str().unwrap();
    for args in [
        vec![
            "init",
            s,
            "--database-provider",
            "sqlite",
            "--database-version",
            "3",
        ],
        vec!["config", "--path", s, "user.name", "t"],
        vec!["config", "--path", s, "user.email", "t@example.com"],
    ] {
        let (ok, _, err) = gfs(&args);
        assert!(ok, "setup {args:?}: {err}");
    }
    sqlite_exec(
        &src,
        "CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('before');",
    );
    assert!(gfs(&["commit", "--path", s, "-m", "s1"]).0, "commit s1");
    let s1 = head(&src);
    sqlite_exec(&src, "INSERT INTO t VALUES ('after');");
    assert!(gfs(&["commit", "--path", s, "-m", "s2"]).0, "commit s2");
    let s2 = head(&src);
    (src, s1, s2)
}

#[test]
fn a_sqlite_clone_holds_exactly_the_chosen_snapshot() {
    let tmp = TempDir::new().unwrap();
    let (src, s1, s2) = sqlite_source(tmp.path());
    let at_s1 = tmp.path().join("at-s1");
    let latest = tmp.path().join("latest");
    let on_main = tmp.path().join("on-main");

    let (ok, _, err) = gfs(&[
        "clone",
        src.to_str().unwrap(),
        at_s1.to_str().unwrap(),
        "--snapshot",
        &s1,
    ]);
    assert!(ok, "clone at s1: {err}");
    assert_eq!(
        sqlite_values(&at_s1),
        ["before"],
        "the copy holds s1, not s2"
    );

    let history = commits(&at_s1);
    assert_eq!(
        history.len(),
        1,
        "the copy starts with one commit: {history:?}"
    );
    let message = history[0]["message"].as_str().unwrap();
    assert!(
        message.contains(src.to_str().unwrap()) && message.contains(&s1[..7]),
        "the seed commit names the source and its snapshot: {message}"
    );

    sqlite_exec(&at_s1, "INSERT INTO t VALUES ('copy-only');");
    assert_eq!(
        sqlite_values(&src),
        ["before", "after"],
        "writing to the copy leaves the source alone"
    );

    assert!(gfs(&["clone", src.to_str().unwrap(), latest.to_str().unwrap()]).0);
    assert_eq!(
        sqlite_values(&latest),
        ["before", "after"],
        "default is the latest snapshot"
    );

    let (ok, _, err) = gfs(&[
        "clone",
        src.to_str().unwrap(),
        on_main.to_str().unwrap(),
        "--branch",
        "main",
    ]);
    assert!(ok, "clone --branch main: {err}");
    assert_eq!(
        sqlite_values(&on_main),
        ["before", "after"],
        "--branch takes its tip"
    );
    assert_eq!(head(&src), s2, "cloning never moves the source");
}

#[test]
fn every_refusal_leaves_no_target_behind() {
    let tmp = TempDir::new().unwrap();
    let (src, s1, _) = sqlite_source(tmp.path());
    let s = src.to_str().unwrap();

    let not_a_repo = tmp.path().join("not-a-repo");
    std::fs::create_dir_all(&not_a_repo).unwrap();
    let empty = tmp.path().join("empty");
    let e = empty.to_str().unwrap();
    assert!(
        gfs(&[
            "init",
            e,
            "--database-provider",
            "sqlite",
            "--database-version",
            "3"
        ])
        .0
    );

    let cases: Vec<(Vec<&str>, &str, &str)> = vec![
        (
            vec![not_a_repo.to_str().unwrap()],
            "is not a gfs repository",
            "no source repository",
        ),
        (vec![e], "has no snapshots yet", "source with no commits"),
        (
            vec![s, "--snapshot", "deadbeef"],
            "not found in the source",
            "unknown snapshot",
        ),
        (
            vec![s, "--branch", "no-such-branch"],
            "not found in the source",
            "unknown branch",
        ),
    ];
    for (i, (prefix, expected, what)) in cases.iter().enumerate() {
        let target = tmp.path().join(format!("target-{i}"));
        let mut args = vec!["clone", prefix[0], target.to_str().unwrap()];
        args.extend_from_slice(&prefix[1..]);
        let (ok, _, err) = gfs(&args);
        assert!(!ok, "{what}: refused");
        assert!(
            err.contains(expected),
            "{what}: says why ({expected}): {err}"
        );
        assert!(!target.exists(), "{what}: no target left behind");
    }

    // A snapshot whose data is gone (a partial copy of the repository).
    let commit: Value = serde_json::from_str(
        &std::fs::read_to_string({
            let full = full_hash(&src, &s1);
            src.join(".gfs/objects").join(&full[..2]).join(&full[2..])
        })
        .unwrap(),
    )
    .unwrap();
    let snap = commit["snapshot_hash"].as_str().unwrap();
    let snap_dir = src.join(".gfs/snapshots").join(&snap[..2]).join(&snap[2..]);
    let parked = snap_dir.with_extension("parked");
    std::fs::rename(&snap_dir, &parked).unwrap();
    let target = tmp.path().join("target-missing-data");
    let (ok, _, err) = gfs(&["clone", s, target.to_str().unwrap(), "--snapshot", &s1]);
    std::fs::rename(&parked, &snap_dir).unwrap();
    assert!(!ok, "missing snapshot data: refused");
    assert!(err.contains("its data is missing"), "says why: {err}");
    assert!(
        !target.exists(),
        "missing snapshot data: no target left behind"
    );

    // --snapshot and --branch together is a usage error.
    let target = tmp.path().join("target-both");
    let (ok, _, err) = gfs(&[
        "clone",
        s,
        target.to_str().unwrap(),
        "--snapshot",
        &s1,
        "--branch",
        "main",
    ]);
    assert!(
        !ok && err.contains("cannot be used with"),
        "--snapshot with --branch: {err}"
    );
    assert!(!target.exists());

    // An existing repository is never overwritten.
    let (ok, _, err) = gfs(&["clone", s, e]);
    assert!(
        !ok && err.contains("is already a gfs repository"),
        "existing target: {err}"
    );
}

// ---------------------------------------------------------------------------
// Postgres: real container, custom credentials
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod postgres {
    use super::*;
    use common::container_runtime::runtime_command;
    use gfs_domain::repo_utils::repo_layout;
    use serial_test::serial;
    use std::thread;
    use std::time::Duration;

    const USER: &str = "appuser";
    const PASSWORD: &str = "app-secret";
    const DB: &str = "appdb";

    struct RemoveContainer(String);
    impl Drop for RemoveContainer {
        fn drop(&mut self) {
            let _ = runtime_command().args(["rm", "-f", &self.0]).output();
        }
    }

    fn image_present() -> bool {
        runtime_command()
            .args(["image", "inspect", "gfs-postgres:17"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// A Docker-shareable scratch directory (the default temp dir is not
    /// mountable on every runtime).
    fn shareable_tempdir() -> TempDir {
        let base = PathBuf::from(std::env::var("HOME").unwrap()).join(".gfs-test-tmp");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("gfs-clone-snap-")
            .tempdir_in(base)
            .unwrap()
    }

    fn container(repo: &Path) -> String {
        repo_layout::get_runtime_config(repo)
            .unwrap()
            .unwrap()
            .container_name
    }

    /// Query as the custom role. If the copy came up with the default
    /// credentials instead, this role would not be the one it booted with and
    /// the startup check would already have failed.
    fn psql(cid: &str, sql: &str) -> String {
        for _ in 0..60 {
            let out = runtime_command()
                .args(["exec", cid, "psql", "-U", USER, "-d", DB, "-tA", "-c", sql])
                .output()
                .expect("psql");
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout).trim().to_string();
            }
            thread::sleep(Duration::from_secs(1));
        }
        panic!("psql never succeeded on {cid}: {sql}");
    }

    #[test]
    #[serial]
    fn a_postgres_clone_holds_its_snapshot_and_stands_on_its_own() {
        if !image_present() {
            eprintln!("SKIP: gfs-postgres:17 absent");
            return;
        }
        let tmp = shareable_tempdir();
        let src = tmp.path().join("src");
        let s = src.to_str().unwrap();
        let (ok, _, err) = gfs(&[
            "init",
            s,
            "--database-provider",
            "postgres",
            "--database-version",
            "17",
            "--database-user",
            USER,
            "--database-password",
            PASSWORD,
            "--database-name",
            DB,
        ]);
        assert!(ok, "init: {err}");
        let src_cid = container(&src);
        let _src_guard = RemoveContainer(src_cid.clone());
        assert!(gfs(&["config", "--path", s, "user.name", "t"]).0);
        assert!(gfs(&["config", "--path", s, "user.email", "t@example.com"]).0);

        psql(
            &src_cid,
            "CREATE TABLE ledger (v text); INSERT INTO ledger VALUES ('before');",
        );
        let (ok, _, err) = gfs(&["commit", "--path", s, "-m", "s1"]);
        assert!(ok, "commit s1: {err}");
        let s1 = head(&src);
        psql(&src_cid, "INSERT INTO ledger VALUES ('after');");
        let (ok, _, err) = gfs(&["commit", "--path", s, "-m", "s2"]);
        assert!(ok, "commit s2: {err}");

        let copy = tmp.path().join("copy");
        let (ok, _, err) = gfs(&["clone", s, copy.to_str().unwrap(), "--snapshot", &s1]);
        assert!(ok, "clone: {err}");
        let copy_cid = container(&copy);
        let _copy_guard = RemoveContainer(copy_cid.clone());
        assert_ne!(copy_cid, src_cid, "the copy runs in its own container");

        assert_eq!(
            psql(
                &copy_cid,
                "SELECT string_agg(v, ',' ORDER BY v) FROM ledger"
            ),
            "before",
            "the copy holds s1, not s2, and starts on the source's credentials"
        );
        let history = commits(&copy);
        assert_eq!(history.len(), 1, "one commit: {history:?}");
        assert!(history[0]["message"].as_str().unwrap().contains(&s1[..7]));

        psql(&copy_cid, "INSERT INTO ledger VALUES ('copy-only');");
        assert_eq!(
            psql(
                &src_cid,
                "SELECT count(*) FROM ledger WHERE v = 'copy-only'"
            ),
            "0",
            "writing to the copy leaves the source alone"
        );

        // The copy does not depend on the source's files: move the source
        // repository away entirely, restart the copy, read it again.
        let away = tmp.path().join("src-moved-away");
        std::fs::rename(&src, &away).unwrap();
        let (ok, _, err) = gfs(&["compute", "--path", copy.to_str().unwrap(), "restart"]);
        let readback = psql(
            &copy_cid,
            "SELECT string_agg(v, ',' ORDER BY v) FROM ledger",
        );
        std::fs::rename(&away, &src).unwrap();
        assert!(ok, "restart the copy with the source gone: {err}");
        assert_eq!(
            readback, "before,copy-only",
            "the copy works with its source gone"
        );
    }
}
