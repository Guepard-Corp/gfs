//! Shared utilities for PostgreSQL import integration tests.
//!
//! Uses gfs_cli::run() in-process for coverage capture.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use super::cli_runner;
use gfs_domain::repo_utils::repo_layout;
use tempfile::TempDir;

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

/// Guard that stops and removes a container on drop (success or panic).
/// Ensures containers are always cleaned up, even when tests fail.
struct ContainerCleanupGuard(String);

impl Drop for ContainerCleanupGuard {
    fn drop(&mut self) {
        let runtime = super::container_runtime::runtime_binary();
        let _ = Command::new(runtime).args(["stop", &self.0]).output();
        let _ = Command::new(runtime).args(["rm", "-f", &self.0]).output();
    }
}

/// Create a fresh repo with Postgres, run the given closure, then clean up the container.
/// Uses a Drop guard so the container is always removed, even when the closure panics.
pub fn with_fresh_repo<F>(f: F)
where
    F: FnOnce(&Path),
{
    let temp = TempDir::new().expect("create temp dir for repo");
    let repo_path = temp.path();

    // 1. Init with postgres (in-process for coverage)
    assert!(
        cli_runner::gfs_init_with_db(repo_path),
        "gfs init should succeed"
    );

    // 2. Register container for cleanup as soon as we have it (runs on drop, including panic)
    let container_id = repo_layout::get_runtime_config(repo_path)
        .ok()
        .flatten()
        .map(|r| r.container_name)
        .expect("runtime config with container_name");
    let _container_guard = ContainerCleanupGuard(container_id.clone());

    // 3. Config (in-process)
    let (ok, _, _) = cli_runner::gfs_config(repo_path, "user.name", Some("Test User"));
    assert!(ok, "gfs config user.name should succeed");
    let (ok, _, _) = cli_runner::gfs_config(repo_path, "user.email", Some("test@example.com"));
    assert!(ok, "gfs config user.email should succeed");

    // 4. Wait for Postgres before commit (commit runs CHECKPOINT)
    for _ in 0..30 {
        let ok = Command::new(super::container_runtime::runtime_binary())
            .args([
                "exec",
                &container_id,
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
            break;
        }
        thread::sleep(Duration::from_secs(1));
    }

    // 5. Commit (ensures workspace and container are ready)
    let (ok, _, stderr) = cli_runner::gfs_commit(
        repo_path,
        "initial",
        Some("Test User"),
        Some("test@example.com"),
    );
    assert!(ok, "gfs commit should succeed; stderr: {stderr}");

    // 6. Run the test body (guard cleans up on panic)
    f(repo_path);
}

pub fn gfs_import(repo_path: &Path, file: &Path, format: Option<&str>) -> (bool, String, String) {
    cli_runner::gfs_import(repo_path, file, format)
}

pub fn run_psql_select(container_id: &str, query: &str) -> String {
    let out = Command::new(super::container_runtime::runtime_binary())
        .args([
            "exec",
            container_id,
            "psql",
            "-U",
            "postgres",
            "-d",
            "postgres",
            "-t",
            "-A",
            "-c",
            query,
        ])
        .output()
        .expect("run psql");
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

pub fn get_container_id(repo_path: &Path) -> String {
    repo_layout::get_runtime_config(repo_path)
        .ok()
        .flatten()
        .map(|r| r.container_name)
        .expect("container_id")
}
