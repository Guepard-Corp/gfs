//! End-to-end tests for `gfs commit`.
//!
//! Invoke the **CLI binary** via `std::process::Command` (same pattern as
//! `integration_test.rs`). We do not import the CLI crate; we only run
//! `cargo run --package gfs --bin gfs init/commit ...`
//! and assert on the resulting filesystem.
//!
//! macOS-only: commit uses the APFS storage backend. Docker must be running.
//! Tests that use a real database also require Docker with the postgres image.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use gfs_domain::model::commit::Commit;
use gfs_domain::repo_utils::repo_layout;
use tempfile::tempdir;

/// Workspace root (parent of integration_tests), for `current_dir` when running the CLI.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// Run `gfs init <path>` via Command; same pattern as `integration_test::gfs_init_creates_valid_repo_layout`.
fn gfs_init(path: &Path) -> bool {
    let status = Command::new("cargo")
        .args([
            "run",
            "--package",
            "gfs-cli",
            "--bin",
            "gfs",
            "init",
            path.to_str().unwrap(),
        ])
        .current_dir(workspace_root())
        .status()
        .expect("run gfs init");
    status.success()
}

/// Run `gfs init --database-provider postgres --database-version 17 <path>` to create a repo with a real Postgres container.
/// Returns true on success. Requires Docker running with postgres image available.
fn gfs_init_with_db(path: &Path) -> bool {
    let status = Command::new("cargo")
        .args([
            "run",
            "--package",
            "gfs-cli",
            "--bin",
            "gfs",
            "init",
            "--database-provider",
            "postgres",
            "--database-version",
            "17",
            path.to_str().unwrap(),
        ])
        .current_dir(workspace_root())
        .status()
        .expect("run gfs init --database-provider postgres --database-version 17");
    status.success()
}

/// Read the container id from `.gfs/config.toml` (runtime.container_name). Returns None if no runtime config.
fn get_container_id(repo_path: &Path) -> Option<String> {
    repo_layout::get_runtime_config(repo_path)
        .ok()
        .and_then(|opt| opt.map(|r| r.container_name))
}

/// Wait for Postgres in the container to accept connections. Retries up to 30 times with 1s delay.
/// Returns true if `psql -U postgres -c 'SELECT 1'` succeeds.
fn wait_for_postgres(container_id: &str) -> bool {
    for _ in 0..30 {
        let ok = Command::new("docker")
            .args([
                "exec",
                container_id,
                "psql",
                "-U",
                "postgres",
                "-c",
                "SELECT 1",
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return true;
        }
        thread::sleep(Duration::from_secs(1));
    }
    false
}

/// Guard that stops and removes a container on drop (success or panic).
struct ContainerCleanupGuard(String);

impl Drop for ContainerCleanupGuard {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["stop", &self.0])
            .output();
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.0])
            .output();
    }
}

/// Run `gfs log [--path <path>] [-n N]` via Command.
/// Returns (success, stdout, stderr).
fn gfs_log(repo_path: &Path, max_count: Option<usize>) -> (bool, String, String) {
    let mut args = vec![
        "run".to_string(),
        "--package".to_string(),
        "gfs-cli".to_string(),
        "--bin".to_string(),
        "gfs".to_string(),
        "log".to_string(),
        "--path".to_string(),
        repo_path.to_str().unwrap().to_string(),
    ];
    if let Some(n) = max_count {
        args.push("-n".to_string());
        args.push(n.to_string());
    }
    let out = Command::new("cargo")
        .args(&args)
        .current_dir(workspace_root())
        .output()
        .expect("run gfs log");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.success(), stdout, stderr)
}

/// Run `gfs commit -m <msg> --path <path> ...` via Command (no CLI crate import).
/// Returns (success, stdout, stderr).
fn gfs_commit(
    repo_path: &Path,
    message: &str,
    author: Option<&str>,
    author_email: Option<&str>,
) -> (bool, String, String) {
    let mut args = vec![
        "run",
        "--package",
        "gfs-cli",
        "--bin",
        "gfs",
        "commit",
        "-m",
        message,
        "--path",
        repo_path.to_str().unwrap(),
    ];
    if let Some(a) = author {
        args.push("--author");
        args.push(a);
    }
    if let Some(e) = author_email {
        args.push("--author-email");
        args.push(e);
    }
    let out = Command::new("cargo")
        .args(args)
        .current_dir(workspace_root())
        .output()
        .expect("run gfs commit");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.success(), stdout, stderr)
}

fn workspace_data_dir(repo_path: &Path) -> PathBuf {
    repo_path.join(".gfs/workspaces/main/0/data")
}

fn read_snapshot_hash(repo_path: &Path, commit_hash: &str) -> String {
    let (d, f) = commit_hash.split_at(2);
    let bytes = fs::read(repo_path.join(".gfs/objects").join(d).join(f)).unwrap();
    let commit: Commit = serde_json::from_slice(&bytes).unwrap();
    commit.snapshot_hash
}

