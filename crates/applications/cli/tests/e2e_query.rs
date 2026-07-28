//! End-to-end test for `gfs query` error propagation on the Docker/local runtime.
//!
//! `gfs query` on Docker shells out to the host DB client (psql) and exits with
//! the client's exit code (see `cmd_query.rs`). This guards that a **failing**
//! query exits non-zero: a silent exit 0 would hide SQL errors (division by zero,
//! missing relation, syntax error) from any caller that trusts the exit status.
//!
//! Runs `gfs` as a subprocess on purpose — the Docker query path calls
//! `std::process::exit`, which would abort an in-process test runner.
//!
//! Skips (does not fail) when the `gfs-postgres:17` image or a host `psql` is
//! unavailable, matching the repo's "skip gracefully when prerequisites are
//! missing" testing rule (e.g. CI without the locally-built image).
//!
//! macOS-only: init uses the APFS storage backend. Docker or Podman must be running.

#![cfg(target_os = "macos")]

mod common;

use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use common::cli_runner;
use gfs_domain::repo_utils::repo_layout;
use tempfile::tempdir;

fn get_container_id(repo_path: &Path) -> Option<String> {
    repo_layout::get_runtime_config(repo_path)
        .ok()
        .and_then(|opt| opt.map(|r| r.container_name))
}

/// Removes the container on drop (success or panic).
struct ContainerCleanupGuard(String);

impl Drop for ContainerCleanupGuard {
    fn drop(&mut self) {
        let _ = common::container_runtime::runtime_command()
            .args(["rm", "-f", &self.0])
            .output();
    }
}

fn image_present() -> bool {
    common::container_runtime::runtime_command()
        .args(["image", "inspect", "gfs-postgres:17"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn host_psql_present() -> bool {
    Command::new("psql")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn wait_for_postgres(container_id: &str) -> bool {
    for _ in 0..30 {
        let ok = common::container_runtime::runtime_command()
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

/// Run `gfs query` as a subprocess and return its exit code.
/// Must be a subprocess: the Docker query path calls `std::process::exit`.
fn gfs_query_code(repo_path: &Path, sql: &str) -> Option<i32> {
    Command::new(env!("CARGO_BIN_EXE_gfs"))
        .args(["query", "--path", repo_path.to_str().unwrap(), sql])
        .output()
        .expect("spawn gfs query")
        .status
        .code()
}

#[test]
fn query_exit_code_reflects_sql_success_and_failure() {
    if !image_present() {
        eprintln!("SKIP: gfs-postgres:17 image absent");
        return;
    }
    if !host_psql_present() {
        eprintln!("SKIP: host psql not installed");
        return;
    }

    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();
    assert!(
        cli_runner::gfs_init_with_db(repo_path),
        "gfs init --database-provider postgres should succeed"
    );
    let container_id = get_container_id(repo_path).expect("container_name present after init");
    let _guard = ContainerCleanupGuard(container_id.clone());
    assert!(
        wait_for_postgres(&container_id),
        "postgres in {container_id} should become ready"
    );

    // A valid query exits 0.
    assert_eq!(
        gfs_query_code(repo_path, "select 1"),
        Some(0),
        "a valid query should exit 0"
    );

    // Division by zero must surface as a non-zero exit, not a silent success.
    let code = gfs_query_code(repo_path, "select 1/0");
    assert!(
        matches!(code, Some(c) if c != 0),
        "failing query (division by zero) must exit non-zero; got {code:?}"
    );

    // A missing relation must also exit non-zero.
    let code = gfs_query_code(repo_path, "select * from no_such_table_xyz");
    assert!(
        matches!(code, Some(c) if c != 0),
        "query on a missing table must exit non-zero; got {code:?}"
    );
}
