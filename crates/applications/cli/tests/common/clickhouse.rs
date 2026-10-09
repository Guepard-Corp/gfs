//! Shared utilities for ClickHouse integration tests.

#![allow(dead_code)]

use std::path::Path;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use super::cli_runner;
use super::container_runtime;
use gfs_domain::repo_utils::repo_layout;

const TEST_VERSION: &str = "24.8.14.39";
const TEST_USER: &str = "default";
const TEST_PASSWORD: &str = "clickhouse";

/// Guard that stops and removes a container on drop (success or panic).
struct ContainerCleanupGuard {
    initial_container_id: String,
    repo_path: PathBuf,
}

impl Drop for ContainerCleanupGuard {
    fn drop(&mut self) {
        let _ = container_runtime::runtime_command()
            .args(["stop", &self.initial_container_id])
            .output();
        let _ = container_runtime::runtime_command()
            .args(["rm", "-f", &self.initial_container_id])
            .output();

        if let Some(current_container_id) = repo_layout::get_runtime_config(&self.repo_path)
            .ok()
            .flatten()
            .map(|r| r.container_name)
            .filter(|id| id != &self.initial_container_id)
        {
            let _ = container_runtime::runtime_command()
                .args(["stop", &current_container_id])
                .output();
            let _ = container_runtime::runtime_command()
                .args(["rm", "-f", &current_container_id])
                .output();
        }
    }
}

/// Create a fresh repo with ClickHouse, run the given closure, then clean up the container.
pub fn with_fresh_repo<F>(f: F)
where
    F: FnOnce(&Path),
{
    let temp = crate::common::shared_tempdir::shared_tempdir().expect("create temp dir for repo");
    let repo_path = temp.path();

    assert!(
        cli_runner::gfs_init_with_provider(repo_path, "clickhouse", TEST_VERSION),
        "gfs init should succeed"
    );

    let container_id = get_container_id(repo_path);
    let _container_guard = ContainerCleanupGuard {
        initial_container_id: container_id.clone(),
        repo_path: repo_path.to_path_buf(),
    };

    let (ok, _, _) = cli_runner::gfs_config(repo_path, "user.name", Some("Test User"));
    assert!(ok, "gfs config user.name should succeed");
    let (ok, _, _) = cli_runner::gfs_config(repo_path, "user.email", Some("test@example.com"));
    assert!(ok, "gfs config user.email should succeed");

    assert!(
        wait_for_clickhouse(&container_id),
        "ClickHouse should be ready before test body"
    );

    f(repo_path);
}

pub fn run_clickhouse_query(container_id: &str, query: &str) -> String {
    let mut last_output = String::new();
    for _ in 0..10 {
        let out = container_runtime::runtime_command()
            .args([
                "exec",
                container_id,
                "clickhouse-client",
                "--user",
                TEST_USER,
                "--password",
                TEST_PASSWORD,
                "--connect_timeout",
                "2",
                "--query",
                query,
            ])
            .output()
            .expect("run clickhouse-client");
        last_output = format!(
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if out.status.success() {
            return last_output;
        }
        thread::sleep(Duration::from_secs(1));
    }
    last_output
}

pub fn get_container_id(repo_path: &Path) -> String {
    repo_layout::get_runtime_config(repo_path)
        .ok()
        .flatten()
        .map(|r| r.container_name)
        .expect("runtime config with container_name")
}

pub fn wait_for_clickhouse(container_id: &str) -> bool {
    let mut last = String::new();
    for _ in 0..30 {
        let out = container_runtime::runtime_command()
            .args([
                "exec",
                container_id,
                "clickhouse-client",
                "--user",
                TEST_USER,
                "--password",
                TEST_PASSWORD,
                "--connect_timeout",
                "2",
                "--query",
                "SELECT 1",
            ])
            .output();
        let ok = out.as_ref().map(|o| o.status.success()).unwrap_or(false);
        if ok {
            return true;
        }
        if let Ok(o) = out {
            last = format!(
                "{}\n{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
        }
        thread::sleep(Duration::from_secs(1));
    }
    eprintln!("clickhouse readiness failed, last output:\n{last}");
    false
}
