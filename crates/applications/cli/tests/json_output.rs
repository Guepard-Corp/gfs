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
    // 3, not 1: a rejected argument list means the command did not run, and 1 is
    // reserved for a command that ran and reports a non-success outcome. This
    // assertion is incidental to what the test is about -- announcing the error
    // once -- but it pins the code, so it is corrected rather than dropped.
    assert_eq!(code, 3, "a usage error could not run, so it exits 3");
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
        // Also 3. These are clap's DisplayHelpOnMissingArgumentOrSubcommand: the
        // invocation was incomplete, so the command never ran, and the assertion
        // below confirms they do announce an error rather than merely offering
        // help. Bare `gfs` therefore exits 3 as well, which is the one visibly
        // unusual consequence of this scheme -- the alternative, mapping it back to
        // 1, would return it to collision with "the command ran and found
        // something".
        assert_eq!(
            code,
            3,
            "`gfs {}` did not run, so it exits 3",
            bare.join(" ")
        );
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

/// Cases that must see freshly made garbage pass `--grace 0` with
/// `--disable-grace-period-check`, because the default holds back anything
/// written in the last 24 hours, which in a test is everything. Cases whose
/// outcome does not depend on the window deliberately do NOT, so the one-hour
/// floor is exercised rather than bypassed everywhere.
///
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

    let (code, stdout, stderr) = run_gfs(
        tmp.path(),
        &[
            "fsck",
            "--grace",
            "0",
            "--disable-grace-period-check",
            "--json",
        ],
    );
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

/// A reader that closes early must not decide the verdict.
///
/// `println_safe!` used to `exit(0)` on the first BrokenPipe, so
/// `gfs fsck --json | head -1` reported success on a corrupt repository, and
/// `--json` through a bare `println!` panicked out at 101 -- a code outside the
/// documented scheme entirely.
#[test]
fn a_closed_reader_does_not_replace_the_exit_status() {
    use std::process::{Command, Stdio};

    let tmp = TempDir::new().unwrap();
    assert_eq!(run_gfs(tmp.path(), &["init", "."]).0, 0, "init");
    // An entry the object store cannot identify: corruption, which exits 2.
    let shard = tmp.path().join(".gfs/objects/zz");
    std::fs::create_dir_all(&shard).unwrap();
    std::fs::write(shard.join("garbage"), b"not an object").unwrap();

    // The precondition. Without it a wrong exit status could look correct.
    let (plain, _, _) = run_gfs(
        tmp.path(),
        &["fsck", "--grace", "0", "--disable-grace-period-check"],
    );
    assert_eq!(plain, 2, "precondition: this repository must be corrupt");

    for args in [
        vec!["fsck", "--grace", "0", "--disable-grace-period-check"],
        vec![
            "fsck",
            "--grace",
            "0",
            "--disable-grace-period-check",
            "--json",
        ],
    ] {
        let mut child = Command::new(gfs_bin())
            .current_dir(tmp.path())
            .args(&args)
            .env("RUST_LOG", "off")
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn gfs");
        // Close the read end immediately: the next write gets BrokenPipe.
        drop(child.stdout.take());
        let status = child.wait().expect("wait");
        assert_eq!(
            status.code(),
            Some(2),
            "a closed reader must not change the verdict for {args:?}"
        );
    }
}

/// A usage error must not borrow the code that means "the command ran and found
/// something". `gfs fsck --typo` exited 1, which fsck documents as "unreachable
/// objects found -- a collector would have work to do", so a script branching on
/// 1 to run a collector was triggered by a mistyped flag.
///
/// The help cases are the control: they travel the same clap error path and must
/// stay 0, or this fix would have broken `--help` for every command.
#[test]
fn a_usage_error_does_not_share_the_exit_code_of_a_finding() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(run_gfs(tmp.path(), &["init", "."]).0, 0, "init");

    for args in [
        vec!["fsck", "--no-such-flag"],
        vec!["fsck", "--grace", "notanumber"],
        vec!["commit", "--no-such-flag"],
        vec!["no-such-command"],
    ] {
        assert_eq!(
            run_gfs(tmp.path(), &args).0,
            3,
            "a usage error could not run, so it is 3: {args:?}"
        );
    }

    for args in [vec!["--help"], vec!["--version"], vec!["fsck", "--help"]] {
        assert_eq!(
            run_gfs(tmp.path(), &args).0,
            0,
            "a request is not an error: {args:?}"
        );
    }

    // And an error from a command that DID run keeps 1, so this change did not
    // quietly redefine the code for everything that fails.
    assert_eq!(
        run_gfs(tmp.path(), &["checkout", "no-such-branch"]).0,
        1,
        "it ran and failed, which is not the same as not running"
    );
}

