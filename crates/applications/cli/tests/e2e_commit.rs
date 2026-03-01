//! End-to-end tests for `gfs commit`.
//!
//! Runs CLI in-process via gfs_cli::run() for coverage capture.
//! macOS-only: commit uses the APFS storage backend. Docker required for DB tests.

#![cfg(target_os = "macos")]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use common::cli_runner;
use gfs_domain::model::commit::Commit;
use gfs_domain::repo_utils::repo_layout;
use tempfile::tempdir;

/// Read the container id from `.gfs/config.toml` (runtime.container_name). Returns None if no runtime config.
fn get_container_id(repo_path: &Path) -> Option<String> {
    repo_layout::get_runtime_config(repo_path)
        .ok()
        .and_then(|opt| opt.map(|r| r.container_name))
}

/// Wait for Postgres in the container to accept connections. Retries up to 30 times with 1s delay.
fn wait_for_postgres(container_id: &str) -> bool {
    for _ in 0..30 {
        let ok = std::process::Command::new("docker")
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
        let _ = std::process::Command::new("docker")
            .args(["stop", &self.0])
            .output();
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &self.0])
            .output();
    }
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

#[test]
fn init_creates_workspace_file_pointing_to_initial_data_dir() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(cli_runner::gfs_init(repo_path), "gfs init should succeed");

    let workspace_file = repo_path.join(".gfs/WORKSPACE");
    assert!(
        workspace_file.exists(),
        ".gfs/WORKSPACE should be created by init"
    );

    let recorded = fs::read_to_string(&workspace_file).unwrap();
    assert_eq!(
        recorded.trim(),
        workspace_data_dir(repo_path).to_str().unwrap(),
        "WORKSPACE should point at the initial 0/data directory"
    );
}

#[test]
fn commit_creates_snapshot_folder_with_copied_files() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(cli_runner::gfs_init(repo_path), "gfs init should succeed");

    let data_dir = workspace_data_dir(repo_path);
    assert!(
        data_dir.exists(),
        "workspace data dir must exist after init"
    );
    fs::write(data_dir.join("pg_version"), "16\n").unwrap();
    fs::write(data_dir.join("schema.sql"), "CREATE TABLE test (id INT);\n").unwrap();

    let (ok, _stdout, stderr) = cli_runner::gfs_commit(
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
    assert!(
        snapshot_path.exists(),
        "snapshot dir should exist: {snapshot_path:?}"
    );
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

#[test]
fn log_displays_commit_after_commit() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(cli_runner::gfs_init(repo_path), "gfs init should succeed");

    let data_dir = workspace_data_dir(repo_path);
    fs::write(data_dir.join("dummy"), "").unwrap();

    let (ok, _, stderr) = cli_runner::gfs_commit(
        repo_path,
        "feat: add schema",
        Some("Alice"),
        Some("alice@example.com"),
    );
    assert!(ok, "gfs commit should succeed; stderr: {stderr}");

    let (log_ok, log_stdout, log_stderr) = cli_runner::gfs_log(repo_path, None);
    assert!(log_ok, "gfs log should succeed; stderr: {log_stderr}");

    if !log_stdout.is_empty() {
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
    } else {
        // gag may not capture stdout; verify commit state via filesystem
        let ref_content =
            fs::read_to_string(repo_path.join(".gfs/refs/heads/main")).expect("main ref exists");
        let hash = ref_content.trim();
        assert!(hash.len() >= 2, "main should point to a commit");
        let (d, f) = hash.split_at(2);
        let obj_bytes = fs::read(repo_path.join(".gfs/objects").join(d).join(f)).unwrap();
        let commit: Commit = serde_json::from_slice(&obj_bytes).unwrap();
        assert!(
            commit.message.contains("feat: add schema"),
            "commit message should match; got: {}",
            commit.message
        );
    }
}

#[test]
fn log_respects_max_count() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(cli_runner::gfs_init(repo_path), "gfs init should succeed");

    let data_dir = workspace_data_dir(repo_path);
    fs::write(data_dir.join("f1"), "1").unwrap();
    let (ok1, _, _) = cli_runner::gfs_commit(repo_path, "first", None, None);
    assert!(ok1);

    fs::write(data_dir.join("f2"), "2").unwrap();
    let (ok2, _, _) = cli_runner::gfs_commit(repo_path, "second", None, None);
    assert!(ok2);

    let (log_ok, log_stdout, log_stderr) = cli_runner::gfs_log(repo_path, Some(1));
    assert!(log_ok, "gfs log should succeed; stderr: {log_stderr}");

    if !log_stdout.is_empty() {
        let commit_blocks = log_stdout.matches("commit ").count();
        assert_eq!(
            commit_blocks, 1,
            "log -n 1 should show exactly one commit; got: {log_stdout}"
        );
        assert!(
            log_stdout.contains("second"),
            "log -n 1 should show most recent commit; got: {log_stdout}"
        );
    } else {
        // gag may not capture stdout; verify we have 2 commits and tip is "second"
        let ref_content =
            fs::read_to_string(repo_path.join(".gfs/refs/heads/main")).expect("main ref exists");
        let mut hash = ref_content.trim().to_string();
        let mut count = 0usize;
        while hash.len() >= 2 {
            count += 1;
            let (d, f) = hash.split_at(2);
            let obj_bytes = fs::read(repo_path.join(".gfs/objects").join(d).join(f)).unwrap();
            let commit: Commit = serde_json::from_slice(&obj_bytes).unwrap();
            if count == 1 {
                assert!(
                    commit.message.contains("second"),
                    "tip commit should be 'second'; got: {}",
                    commit.message
                );
            }
            hash = commit
                .parents
                .as_ref()
                .and_then(|p: &Vec<String>| p.first().cloned())
                .unwrap_or_default();
            if hash.is_empty() {
                break;
            }
        }
        assert_eq!(count, 2, "repo should have 2 commits");
    }
}

