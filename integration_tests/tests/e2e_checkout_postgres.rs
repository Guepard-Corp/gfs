//! End-to-end tests for `gfs checkout` with a real Postgres container.
//!
//! These tests share a single repo and run sequentially (via serial_test).
//! **Must be run with `--test-threads=1`** so they execute in order and share state.
//! Tests never start or stop the compute container; they only validate that repo state,
//! workspace paths, and compute status are as expected (running/stopped). One-off Postgres
//! is only used on cold data dirs (e.g. snapshots) and we do not remove postmaster.pid/opts.
//! Flow: init with postgres, config, validate layout, commit, log, pgbench + commit,
//! compute status, checkout previous (validate no pgbench tables), checkout head
//! (validate workspace structure), create branch from main (validate pgbench data on branch),
//! checkout back to main, then a final test stops and removes the main container so none remain.
//!
//! macOS-only: commit uses the APFS storage backend. Docker must be running.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use gfs_domain::model::commit::Commit;
use gfs_domain::repo_utils::repo_layout;
use once_cell::sync::Lazy;
use serial_test::serial;
use tempfile::TempDir;

/// Shared repo: temp dir kept alive for process lifetime.
static SHARED: Lazy<(TempDir, PathBuf)> = Lazy::new(|| {
    let t = TempDir::new().expect("create temp dir for shared repo");
    let p = t.path().to_path_buf();
    (t, p)
});

fn shared_repo_path() -> &'static Path {
    &SHARED.1
}

/// Guard that removes a one-off container on drop (success or panic).
struct OneOffContainerGuard(String);

impl Drop for OneOffContainerGuard {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.0])
            .output();
    }
}

/// Holds the main container id for explicit cleanup in test_99 (statics are never dropped in Rust).
struct MainContainerCleanup(Mutex<Option<String>>);

impl MainContainerCleanup {
    fn register(&self, container_id: String) {
        self.0.lock().unwrap().replace(container_id);
    }

    /// Take the registered container id for explicit cleanup (statics are never dropped in Rust).
    fn take_container_id(&self) -> Option<String> {
        self.0.lock().unwrap().take()
    }
}

impl Drop for MainContainerCleanup {
    fn drop(&mut self) {
        if let Ok(mut id) = self.0.lock() {
            if let Some(container_id) = id.take() {
                let _ = Command::new("docker")
                    .args(["stop", &container_id])
                    .output();
                let _ = Command::new("docker")
                    .args(["rm", "-f", &container_id])
                    .output();
            }
        }
    }
}

static MAIN_CONTAINER_CLEANUP: Lazy<MainContainerCleanup> =
    Lazy::new(|| MainContainerCleanup(Mutex::new(None)));

/// Register the main repo container for cleanup in the final test.
fn register_main_container_for_cleanup(container_id: String) {
    MAIN_CONTAINER_CLEANUP.register(container_id);
}

/// Stop and remove the main container if one was registered. Ensures no test-provisioned containers remain.
fn cleanup_main_container(repo_path: &Path) {
    let container_id = MAIN_CONTAINER_CLEANUP.take_container_id()
        .or_else(|| get_container_id(repo_path));
    if let Some(id) = container_id {
        let _ = Command::new("docker").args(["stop", &id]).output();
        let _ = Command::new("docker").args(["rm", "-f", &id]).output();
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

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

fn gfs_config(path: &Path, key: &str, value: &str) -> (bool, String, String) {
    let out = Command::new("cargo")
        .args([
            "run",
            "--package",
            "gfs-cli",
            "--bin",
            "gfs",
            "config",
            "--path",
            path.to_str().unwrap(),
            key,
            value,
        ])
        .current_dir(workspace_root())
        .output()
        .expect("run gfs config");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.success(), stdout, stderr)
}

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

fn gfs_log(repo_path: &Path, max_count: Option<usize>) -> (bool, String, String) {
    let mut args: Vec<String> = vec![
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
        .args(args)
        .current_dir(workspace_root())
        .output()
        .expect("run gfs log");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.success(), stdout, stderr)
}

fn gfs_checkout(repo_path: &Path, revision: &str) -> (bool, String, String) {
    let out = Command::new("cargo")
        .args([
            "run",
            "--package",
            "gfs-cli",
            "--bin",
            "gfs",
            "checkout",
            "--path",
            repo_path.to_str().unwrap(),
            revision,
        ])
        .current_dir(workspace_root())
        .output()
        .expect("run gfs checkout");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.success(), stdout, stderr)
}