/// Three messages that described something other than what was wrong.
///
/// The config warning fired wherever the config could not be read -- including
/// where there was no repository to have one, so an empty directory printed
/// "could not read the repository config" and then, one line later, "not a GFS
/// repository". A `.gfs` that was a regular file fell through to an error saying
/// no repository was found "in <dir> or any parent directory", when the thing was
/// right there as a file and no command searches parents at all.
#[test]
fn a_directory_with_no_repository_does_not_warn_about_a_config_it_could_not_have() {
    let tmp = TempDir::new().unwrap();
    let (_, _, stderr) = run_gfs(tmp.path(), &["fsck"]);
    assert!(
        !stderr.contains("could not read the repository config"),
        "no repository here, so there is no config to warn about: {stderr}"
    );
    assert!(
        stderr.contains("not a GFS repository"),
        "it must still say what is wrong: {stderr}"
    );
    assert!(
        !stderr.contains("parent directory"),
        "no gfs command searches parents, so it must not claim it did: {stderr}"
    );
}

/// And the case that used to produce the parent-directory claim: `.gfs` present,
/// but as a file.
#[test]
fn a_gfs_that_is_a_file_says_that_rather_than_that_nothing_was_found() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join(".gfs"), b"not a directory").unwrap();
    let (_, _, stderr) = run_gfs(tmp.path(), &["fsck"]);
    assert!(
        stderr.contains("is not a directory"),
        "the real problem is that .gfs is a file: {stderr}"
    );
    assert!(
        !stderr.contains("could not read the repository config"),
        "one problem, one message: {stderr}"
    );
}

/// The control for both: a REAL repository whose config will not parse is exactly
/// what that warning is for, and it must still fire. Without this the two above
/// would pass just as well if the warning had been deleted.
#[test]
fn a_real_repository_with_an_unreadable_config_still_warns() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(run_gfs(tmp.path(), &["init", "."]).0, 0, "init");
    std::fs::write(tmp.path().join(".gfs/config.toml"), b"garbage {{{").unwrap();
    let (_, _, stderr) = run_gfs(tmp.path(), &["fsck"]);
    assert!(
        stderr.contains("could not read the repository config"),
        "this is the case the warning exists for: {stderr}"
    );
}

