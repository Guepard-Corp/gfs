use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

fn gfs_bin() -> &'static str {
    env!("CARGO_BIN_EXE_gfs")
}

fn run_gfs(cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(gfs_bin())
        .current_dir(cwd)
        .args(args)
        // Keep stderr clean for --json contract assertions.
        .env("RUST_LOG", "off")
        // And keep it uncoloured: clap writes ANSI when colour is forced, which
        // would otherwise make an assertion on the start of a line pass or fail
        // depending on the environment the suite is run from.
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run gfs");

    let code = out.status.code().unwrap_or(1);
    let stdout = String::from_utf8(out.stdout).expect("stdout must be utf-8");
    let stderr = String::from_utf8(out.stderr).expect("stderr must be utf-8");
    (code, stdout, stderr)
}

fn assert_stdout_json(stdout: &str) -> Value {
    assert!(
        !stdout.trim().is_empty(),
        "expected non-empty stdout JSON, got empty"
    );
    serde_json::from_str::<Value>(stdout).unwrap_or_else(|e| {
        panic!("stdout is not valid JSON: {e}\n--- stdout ---\n{stdout}");
    })
}

fn assert_stderr_empty(stderr: &str) {
    assert!(
        stderr.trim().is_empty(),
        "expected empty stderr, got:\n--- stderr ---\n{stderr}"
    );
}

#[test]
fn json_init_stdout_is_json() {
    let tmp = TempDir::new().unwrap();
    let (code, stdout, _stderr) = run_gfs(tmp.path(), &["--json", "init", "."]);
    assert_eq!(code, 0, "expected init to succeed");
    let v = assert_stdout_json(&stdout);
    assert!(
        v.get("branch").is_some() && v.get("path").is_some(),
        "expected init JSON to have branch + path"
    );
}

#[test]
fn json_status_stdout_is_json() {
    let tmp = TempDir::new().unwrap();
    run_gfs(tmp.path(), &["init", "."]);

    let (code, stdout, stderr) = run_gfs(tmp.path(), &["--json", "status"]);
    assert_eq!(code, 0, "expected status to succeed");
    assert_stderr_empty(&stderr);
    let v = assert_stdout_json(&stdout);
    assert!(
        v.get("current_branch").is_some(),
        "expected status JSON to have current_branch"
    );
}

#[test]
fn json_providers_stdout_is_json() {
    let tmp = TempDir::new().unwrap();
    let (code, stdout, stderr) = run_gfs(tmp.path(), &["--json", "providers"]);
    assert_eq!(code, 0, "expected providers to succeed");
    assert_stderr_empty(&stderr);
    let v = assert_stdout_json(&stdout);
    assert!(
        v.get("providers").is_some() || v.get("provider").is_some(),
        "expected providers JSON to have providers/provider"
    );
}

#[test]
fn json_branch_stdout_is_json_in_empty_repo() {
    let tmp = TempDir::new().unwrap();
    run_gfs(tmp.path(), &["init", "."]);

    let (code, stdout, stderr) = run_gfs(tmp.path(), &["--json", "branch"]);
    assert_eq!(code, 0, "expected branch list to succeed");
    assert_stderr_empty(&stderr);
    let v = assert_stdout_json(&stdout);
    assert!(
        v.get("branches").is_some(),
        "expected branch JSON to have branches"
    );
}

#[test]
fn json_log_stdout_is_json_in_empty_repo() {
    let tmp = TempDir::new().unwrap();
    run_gfs(tmp.path(), &["init", "."]);

    let (code, stdout, stderr) = run_gfs(tmp.path(), &["--json", "log"]);
    assert_eq!(code, 0, "expected log to succeed (empty list ok)");
    assert_stderr_empty(&stderr);
    let v = assert_stdout_json(&stdout);
    assert!(
        v.get("commits").is_some(),
        "expected log JSON to have commits"
    );
}

#[test]
fn json_error_commit_outside_repo_is_json() {
    let tmp = TempDir::new().unwrap();
    let (code, stdout, stderr) = run_gfs(tmp.path(), &["--json", "commit", "-m", "x"]);
    assert_eq!(code, 1, "expected commit outside repo to fail");
    assert_stderr_empty(&stderr);
    let v = assert_stdout_json(&stdout);
    assert!(v.get("error").is_some(), "expected error envelope");
}

#[test]
fn json_error_checkout_main_on_empty_repo_is_json() {
    let tmp = TempDir::new().unwrap();
    run_gfs(tmp.path(), &["init", "."]);

    let (code, stdout, stderr) = run_gfs(tmp.path(), &["--json", "checkout", "main"]);
    assert_eq!(code, 1, "expected checkout main on empty repo to fail");
    assert_stderr_empty(&stderr);
    let v = assert_stdout_json(&stdout);
    assert!(v.get("error").is_some(), "expected error envelope");
}

#[test]
fn json_error_compute_status_without_config_is_json() {
    let tmp = TempDir::new().unwrap();
    run_gfs(tmp.path(), &["init", "."]);

    let (code, stdout, stderr) = run_gfs(tmp.path(), &["--json", "compute", "status"]);
    assert_eq!(code, 1, "expected compute status without config to fail");
    assert_stderr_empty(&stderr);
    let v = assert_stdout_json(&stdout);
    assert!(v.get("error").is_some(), "expected error envelope");
}

#[test]
fn json_error_compute_logs_without_config_is_json() {
    let tmp = TempDir::new().unwrap();
    run_gfs(tmp.path(), &["init", "."]);

    let (code, stdout, stderr) = run_gfs(tmp.path(), &["--json", "compute", "logs"]);
    assert_eq!(code, 1, "expected compute logs without config to fail");
    assert_stderr_empty(&stderr);
    let v = assert_stdout_json(&stdout);
    assert!(v.get("error").is_some(), "expected error envelope");
}

/// Covers both error paths, which are not interchangeable: a parse failure is
/// formatted by clap, a runtime failure keeps the prefix `main` adds, and neither
/// may double it.
#[test]
fn an_error_is_announced_once() {
    let tmp = TempDir::new().unwrap();

    // Parse failure: `init` takes its path positionally, so `--path` is unknown.
    let (code, _, stderr) = run_gfs(tmp.path(), &["init", "--path", "somewhere"]);
    assert_eq!(code, 1, "a usage error still exits 1");
    // Counting the substring would be wrong: a message like "internal error: ..."
    // legitimately contains it. Only the prefix is under test.
    let first = stderr.lines().next().unwrap_or_default();
    assert!(
        first.starts_with("error: ") && !first.starts_with("error: error:"),
        "clap already says 'error:' once: {first}"
    );
    assert!(
        stderr.contains("Usage:"),
        "clap's usage block must survive: {stderr}"
    );

    // clap renders bare help for these and prefixes nothing, so a test covering
    // only a mistyped flag would not notice the line going missing.
    for bare in [vec![], vec!["storage"], vec!["schema"], vec!["user"]] {
        let (code, _, stderr) = run_gfs(tmp.path(), &bare);
        assert_eq!(code, 1, "`gfs {}` exits 1", bare.join(" "));
        assert!(
            stderr.lines().any(|l| l.starts_with("error: ")),
            "`gfs {}` must still announce an error: {stderr}",
            bare.join(" ")
        );
    }

    // Runtime failure: a valid command against a directory that is not a repo.
    let (code, _, stderr) = run_gfs(tmp.path(), &["status"]);
    assert_eq!(code, 1);
    let first = stderr.lines().next().unwrap_or_default();
    assert!(
        first.starts_with("error: ") && !first.starts_with("error: error:"),
        "and main's own prefix is still applied exactly once: {first}"
    );
}