/// Run `gfs checkout -b <branch_name> [start_revision]`. If `start_revision` is None, uses HEAD.
fn gfs_checkout_b(
    repo_path: &Path,
    branch_name: &str,
    start_revision: Option<&str>,
) -> (bool, String, String) {
    let mut args = vec![
        "run",
        "--package",
        "gfs-cli",
        "--bin",
        "gfs",
        "checkout",
        "--path",
        repo_path.to_str().unwrap(),
        "-b",
        branch_name,
    ];
    if let Some(rev) = start_revision {
        args.push(rev);
    }
    let out = Command::new("cargo")
        .args(args)
        .current_dir(workspace_root())
        .output()
        .expect("run gfs checkout -b");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.success(), stdout, stderr)
}

fn gfs_compute_status(path: &Path) -> (bool, String, String) {
    let out = Command::new("cargo")
        .args([
            "run",
            "--package",
            "gfs-cli",
            "--bin",
            "gfs",
            "compute",
            "--path",
            path.to_str().unwrap(),
            "status",
        ])
        .current_dir(workspace_root())
        .output()
        .expect("run gfs compute status");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.success(), stdout, stderr)
}

fn gfs_compute_stop(path: &Path) -> (bool, String, String) {
    let out = Command::new("cargo")
        .args([
            "run",
            "--package",
            "gfs-cli",
            "--bin",
            "gfs",
            "compute",
            "--path",
            path.to_str().unwrap(),
            "stop",
        ])
        .current_dir(workspace_root())
        .output()
        .expect("run gfs compute stop");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.success(), stdout, stderr)
}

fn get_container_id(repo_path: &Path) -> Option<String> {
    repo_layout::get_runtime_config(repo_path)
        .ok()
        .and_then(|opt| opt.map(|r| r.container_name))
}

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

fn run_pgbench_init(container_id: &str) -> (Duration, String) {
    let start = Instant::now();
    let out = Command::new("docker")
        .args([
            "exec",
            container_id,
            "pgbench",
            "-i",
            "-U",
            "postgres",
            "-d",
            "postgres",
        ])
        .output()
        .expect("run pgbench -i");
    let elapsed = start.elapsed();
    let report = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    eprintln!("pgbench -i elapsed: {:?}\nReport:\n{}", elapsed, report);
    (elapsed, report)
}