/// The output layer is shared, so the closed-reader fix has to hold outside
/// fsck too -- and measured on this branch it did not: `gfs log --json` with the
/// reader gone exited 101, a panic, which is outside every exit code this CLI
/// documents. `gfs log --json | head -1` is an ordinary thing to type.
///
/// This covers ONE non-fsck command. `status` (48 bare `println!`) and `branch`
/// (19) still panic the same way; that sweep is tracked separately, and asks for it in
/// one pass rather than piecemeal.
#[test]
fn a_closed_reader_does_not_panic_a_non_fsck_command() {
    use std::process::{Command, Stdio};

    let tmp = TempDir::new().unwrap();
    assert_eq!(run_gfs(tmp.path(), &["init", "."]).0, 0, "init");
    let data = tmp.path().join(".gfs/workspaces/main/0/data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("f"), b"x").unwrap();
    assert_eq!(run_gfs(tmp.path(), &["commit", "-m", "one"]).0, 0, "commit");

    // The precondition: with a reader present this prints JSON and exits 0, so a
    // 0 below means "survived the closed reader", not "had nothing to say".
    let (code, stdout, _) = run_gfs(tmp.path(), &["log", "--json"]);
    assert_eq!(code, 0, "precondition: log --json must succeed");
    assert!(
        stdout.contains("\"commits\""),
        "precondition: it must actually write to stdout, got {stdout:?}"
    );

    let mut child = Command::new(gfs_bin())
        .current_dir(tmp.path())
        .args(["log", "--json"])
        .env("RUST_LOG", "off")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn gfs");
    drop(child.stdout.take());
    let status = child.wait().expect("wait");
    assert_ne!(
        status.code(),
        Some(101),
        "a closed reader must not panic: 101 is a crash, not a verdict"
    );
    assert_eq!(
        status.code(),
        Some(0),
        "and the real exit status must survive"
    );
}

/// RFC 009 D4 requires a floor in argument parsing and zero only behind a flag
/// that names what it disables. Without the floor a mistyped window silently
/// reports an in-flight commit as garbage -- and with `--plan`, persists that
/// judgement for a collector.
#[test]
fn a_grace_below_the_floor_is_refused_without_the_override() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(run_gfs(tmp.path(), &["init", "."]).0, 0, "init");

    for window in ["0", "1", "3599"] {
        let (code, stdout, stderr) = run_gfs(tmp.path(), &["fsck", "--grace", window]);
        // Was `assert_ne!(code, 0)`, which passes for 1, 2 or 3 and so could not
        // see that this refusal was exiting 1 -- the same code as "the repository
        // has collectable objects". Pinned now.
        assert_eq!(
            code, 3,
            "a grace of {window}s must not be accepted: {stdout}"
        );
        assert!(
            stderr.contains("below the") && stderr.contains("floor"),
            "the refusal must name the floor, got: {stderr}"
        );
        assert!(
            stderr.contains("--disable-grace-period-check"),
            "the refusal must name the override, got: {stderr}"
        );
    }

    // The override is the documented way through, and it must work.
    let (code, _, stderr) = run_gfs(
        tmp.path(),
        &["fsck", "--grace", "0", "--disable-grace-period-check"],
    );
    assert_eq!(code, 0, "the override must be honoured: {stderr}");

    // And the floor itself is accepted without it.
    let (code, _, stderr) = run_gfs(tmp.path(), &["fsck", "--grace", "3600"]);
    assert_eq!(code, 0, "the floor itself must be accepted: {stderr}");
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

    let (code, _, _) = run_gfs(
        tmp.path(),
        &[
            "fsck",
            "--plan",
            "--grace",
            "0",
            "--disable-grace-period-check",
        ],
    );
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

    let (code, _, _) = run_gfs(
        tmp.path(),
        &[
            "fsck",
            "--plan",
            "--grace",
            "0",
            "--disable-grace-period-check",
        ],
    );
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

/// Runs gfs with raw argument bytes, and optionally a raw argv[0].
#[cfg(unix)]
fn run_gfs_raw(cwd: &Path, arg0: Option<&[u8]>, args: &[&[u8]]) -> (i32, String, String) {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;

    let mut command = Command::new(gfs_bin());
    if let Some(arg0) = arg0 {
        command.arg0(OsStr::from_bytes(arg0));
    }
    let out = command
        .current_dir(cwd)
        .args(args.iter().map(|arg| OsStr::from_bytes(arg)))
        .env("RUST_LOG", "off")
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run gfs");

    let code = out.status.code().unwrap_or(1);
    let stdout = String::from_utf8(out.stdout).expect("stdout must be utf-8");
    let stderr = String::from_utf8(out.stderr).expect("stderr must be utf-8");
    (code, stdout, stderr)
}

/// An argument that is not UTF-8 panicked inside `std::env::args()`: exit 101,
/// the code of any crash, with the undecodable bytes echoed in the panic message.
/// It is a usage error like any other value that will not parse, so it exits 3
/// and names the position, and never repeats the bytes back.
///
/// Unix only: Windows arguments are UTF-16 and cannot carry these bytes.
#[cfg(unix)]
#[test]
fn an_argument_that_is_not_utf8_is_a_usage_error_not_a_crash() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(run_gfs(tmp.path(), &["init", "."]).0, 0, "init");

    let (code, stdout, stderr) =
        run_gfs_raw(tmp.path(), None, &[b"commit", b"-m", b"echomarker\xe9"]);
    assert_eq!(code, 3, "a usage error, not a crash: {stderr}");
    assert!(stdout.is_empty(), "nothing on stdout: {stdout}");
    assert!(
        stderr.contains("argument 3 is not valid UTF-8"),
        "names the position: {stderr}"
    );
    assert!(!stderr.contains("panicked"), "no panic: {stderr}");
    assert!(
        !stderr.contains("echomarker"),
        "the argument is not echoed: {stderr}"
    );

    let (code, stdout, stderr) = run_gfs_raw(tmp.path(), None, &[b"--json", b"log", b"\xff\xfe"]);
    assert_eq!(code, 3, "the same code under --json: {stderr}");
    let v = assert_stdout_json(&stdout);
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("argument 3 is not valid UTF-8"),
        "the JSON error names the position: {v}"
    );

    // The control: argv[0] is the path the binary was launched by, not user
    // input, so a non-UTF-8 one must not turn every command into an error.
    let (code, _, stderr) = run_gfs_raw(tmp.path(), Some(b"gfs\xff"), &[b"--version"]);
    assert_eq!(code, 0, "argv[0] is not an argument: {stderr}");
}