fn snapshot_dir(repo_path: &Path, snapshot_hash: &str) -> PathBuf {
    let (prefix, rest) = snapshot_hash.split_at(2);
    repo_path.join(".gfs/snapshots").join(prefix).join(rest)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `gfs init <path>` creates `.gfs/WORKSPACE` pointing at the initial data dir.
#[test]
fn init_creates_workspace_file_pointing_to_initial_data_dir() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(gfs_init(repo_path), "gfs init should succeed");

    let workspace_file = repo_path.join(".gfs/WORKSPACE");
    assert!(workspace_file.exists(), ".gfs/WORKSPACE should be created by init");

    let recorded = fs::read_to_string(&workspace_file).unwrap();
    assert_eq!(
        recorded.trim(),
        workspace_data_dir(repo_path).to_str().unwrap(),
        "WORKSPACE should point at the initial 0/data directory"
    );
}

/// `gfs commit` creates the snapshot folder and commit object; snapshot contains
/// the workspace data that was on disk at commit time.
#[test]
fn commit_creates_snapshot_folder_with_copied_files() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(gfs_init(repo_path), "gfs init should succeed");

    let data_dir = workspace_data_dir(repo_path);
    assert!(data_dir.exists(), "workspace data dir must exist after init");
    fs::write(data_dir.join("pg_version"), "16\n").unwrap();
    fs::write(data_dir.join("schema.sql"), "CREATE TABLE test (id INT);\n").unwrap();

    let (ok, _stdout, stderr) = gfs_commit(
        repo_path,
        "first commit",
        Some("Alice"),
        Some("alice@example.com"),
    );
    assert!(ok, "gfs commit should succeed; stderr: {stderr}");

    let ref_content = fs::read_to_string(repo_path.join(".gfs/refs/heads/main")).unwrap();
    let commit_hash = ref_content.trim();
    assert_eq!(commit_hash.len(), 64);
    assert!(commit_hash.chars().all(|c| c.is_ascii_hexdigit()));

    let (obj_dir, obj_file) = commit_hash.split_at(2);
    let obj_bytes = fs::read(repo_path.join(".gfs/objects").join(obj_dir).join(obj_file)).unwrap();
    let commit: Commit = serde_json::from_slice(&obj_bytes).expect("valid JSON commit object");

    assert_eq!(commit.message, "first commit");
    assert_eq!(commit.author, "Alice");
    assert_eq!(commit.hash.as_deref(), Some(commit_hash));
    assert_eq!(commit.snapshot_hash.len(), 64);

    let snapshot_path = snapshot_dir(repo_path, &commit.snapshot_hash);
    assert!(snapshot_path.exists(), "snapshot dir should exist: {snapshot_path:?}");
    assert!(snapshot_path.is_dir());

    assert_eq!(
        fs::read_to_string(snapshot_path.join("pg_version")).unwrap(),
        "16\n"
    );
    assert_eq!(
        fs::read_to_string(snapshot_path.join("schema.sql")).unwrap(),
        "CREATE TABLE test (id INT);\n"
    );
}

/// `gfs log` displays commit history in git-style format after a commit.
#[test]
fn log_displays_commit_after_commit() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(gfs_init(repo_path), "gfs init should succeed");

    let data_dir = workspace_data_dir(repo_path);
    fs::write(data_dir.join("dummy"), "").unwrap();

    let (ok, _, stderr) = gfs_commit(
        repo_path,
        "feat: add schema",
        Some("Alice"),
        Some("alice@example.com"),
    );
    assert!(ok, "gfs commit should succeed; stderr: {stderr}");

    let (log_ok, log_stdout, log_stderr) = gfs_log(repo_path, None);
    assert!(log_ok, "gfs log should succeed; stderr: {log_stderr}");

    assert!(
        log_stdout.contains("feat: add schema"),
        "log output should contain commit message; got: {log_stdout}"
    );
    assert!(
        log_stdout.contains("Author: Alice"),
        "log output should contain author; got: {log_stdout}"
    );
    assert!(
        log_stdout.contains("(HEAD -> main"),
        "log output should contain HEAD -> main; got: {log_stdout}"
    );
}

/// `gfs log -n 1` limits output to one commit.
#[test]
fn log_respects_max_count() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(gfs_init(repo_path), "gfs init should succeed");

    let data_dir = workspace_data_dir(repo_path);
    fs::write(data_dir.join("f1"), "1").unwrap();
    let (ok1, _, _) = gfs_commit(repo_path, "first", None, None);
    assert!(ok1);

    fs::write(data_dir.join("f2"), "2").unwrap();
    let (ok2, _, _) = gfs_commit(repo_path, "second", None, None);
    assert!(ok2);

    let (log_ok, log_stdout, log_stderr) = gfs_log(repo_path, Some(1));
    assert!(log_ok, "gfs log should succeed; stderr: {log_stderr}");

    let commit_blocks = log_stdout.matches("commit ").count();
    assert_eq!(commit_blocks, 1, "log -n 1 should show exactly one commit; got: {log_stdout}");
    assert!(
        log_stdout.contains("second"),
        "log -n 1 should show most recent commit; got: {log_stdout}"
    );
}

