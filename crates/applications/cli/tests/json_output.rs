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

// ---------------------------------------------------------------------------
// fsck
// ---------------------------------------------------------------------------

/// A freshly initialised repository has nothing unreachable and nothing broken,
/// so fsck must exit 0 — the code a script uses to decide there is no work.
#[test]
fn fsck_on_a_fresh_repo_is_clean_and_exits_zero() {
    let tmp = TempDir::new().unwrap();
    let (code, _, _) = run_gfs(tmp.path(), &["init", "."]);
    assert_eq!(code, 0, "init should succeed");

    let (code, stdout, stderr) = run_gfs(tmp.path(), &["fsck", "--json"]);
    assert_stderr_empty(&stderr);
    let v = assert_stdout_json(&stdout);

    assert_eq!(code, 0, "a clean repository must exit 0, got {code}");
    assert_eq!(v["exit_code"], 0);
    assert_eq!(v["fsck"]["unreachable"].as_array().unwrap().len(), 0);
    assert_eq!(v["fsck"]["dangling"].as_array().unwrap().len(), 0);
    assert_eq!(v["fsck"]["unrecognised"].as_array().unwrap().len(), 0);
}

/// An entry the object store cannot identify is corruption, and corruption
/// exits 2 so it is distinguishable from "there is garbage to collect" (1).
#[test]
fn fsck_reports_an_unidentifiable_object_and_exits_two() {
    let tmp = TempDir::new().unwrap();
    run_gfs(tmp.path(), &["init", "."]);

    let shard = tmp.path().join(".gfs/objects/ab");
    std::fs::create_dir_all(&shard).unwrap();
    std::fs::write(shard.join("c".repeat(62)), b"\xff\xfe not an object").unwrap();

    let (code, stdout, stderr) = run_gfs(tmp.path(), &["fsck", "--json"]);
    assert_stderr_empty(&stderr);
    let v = assert_stdout_json(&stdout);

    assert_eq!(code, 2, "corruption must exit 2, got {code}");
    assert_eq!(v["exit_code"], 2);
    assert_eq!(v["fsck"]["unrecognised"].as_array().unwrap().len(), 1);
}

/// A plan is the prelude to a deletion, so it must not be produced for a
/// repository that is already inconsistent.
#[test]
fn fsck_refuses_to_write_a_plan_for_an_inconsistent_repo() {
    let tmp = TempDir::new().unwrap();
    run_gfs(tmp.path(), &["init", "."]);

    let shard = tmp.path().join(".gfs/objects/ab");
    std::fs::create_dir_all(&shard).unwrap();
    std::fs::write(shard.join("c".repeat(62)), b"\xff\xfe not an object").unwrap();

    let (code, _, _) = run_gfs(tmp.path(), &["fsck", "--plan"]);
    assert_ne!(code, 0, "refusal must not report success");
    assert!(
        !tmp.path().join(".gfs/gc").exists(),
        "no plan directory should have been created"
    );
}

/// On a clean repository the plan is written, and it is the only thing fsck
/// ever creates.
#[test]
fn fsck_plan_writes_exactly_one_artefact() {
    let tmp = TempDir::new().unwrap();
    run_gfs(tmp.path(), &["init", "."]);

    let (code, _, _) = run_gfs(tmp.path(), &["fsck", "--plan"]);
    assert_eq!(code, 0);

    let gc = tmp.path().join(".gfs/gc");
    let runs: Vec<_> = std::fs::read_dir(&gc).unwrap().flatten().collect();
    assert_eq!(runs.len(), 1, "expected one mark directory");
    let plan = runs[0].path().join("plan.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    assert!(v.get("mark_id").is_some(), "plan must carry its mark id");
    assert!(v.get("cutoff").is_some(), "plan must carry its cutoff");
    assert!(v.get("report").is_some(), "plan must carry the marked set");
}
