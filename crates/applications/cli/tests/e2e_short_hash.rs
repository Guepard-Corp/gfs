//! End-to-end tests for short hash support
//!
//! Runs CLI in-process via gfs_cli::run() for coverage capture.
//! macOS-only: uses the APFS storage backend. Docker or Podman required for DB tests.

#![cfg(target_os = "macos")]

mod common;

use std::path::Path;
use std::thread;
use std::time::Duration;

use common::cli_runner;
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
        let ok = std::process::Command::new(common::container_runtime::runtime_binary())
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
        let _ = std::process::Command::new(common::container_runtime::runtime_binary())
            .args(["stop", &self.0])
            .output();
        let _ = std::process::Command::new(common::container_runtime::runtime_binary())
            .args(["rm", "-f", &self.0])
            .output();
    }
}

/// Helper to run gfs init with database
fn gfs_init_with_database(path: &Path, provider: &str, version: &str) -> bool {
    let (ok, _, _) = cli_runner::run_gfs([
        "gfs",
        "init",
        "--database-provider",
        provider,
        "--database-version",
        version,
        path.to_str().unwrap(),
    ]);
    ok
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn log_shows_short_hash_by_default() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    // Init with postgres
    assert!(
        gfs_init_with_database(repo_path, "postgres", "17"),
        "gfs init should succeed"
    );

    let container_id = get_container_id(repo_path).expect("container_name in config");
    let _guard = ContainerCleanupGuard(container_id.clone());

    assert!(
        wait_for_postgres(&container_id),
        "postgres should accept connections"
    );

    // Make a commit
    let (ok, _, _) = cli_runner::gfs_commit(repo_path, "test commit", None, None);
    assert!(ok, "commit should succeed");

    // Get the full hash
    let full_hash = repo_layout::get_current_commit_id(repo_path).expect("get commit hash");

    // Run log and capture output
    let (ok, stdout, _) = cli_runner::gfs_log(repo_path, None);
    assert!(ok, "log should succeed");

    // Verify short hash (7 chars) is displayed
    let short_hash = &full_hash[..7];
    assert!(
        stdout.contains(short_hash),
        "log should show 7-char short hash"
    );

    // Verify full hash is NOT displayed (unless by coincidence in other fields)
    let first_line = stdout.lines().next().unwrap();
    assert!(
        !first_line.contains(&full_hash),
        "log should not show full 64-char hash by default"
    );
}

#[test]
fn log_shows_full_hash_with_flag() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    // Init with postgres
    assert!(
        gfs_init_with_database(repo_path, "postgres", "17"),
        "gfs init should succeed"
    );

    let container_id = get_container_id(repo_path).expect("container_name in config");
    let _guard = ContainerCleanupGuard(container_id.clone());

    assert!(
        wait_for_postgres(&container_id),
        "postgres should accept connections"
    );

    // Make a commit
    let (ok, _, _) = cli_runner::gfs_commit(repo_path, "test commit", None, None);
    assert!(ok, "commit should succeed");

    // Get the full hash
    let full_hash = repo_layout::get_current_commit_id(repo_path).expect("get commit hash");

    // Run log with --full-hash flag
    let (ok, stdout, _) = cli_runner::run_gfs([
        "gfs",
        "log",
        "--full-hash",
        "--path",
        repo_path.to_str().unwrap(),
    ]);
    assert!(ok, "log with --full-hash should succeed");

    // Verify full hash is displayed
    assert!(
        stdout.contains(&full_hash),
        "log --full-hash should show full 64-char hash"
    );
}

#[test]
fn checkout_accepts_short_hash() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    // Init with postgres
    assert!(
        gfs_init_with_database(repo_path, "postgres", "17"),
        "gfs init should succeed"
    );

    let container_id = get_container_id(repo_path).expect("container_name in config");
    let _guard = ContainerCleanupGuard(container_id.clone());

    assert!(
        wait_for_postgres(&container_id),
        "postgres should accept connections"
    );

    // Make first commit
    let (ok, _, _) = cli_runner::gfs_commit(repo_path, "first commit", None, None);
    assert!(ok, "first commit should succeed");

    let hash1 = repo_layout::get_current_commit_id(repo_path).expect("get commit hash");

    // Make second commit
    let (ok, _, _) = cli_runner::gfs_commit(repo_path, "second commit", None, None);
    assert!(ok, "second commit should succeed");

    // Checkout using 7-char short hash of first commit
    let short_hash = &hash1[..7];
    let (ok, _, _) = cli_runner::gfs_checkout(repo_path, short_hash);
    assert!(ok, "checkout with short hash should succeed");

    // Verify we're at the correct commit
    let current = repo_layout::get_current_commit_id(repo_path).expect("get current commit");
    assert_eq!(current, hash1, "should be checked out to first commit");
}

#[test]
fn short_hash_works_with_tilde_notation() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    // Init with postgres
    assert!(
        gfs_init_with_database(repo_path, "postgres", "17"),
        "gfs init should succeed"
    );

    let container_id = get_container_id(repo_path).expect("container_name in config");
    let _guard = ContainerCleanupGuard(container_id.clone());

    assert!(
        wait_for_postgres(&container_id),
        "postgres should accept connections"
    );

    // Make three commits
    for i in 1..=3 {
        let (ok, _, _) = cli_runner::gfs_commit(repo_path, &format!("commit {}", i), None, None);
        assert!(ok, "commit {} should succeed", i);
    }

    let hash3 = repo_layout::get_current_commit_id(repo_path).expect("get commit hash");

    // Checkout using short hash with tilde notation
    let short_hash = &hash3[..7];
    let revision = format!("{}~2", short_hash);
    let (ok, _, _) = cli_runner::gfs_checkout(repo_path, &revision);
    assert!(ok, "checkout with short hash and tilde should succeed");

    // Verify we're 2 commits back (at commit 1 which is the initial commit "0")
    let current = repo_layout::get_current_commit_id(repo_path).expect("get current commit");
    assert_eq!(current, "0", "should be checked out to initial commit");
}

#[test]
fn short_hash_error_on_not_found() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(cli_runner::gfs_init(repo_path), "gfs init should succeed");

    // Try to checkout non-existent short hash
    let (ok, _, _) = cli_runner::gfs_checkout(repo_path, "ffffffff");
    assert!(!ok, "checkout with non-existent short hash should fail");
}

#[test]
fn short_hash_minimum_length() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    // Init with postgres
    assert!(
        gfs_init_with_database(repo_path, "postgres", "17"),
        "gfs init should succeed"
    );

    let container_id = get_container_id(repo_path).expect("container_name in config");
    let _guard = ContainerCleanupGuard(container_id.clone());

    assert!(
        wait_for_postgres(&container_id),
        "postgres should accept connections"
    );

    // Make a commit
    let (ok, _, _) = cli_runner::gfs_commit(repo_path, "test commit", None, None);
    assert!(ok, "commit should succeed");

    let full_hash = repo_layout::get_current_commit_id(repo_path).expect("get commit hash");

    // Checkout main first
    let (ok, _, _) = cli_runner::gfs_checkout(repo_path, "main");
    assert!(ok, "checkout main should succeed");

    // Try with 4-char short hash (minimum length)
    let short_hash = &full_hash[..4];
    let (ok, _, _) = cli_runner::gfs_checkout(repo_path, short_hash);
    assert!(ok, "checkout with 4-char short hash should succeed");

    // Verify we're at the correct commit
    let current = repo_layout::get_current_commit_id(repo_path).expect("get current commit");
    assert_eq!(current, full_hash, "should be checked out to the commit");
}