/// Two `gfs commit` runs produce two snapshot directories with the expected
/// content; WORKSPACE still points at the initial data dir.
#[test]
fn two_commits_produce_distinct_snapshot_folders_with_files() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(gfs_init(repo_path), "gfs init should succeed");

    let data_dir = workspace_data_dir(repo_path);
    fs::write(data_dir.join("seed.txt"), "data v1").unwrap();

    let (ok1, _, stderr1) = gfs_commit(repo_path, "commit 1", None, None);
    assert!(ok1, "first gfs commit should succeed; stderr: {stderr1}");

    fs::write(data_dir.join("seed.txt"), "data v2").unwrap();

    let (ok2, _, stderr2) = gfs_commit(repo_path, "commit 2", None, None);
    assert!(ok2, "second gfs commit should succeed; stderr: {stderr2}");

    let ref_content = fs::read_to_string(repo_path.join(".gfs/refs/heads/main")).unwrap();
    let hash2 = ref_content.trim().to_string();
    let (d2, f2) = hash2.split_at(2);
    let obj2_bytes = fs::read(repo_path.join(".gfs/objects").join(d2).join(f2)).unwrap();
    let commit2: Commit = serde_json::from_slice(&obj2_bytes).unwrap();
    let hash1 = commit2.parents.as_ref().and_then(|p| p.first().cloned()).expect("second commit has parent");
    assert_ne!(hash1, hash2);

    let snap1 = read_snapshot_hash(repo_path, &hash1);
    let snap2 = read_snapshot_hash(repo_path, &hash2);
    assert_ne!(snap1, snap2);

    let snap1_path = snapshot_dir(repo_path, &snap1);
    let snap2_path = snapshot_dir(repo_path, &snap2);
    assert!(snap1_path.exists(), "first snapshot dir: {snap1_path:?}");
    assert!(snap2_path.exists(), "second snapshot dir: {snap2_path:?}");

    assert_eq!(fs::read_to_string(snap1_path.join("seed.txt")).unwrap(), "data v1");
    assert_eq!(fs::read_to_string(snap2_path.join("seed.txt")).unwrap(), "data v2");

    let workspace_recorded = fs::read_to_string(repo_path.join(".gfs/WORKSPACE")).unwrap();
    assert_eq!(
        PathBuf::from(workspace_recorded.trim()),
        workspace_data_dir(repo_path),
        "WORKSPACE must not change after commits"
    );
}

/// When config has a `mount_point` that does not exist, `gfs commit` fails with
/// an error (storage/cp failure). We set mount_point by patching config after init.
#[test]
fn commit_with_missing_mount_point_source_fails_gracefully() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(gfs_init(repo_path), "gfs init should succeed");

    // Patch config so commit will use a non-existent path as source.
    let config_path = repo_path.join(".gfs/config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("\nmount_point = \"/nonexistent/volume\"\n");
    fs::write(&config_path, config).unwrap();

    let (ok, _stdout, stderr) = gfs_commit(repo_path, "should fail", None, None);

    assert!(!ok, "gfs commit against non-existent source should fail");
    assert!(
        stderr.contains("storage") || stderr.contains("cp") || stderr.contains("failed") || stderr.contains("error"),
        "stderr should mention failure; got: {stderr}"
    );
}

/// `gfs init --database-provider postgres` starts a real Postgres container; `gfs commit` snapshots its data.
/// Requires Docker running. The container is stopped at the end of the test.
#[test]
fn commit_with_real_database_snapshots_workspace() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(
        gfs_init_with_db(repo_path),
        "gfs init --database-provider postgres should succeed (Docker must be running)"
    );

    let container_id = get_container_id(repo_path)
        .expect("runtime config with container_name should be present after init with DB");
    let _container_guard = ContainerCleanupGuard(container_id.clone());

    assert!(
        wait_for_postgres(&container_id),
        "Postgres in container {} should become ready",
        container_id
    );

    let (ok, _stdout, stderr) = gfs_commit(repo_path, "commit with real DB", None, None);
    assert!(ok, "gfs commit should succeed; stderr: {stderr}");

    let ref_content = fs::read_to_string(repo_path.join(".gfs/refs/heads/main")).unwrap();
    let commit_hash = ref_content.trim();
    assert_eq!(commit_hash.len(), 64);

    let (obj_dir, obj_file) = commit_hash.split_at(2);
    let obj_bytes = fs::read(repo_path.join(".gfs/objects").join(obj_dir).join(obj_file)).unwrap();
    let commit: Commit = serde_json::from_slice(&obj_bytes).expect("valid JSON commit object");
    let snapshot_path = snapshot_dir(repo_path, &commit.snapshot_hash);
    assert!(snapshot_path.exists(), "snapshot dir should exist: {snapshot_path:?}");

    // Snapshot should contain Postgres data dir contents (e.g. base/, global/, or postgresql.conf)
    let has_pg_data = ["base", "global", "pg_wal", "postgresql.conf"]
        .iter()
        .any(|name| snapshot_path.join(name).exists());
    assert!(
        has_pg_data,
        "snapshot should contain Postgres data; listing: {:?}",
        fs::read_dir(&snapshot_path).unwrap().collect::<Result<Vec<_>, _>>().unwrap()
    );
}