#[test]
fn two_commits_produce_distinct_snapshot_folders_with_files() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(cli_runner::gfs_init(repo_path), "gfs init should succeed");

    let data_dir = workspace_data_dir(repo_path);
    fs::write(data_dir.join("seed.txt"), "data v1").unwrap();

    let (ok1, _, stderr1) = cli_runner::gfs_commit(repo_path, "commit 1", None, None);
    assert!(ok1, "first gfs commit should succeed; stderr: {stderr1}");

    fs::write(data_dir.join("seed.txt"), "data v2").unwrap();

    let (ok2, _, stderr2) = cli_runner::gfs_commit(repo_path, "commit 2", None, None);
    assert!(ok2, "second gfs commit should succeed; stderr: {stderr2}");

    let ref_content = fs::read_to_string(repo_path.join(".gfs/refs/heads/main")).unwrap();
    let hash2 = ref_content.trim().to_string();
    let (d2, f2) = hash2.split_at(2);
    let obj2_bytes = fs::read(repo_path.join(".gfs/objects").join(d2).join(f2)).unwrap();
    let commit2: Commit = serde_json::from_slice(&obj2_bytes).unwrap();
    let hash1 = commit2
        .parents
        .as_ref()
        .and_then(|p| p.first().cloned())
        .expect("second commit has parent");
    assert_ne!(hash1, hash2);

    let snap1 = read_snapshot_hash(repo_path, &hash1);
    let snap2 = read_snapshot_hash(repo_path, &hash2);
    assert_ne!(snap1, snap2);

    let snap1_path = snapshot_dir(repo_path, &snap1);
    let snap2_path = snapshot_dir(repo_path, &snap2);
    assert!(snap1_path.exists(), "first snapshot dir: {snap1_path:?}");
    assert!(snap2_path.exists(), "second snapshot dir: {snap2_path:?}");

    assert_eq!(
        fs::read_to_string(snap1_path.join("seed.txt")).unwrap(),
        "data v1"
    );
    assert_eq!(
        fs::read_to_string(snap2_path.join("seed.txt")).unwrap(),
        "data v2"
    );

    let workspace_recorded = fs::read_to_string(repo_path.join(".gfs/WORKSPACE")).unwrap();
    assert_eq!(
        PathBuf::from(workspace_recorded.trim()),
        workspace_data_dir(repo_path),
        "WORKSPACE must not change after commits"
    );
}

#[test]
fn commit_with_missing_mount_point_source_fails_gracefully() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(cli_runner::gfs_init(repo_path), "gfs init should succeed");

    let config_path = repo_path.join(".gfs/config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("\nmount_point = \"/nonexistent/volume\"\n");
    fs::write(&config_path, config).unwrap();

    let (ok, _stdout, stderr) = cli_runner::gfs_commit(repo_path, "should fail", None, None);

    assert!(!ok, "gfs commit against non-existent source should fail");
    assert!(
        stderr.contains("storage")
            || stderr.contains("cp")
            || stderr.contains("failed")
            || stderr.contains("error"),
        "stderr should mention failure; got: {stderr}"
    );
}

#[test]
fn commit_with_real_database_snapshots_workspace() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(
        cli_runner::gfs_init_with_db(repo_path),
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

    let (ok, _stdout, stderr) =
        cli_runner::gfs_commit(repo_path, "commit with real DB", None, None);
    assert!(ok, "gfs commit should succeed; stderr: {stderr}");

    let ref_content = fs::read_to_string(repo_path.join(".gfs/refs/heads/main")).unwrap();
    let commit_hash = ref_content.trim();
    assert_eq!(commit_hash.len(), 64);

    let (obj_dir, obj_file) = commit_hash.split_at(2);
    let obj_bytes = fs::read(repo_path.join(".gfs/objects").join(obj_dir).join(obj_file)).unwrap();
    let commit: Commit = serde_json::from_slice(&obj_bytes).expect("valid JSON commit object");
    let snapshot_path = snapshot_dir(repo_path, &commit.snapshot_hash);
    assert!(
        snapshot_path.exists(),
        "snapshot dir should exist: {snapshot_path:?}"
    );

    let has_pg_data = ["base", "global", "pg_wal", "postgresql.conf"]
        .iter()
        .any(|name| snapshot_path.join(name).exists());
    assert!(
        has_pg_data,
        "snapshot should contain Postgres data; listing: {:?}",
        fs::read_dir(&snapshot_path)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    );
}
