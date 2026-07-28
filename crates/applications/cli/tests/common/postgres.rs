//! Shared utilities for PostgreSQL import integration tests.
//!
//! Uses gfs_cli::run() in-process for coverage capture.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use super::cli_runner;
use gfs_domain::repo_utils::repo_layout;
use tempfile::Builder;

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

/// Guard that stops and removes a container on drop (success or panic).
/// Ensures containers are always cleaned up, even when tests fail.
struct ContainerCleanupGuard(String);

impl Drop for ContainerCleanupGuard {
    fn drop(&mut self) {
        let _ = super::container_runtime::runtime_command()
            .args(["stop", &self.0])
            .output();
        let _ = super::container_runtime::runtime_command()
            .args(["rm", "-f", &self.0])
            .output();
    }
}

/// Create a fresh repo with Postgres, run the given closure, then clean up the container.
/// Uses a Drop guard so the container is always removed, even when the closure panics.
pub fn with_fresh_repo<F>(f: F)
where
    F: FnOnce(&Path),
{
    // `gfs init` below provisions a `gfs-postgres:17` container. That image is built
    // locally (not published to any registry), so an environment without it — e.g.
    // GitHub Actions running a full `cargo test` — can't pull it and would otherwise
    // panic at `gfs init`. Skip (don't fail) when the image is absent, matching
    // e2e_clone_postgres and the repo's "skip gracefully when prerequisites are
    // missing" testing rule.
    let img_ok = super::container_runtime::runtime_command()
        .args(["image", "inspect", "gfs-postgres:17"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !img_ok {
        eprintln!(
            "SKIP: image gfs-postgres:17 absent — build: docker build -t gfs-postgres:17 crates/extensions/gfs"
        );
        return;
    }

    // Use a Docker-shareable temp directory (not /tmp which Docker can't mount on Linux)
    // Use home directory which is typically Docker-shareable, fall back to system temp
    let temp_base = std::env::var("HOME")
        .ok()
        .map(|h| {
            let base = PathBuf::from(h).join(".gfs-test-tmp");
            // Ensure the base directory exists
            let _ = std::fs::create_dir_all(&base);
            base
        })
        .or_else(|| {
            // Try /var/tmp as alternative (often Docker-shareable on Linux)
            let var_tmp = PathBuf::from("/var/tmp");
            if var_tmp.exists() {
                Some(var_tmp)
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            // Last resort: use system temp (may fail on Linux with Docker)
            std::env::temp_dir()
        });

    let temp = Builder::new()
        .prefix("gfs-test-")
        .tempdir_in(&temp_base)
        .expect("create temp dir for repo");
    let repo_path = temp.path();

    // 1. Init with postgres (in-process for coverage)
    let (init_ok, init_stdout, init_stderr) = cli_runner::run_gfs(vec![
        "gfs",
        "init",
        "--database-provider",
        "postgres",
        "--database-version",
        "17",
        repo_path.to_str().unwrap(),
    ]);
    assert!(
        init_ok,
        "gfs init should succeed\nstdout: {}\nstderr: {}",
        init_stdout, init_stderr
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
        let ok = super::container_runtime::runtime_command()
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
    let out = super::container_runtime::runtime_command()
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
