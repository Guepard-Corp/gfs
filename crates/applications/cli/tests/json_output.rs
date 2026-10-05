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

/// `--grace 0` throughout: the default holds back anything written in the last
/// 24 hours, which in a test is everything.
///
/// A freshly initialised repository has nothing unreachable and nothing broken,
/// so fsck must exit 0 — the code a script uses to decide there is no work.
#[test]
fn fsck_on_a_fresh_repo_is_clean_and_exits_zero() {
    let tmp = TempDir::new().unwrap();
    let (code, _, _) = run_gfs(tmp.path(), &["init", "."]);
    assert_eq!(code, 0, "init should succeed");

    let (code, stdout, stderr) = run_gfs(tmp.path(), &["fsck", "--grace", "0", "--json"]);
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

    let (code, stdout, stderr) = run_gfs(tmp.path(), &["fsck", "--grace", "0", "--json"]);
    assert_stderr_empty(&stderr);
    let v = assert_stdout_json(&stdout);

    assert_eq!(code, 2, "corruption must exit 2, got {code}");
    assert_eq!(v["exit_code"], 2);
    assert_eq!(v["fsck"]["unrecognised"].as_array().unwrap().len(), 1);
}

/// `--json` means every outcome is machine-readable, including a failure. This
/// path printed to stderr and left stdout empty, so a caller parsing the output
/// got an empty string rather than an error it could act on.
///
/// `status`, `log` and `schema show` all emit `{"error": {...}}` on the same
/// failure, so fsck was the one breaking the contract. It cannot reach the
/// handler in `main.rs` that builds that object, because it reports this outcome
/// as `Ok(3)` rather than an `Err`.
#[test]
fn fsck_json_emits_an_error_object_when_it_cannot_run() {
    let tmp = TempDir::new().unwrap(); // never initialised: not a repository

    let (code, stdout, _) = run_gfs(tmp.path(), &["fsck", "--json"]);
    assert_eq!(code, 3, "could-not-run, not clean and not garbage");

    let v = assert_stdout_json(&stdout);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("not a GFS repository"),
        "the message must say what is wrong: {v}"
    );
    assert!(
        v["error"]["details"].is_string(),
        "same shape as every other command's error: {v}"
    );
}

/// The counting test: build a repository whose garbage is known by construction,
/// then assert fsck finds that and nothing else.
///
/// Two branches, one commit each, both deleted. Each commit contributes exactly
/// three objects — the commit, its file list, its snapshot tree — so six is the
/// whole answer, and `main` plus its own three must survive. The four fsck tests
/// above never commit anything, so none of them exercises the walk on a real
/// graph; a miscount there is invisible to all of them.
///
/// Asserting the exact set rather than a lower bound is the point. Over-reporting
/// is the dangerous direction: everything fsck names is what a collector would
/// later delete, so a test that only checks "at least the garbage" would pass on
/// a walk that also condemned `main`.
#[test]
fn fsck_finds_exactly_the_garbage_two_deleted_branches_leave() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(run_gfs(tmp.path(), &["init", "."]).0, 0, "init");

    // `init .` records WORKSPACE relative to the repo root ("./.gfs/workspaces/..."),
    // so it must be resolved against that root and not against the test process's cwd.
    // `Path::join` returns the argument unchanged when it is absolute, so this is
    // correct for both forms.
    let workspace = |t: &std::path::Path| -> std::path::PathBuf {
        t.join(
            std::fs::read_to_string(t.join(".gfs/WORKSPACE"))
                .unwrap()
                .trim(),
        )
    };

    std::fs::write(workspace(tmp.path()).join("a.txt"), b"base").unwrap();
    assert_eq!(
        run_gfs(tmp.path(), &["commit", "-m", "base"]).0,
        0,
        "base commit"
    );

    // `branch -d` soft-deletes into `refs/deleted/`, and fsck roots every
    // soft-deleted ref regardless of age, so a deleted branch leaves no garbage
    // while it is still recoverable. Zero retention is what "really gone" means
    // to a user, and it is the only way this test can produce garbage without
    // reaching behind the product to unlink refs itself.
    assert_eq!(
        run_gfs(tmp.path(), &["config", "branch.deletedRetentionDays", "0"]).0,
        0,
        "set zero retention"
    );

    for b in ["g1", "g2"] {
        assert_eq!(
            run_gfs(tmp.path(), &["checkout", "-b", b]).0,
            0,
            "branch {b}"
        );
        std::fs::write(workspace(tmp.path()).join(format!("{b}.txt")), b).unwrap();
        assert_eq!(
            run_gfs(tmp.path(), &["commit", "-m", b]).0,
            0,
            "commit on {b}"
        );
    }
    assert_eq!(
        run_gfs(tmp.path(), &["checkout", "main"]).0,
        0,
        "back to main"
    );
    for b in ["g1", "g2"] {
        assert_eq!(run_gfs(tmp.path(), &["branch", "-d", b]).0, 0, "delete {b}");
    }

    // Expiry is enforced inside `branch -d` itself -- there is no collector and no
    // background process -- and it prunes with a strict `<` against now, so the
    // most recent delete cannot prune its own entry. One more delete flushes it.
    // `flush` carries no commit of its own, so its ref points at a commit `main`
    // already roots and it contributes nothing to the garbage counted below.
    assert_eq!(
        run_gfs(tmp.path(), &["checkout", "-b", "flush"]).0,
        0,
        "flush branch"
    );
    assert_eq!(
        run_gfs(tmp.path(), &["checkout", "main"]).0,
        0,
        "back to main again"
    );
    assert_eq!(
        run_gfs(tmp.path(), &["branch", "-d", "flush"]).0,
        0,
        "delete flush, pruning g1 and g2"
    );

    let (code, stdout, stderr) = run_gfs(tmp.path(), &["fsck", "--grace", "0", "--json"]);
    assert_stderr_empty(&stderr);
    let v = assert_stdout_json(&stdout);

    assert_eq!(code, 1, "garbage found exits 1, got {code}");
    assert_eq!(
        v["fsck"]["dangling"].as_array().unwrap().len(),
        0,
        "nothing is broken"
    );
    assert!(
        v["fsck"]["reachability_complete"].as_bool().unwrap(),
        "the walk was whole"
    );

    let mut kinds: Vec<&str> = v["fsck"]["unreachable"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["kind"].as_str().unwrap())
        .collect();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        [
            "commit",
            "commit",
            "file_list",
            "file_list",
            "snapshot",
            "snapshot"
        ],
        "exactly the two deleted branches, and main untouched"
    );

    assert!(
        v["fsck"]["referenced_bytes"].as_u64().unwrap() > 0,
        "the byte total must be reported, not left at zero"
    );
    assert_eq!(
        v["fsck"]["checked_commits"].as_u64().unwrap(),
        1,
        "only main is reachable"
    );
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

    let (code, _, _) = run_gfs(tmp.path(), &["fsck", "--plan", "--grace", "0"]);
    assert_eq!(
        code, 2,
        "the refusal is caused by corruption, so it must not collide with 1 (garbage found)"
    );
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

    let (code, _, _) = run_gfs(tmp.path(), &["fsck", "--plan", "--grace", "0"]);
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