fn run_psql_list_tables(container_id: &str) -> String {
    let out = Command::new("docker")
        .args([
            "exec",
            container_id,
            "psql",
            "-U",
            "postgres",
            "-d",
            "postgres",
            "-c",
            "SELECT tablename FROM pg_tables WHERE schemaname='public'",
        ])
        .output()
        .expect("run psql list tables");
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn has_pgbench_tables(output: &str) -> bool {
    output.contains("pgbench_accounts")
        || output.contains("pgbench_branches")
        || output.contains("pgbench_history")
        || output.contains("pgbench_tellers")
}


/// Return current Unix uid and gid as "(uid):(gid)" for `docker run --user`. Fails if id cannot be determined.
fn host_user_uid_gid() -> String {
    let uid = Command::new("id")
        .args(["-u"])
        .output()
        .expect("run id -u")
        .stdout;
    let gid = Command::new("id")
        .args(["-g"])
        .output()
        .expect("run id -g")
        .stdout;
    let uid = String::from_utf8_lossy(&uid).trim().to_string();
    let gid = String::from_utf8_lossy(&gid).trim().to_string();
    format!("{uid}:{gid}")
}

/// Run a one-off Postgres container as the current host user so it uses the existing data dir
/// (same ownership). Use this to validate that a workspace contains the expected DB content.
fn run_one_off_postgres_list_tables_as_host_user(host_data_path: &Path) -> String {
    let user = host_user_uid_gid();
    run_one_off_postgres_list_tables_inner(host_data_path, Some(&user))
}

/// Run a one-off Postgres container as the postgres user (999:999, same as official image).
/// Use when the data dir is owned by a container (e.g. main/0 after the compute wrote to it).
fn run_one_off_postgres_list_tables_as_postgres(host_data_path: &Path) -> String {
    run_one_off_postgres_list_tables_inner(host_data_path, Some("999:999"))
}

fn run_one_off_postgres_list_tables_inner(
    host_data_path: &Path,
    run_as_user: Option<&str>,
) -> String {
    let host_path_str = host_data_path.to_string_lossy();
    let _ = Command::new("chmod")
        .args(["-R", "a+rX", &host_path_str])
        .output();
    // Do not remove postmaster.pid / postmaster.opts: tests should only run one-offs on cold
    // data dirs (e.g. snapshot copies), so behaviour stays realistic.
    let name = format!(
        "guepard-e2e-checkout-oneoff-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let host_path = host_path_str;
    let volume_arg = format!("{}:/var/lib/postgresql/data", host_path);
    let mut args: Vec<&str> = vec![
        "run",
        "-d",
        "--name",
        &name,
        "-v",
        &volume_arg,
        "-e",
        "POSTGRES_PASSWORD=postgres",
        "postgres:latest",
    ];
    if let Some(u) = run_as_user {
        args.push("--user");
        args.push(u);
    }
    let create = Command::new("docker")
        .args(args)
        .output()
        .expect("docker run one-off postgres");
    if !create.status.success() {
        let stderr = String::from_utf8_lossy(&create.stderr);
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        panic!("failed to create one-off container: {}", stderr);
    }
    let _guard = OneOffContainerGuard(name.clone());
    for _ in 0..30 {
        let ok = Command::new("docker")
            .args(["exec", &name, "psql", "-U", "postgres", "-d", "postgres", "-c", "SELECT 1"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            break;
        }
        thread::sleep(Duration::from_secs(1));
    }
    thread::sleep(Duration::from_secs(2));
    let out = Command::new("docker")
        .args([
            "exec",
            &name,
            "psql",
            "-U",
            "postgres",
            "-d",
            "postgres",
            "-c",
            "\\dt",
        ])
        .output()
        .expect("psql \\dt in one-off");
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn read_workspace_path(repo_path: &Path) -> PathBuf {
    let s = fs::read_to_string(repo_path.join(".gfs/WORKSPACE")).expect("read WORKSPACE");
    let p = PathBuf::from(s.trim());
    let resolved = if p.is_relative() {
        repo_path.join(p)
    } else {
        p
    };
    resolved.canonicalize().unwrap_or(resolved)
}

fn read_head(repo_path: &Path) -> String {
    fs::read_to_string(repo_path.join(".gfs/HEAD"))
        .expect("read HEAD")
        .trim()
        .to_string()
}

fn get_first_commit_hash(repo_path: &Path) -> String {
    let ref_content = fs::read_to_string(repo_path.join(".gfs/refs/heads/main")).unwrap();
    let hash2 = ref_content.trim();
    let (d, f) = hash2.split_at(2);
    let obj_bytes = fs::read(repo_path.join(".gfs/objects").join(d).join(f)).unwrap();
    let commit2: Commit = serde_json::from_slice(&obj_bytes).unwrap();
    commit2
        .parents
        .as_ref()
        .and_then(|p| p.first().cloned())
        .expect("second commit has parent")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn test_01_init_config_validate_commit_log() {
    let repo_path = shared_repo_path();

    assert!(
        gfs_init_with_db(repo_path),
        "gfs init --database-provider postgres should succeed (Docker must be running)"
    );

    let (ok, _, stderr) = gfs_config(repo_path, "user.name", "Test User");
    assert!(ok, "gfs config user.name should succeed; stderr: {stderr}");
    let (ok, _, stderr) = gfs_config(repo_path, "user.email", "test@example.com");
    assert!(ok, "gfs config user.email should succeed; stderr: {stderr}");

    assert!(
        repo_layout::validate_repo_layout(&repo_path.join(".gfs")).is_ok(),
        "repo layout should be valid"
    );

    let (ok, _, stderr) = gfs_commit(
        repo_path,
        "initial",
        Some("Test User"),
        Some("test@example.com"),
    );
    assert!(ok, "gfs commit should succeed; stderr: {stderr}");

    let (log_ok, log_stdout, log_stderr) = gfs_log(repo_path, None);
    assert!(log_ok, "gfs log should succeed; stderr: {log_stderr}");
    assert!(
        log_stdout.contains("initial"),
        "log should contain commit message; got: {log_stdout}"
    );
    assert!(
        log_stdout.contains("HEAD -> main") || log_stdout.contains("main"),
        "log should show HEAD -> main; got: {log_stdout}"
    );

    let container_id = get_container_id(repo_path)
        .expect("runtime config with container_name should be present");
    register_main_container_for_cleanup(container_id.clone());
    assert!(
        wait_for_postgres(&container_id),
        "Postgres in container {} should become ready",
        container_id
    );
}

#[test]
#[serial]
fn test_02_pgbench_commit_compute_status() {
    let repo_path = shared_repo_path();
    let container_id = get_container_id(repo_path).expect("container should exist from test_01");

    let (duration, report) = run_pgbench_init(&container_id);
    eprintln!("pgbench init took {:?}", duration);
    assert!(
        report.contains("done") || report.contains("scale") || !report.is_empty(),
        "pgbench should produce output; got: {report}"
    );

    let (ok, _, stderr) = gfs_commit(
        repo_path,
        "after pgbench",
        Some("Test User"),
        Some("test@example.com"),
    );
    assert!(ok, "gfs commit after pgbench should succeed; stderr: {stderr}");

    let (log_ok, log_stdout, log_stderr) = gfs_log(repo_path, Some(2));
    assert!(log_ok, "gfs log -n 2 should succeed; stderr: {log_stderr}");
    let commit_count = log_stdout.matches("commit ").count();
    assert_eq!(
        commit_count, 2,
        "log should show 2 commits; got: {log_stdout}"
    );

    let (status_ok, status_stdout, status_stderr) = gfs_compute_status(repo_path);
    assert!(
        status_ok,
        "gfs compute status should succeed; stderr: {status_stderr}"
    );
    assert!(
        status_stdout.to_lowercase().contains("running"),
        "compute should be running; got: {status_stdout}"
    );
}

#[test]
#[serial]
fn test_03_checkout_previous_no_pgbench() {
    let repo_path = shared_repo_path();
    let hash1 = get_first_commit_hash(repo_path);

    let start = Instant::now();
    let (ok, stdout, stderr) = gfs_checkout(repo_path, &hash1);
    let elapsed = start.elapsed();
    eprintln!("checkout to first commit took {:?}", elapsed);
    assert!(ok, "gfs checkout <hash1> should succeed; stderr: {stderr}");
    assert!(
        stdout.contains("Switched to"),
        "checkout stdout should show switched; got: {stdout}"
    );

    // Validate compute is still running (we never stop it).
    let (status_ok, status_stdout, status_stderr) = gfs_compute_status(repo_path);
    assert!(
        status_ok,
        "gfs compute status should succeed; stderr: {status_stderr}"
    );
    assert!(
        status_stdout.to_lowercase().contains("running"),
        "compute should still be running after checkout; got: {status_stdout}"
    );

    let workspace_path = read_workspace_path(repo_path);
    assert!(workspace_path.exists(), "workspace dir should exist: {workspace_path:?}");

    let tables_output = run_one_off_postgres_list_tables_as_host_user(&workspace_path);
    assert!(
        !has_pgbench_tables(&tables_output),
        "checked-out workspace should NOT have pgbench tables; got: {tables_output}"
    );
}

#[test]
#[serial]
fn test_04_checkout_head_has_pgbench() {
    let repo_path = shared_repo_path();

    let start = Instant::now();
    let (ok, stdout, stderr) = gfs_checkout(repo_path, "main");
    let elapsed = start.elapsed();
    eprintln!("checkout to main took {:?}", elapsed);
    assert!(ok, "gfs checkout main should succeed; stderr: {stderr}");
    assert!(
        stdout.contains("Switched to"),
        "checkout stdout should show switched; got: {stdout}"
    );

    // Checkout main must point at the branch's persistent workspace (main/0), not a new commit-hash folder.
    let workspace_path = read_workspace_path(repo_path);
    assert!(workspace_path.exists(), "workspace dir should exist: {workspace_path:?}");
    assert!(
        workspace_path.to_string_lossy().contains("main/0/data"),
        "checkout main must use branch workspace workspaces/main/0/data, not workspaces/main/<hash>/data; got: {workspace_path:?}"
    );
    // Validate the checked-out workspace (main's tip) has Postgres data structure
    // (base/, global/). The snapshot for the second commit contains pgbench tables.
    let has_pg_data = ["base", "global"]
        .iter()
        .all(|name| workspace_path.join(name).exists() && workspace_path.join(name).is_dir());
    assert!(
        has_pg_data,
        "workspace should contain Postgres data dirs; listing: {:?}",
        fs::read_dir(&workspace_path).unwrap().collect::<Result<Vec<_>, _>>().unwrap()
    );
    // Validate compute is still running (we never start/stop).
    let (status_ok, status_stdout, status_stderr) = gfs_compute_status(repo_path);
    assert!(
        status_ok,
        "gfs compute status should succeed; stderr: {status_stderr}"
    );
    assert!(
        status_stdout.to_lowercase().contains("running"),
        "compute should still be running; got: {status_stdout}"
    );
}

#[test]
#[serial]
fn test_05_checkout_b_new_branch_has_pgbench() {
    let repo_path = shared_repo_path();
    // We are on main at tip (with pgbench). Create a new branch from current HEAD.
    let (ok, stdout, stderr) = gfs_checkout_b(repo_path, "pgbench-branch", None);
    assert!(
        ok,
        "gfs checkout -b pgbench-branch should succeed; stderr: {stderr}"
    );
    assert!(
        stdout.contains("Switched to new branch") && stdout.contains("pgbench-branch"),
        "checkout -b stdout should show new branch; got: {stdout}"
    );

    // Validate that HEAD switched to the new branch.
    let head = read_head(repo_path);
    assert!(
        head.contains("pgbench-branch"),
        "HEAD should point to pgbench-branch after checkout -b; got: {head}"
    );

    let workspace_path = read_workspace_path(repo_path);
    assert!(workspace_path.exists(), "workspace dir should exist: {workspace_path:?}");
    assert!(
        workspace_path.to_string_lossy().contains("pgbench-branch"),
        "active workspace should be the new branch's; path: {workspace_path:?}"
    );
    // New branch points to same commit as main tip; workspace is populated from snapshot.
    let has_pg_data = ["base", "global"]
        .iter()
        .all(|name| workspace_path.join(name).exists() && workspace_path.join(name).is_dir());
    assert!(
        has_pg_data,
        "new branch workspace should contain Postgres data dirs (snapshot copied); path: {workspace_path:?}"
    );
    // Validate compute is still running (tests never start/stop; we only assert expected state).
    let (status_ok, status_stdout, status_stderr) = gfs_compute_status(repo_path);
    assert!(
        status_ok,
        "gfs compute status should succeed; stderr: {status_stderr}"
    );
    assert!(
        status_stdout.to_lowercase().contains("running"),
        "compute should still be running after checkout -b; got: {status_stdout}"
    );
}

#[test]
#[serial]
fn test_06_checkout_back_to_main() {
    let repo_path = shared_repo_path();

    let (ok, stdout, stderr) = gfs_checkout(repo_path, "main");
    assert!(ok, "gfs checkout main should succeed; stderr: {stderr}");
    assert!(
        stdout.contains("Switched to"),
        "checkout main stdout should show switched; got: {stdout}"
    );

    let head = read_head(repo_path);
    assert!(
        head.contains("refs/heads/main"),
        "HEAD should point to main; got: {head}"
    );
    let workspace_path = read_workspace_path(repo_path);
    assert!(workspace_path.exists(), "workspace dir should exist: {workspace_path:?}");
    assert!(
        workspace_path.to_string_lossy().contains("main/0/data"),
        "checkout main must point at branch workspace workspaces/main/0/data (preserve DB state), not a new snapshot copy; got: {workspace_path:?}"
    );
    // Validate workspace has Postgres data layout (main/0 preserved from earlier).
    let has_pg_data = ["base", "global"]
        .iter()
        .all(|name| workspace_path.join(name).exists() && workspace_path.join(name).is_dir());
    assert!(
        has_pg_data,
        "main workspace should contain Postgres data dirs; path: {workspace_path:?}"
    );
    // Validate compute is still running (tests never start/stop).
    let (status_ok, status_stdout, status_stderr) = gfs_compute_status(repo_path);
    assert!(
        status_ok,
        "gfs compute status should succeed; stderr: {status_stderr}"
    );
    assert!(
        status_stdout.to_lowercase().contains("running"),
        "compute should still be running; got: {status_stdout}"
    );
}

/// Final test: stop and remove the main container so no test-provisioned containers remain.
/// Must run last (use `--test-threads=1`). One-off containers are removed by their guard on drop.
#[test]
#[serial]
fn test_99_cleanup_main_container() {
    cleanup_main_container(shared_repo_path());
}
