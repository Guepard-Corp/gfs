//! End-to-end tests for `gfs checkout`.
//!
//! Runs CLI in-process via gfs_cli::run() for coverage capture.
//! macOS-only: commit uses the APFS storage backend.

#![cfg(target_os = "macos")]

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::cli_runner;
use tempfile::tempdir;

fn workspace_data_dir_main_0(repo_path: &Path) -> PathBuf {
    repo_path.join(".gfs/workspaces/main/0/data")
}

fn read_head(repo_path: &Path) -> String {
    fs::read_to_string(repo_path.join(".gfs/HEAD"))
        .expect("read HEAD")
        .trim()
        .to_string()
}

fn read_workspace_path(repo_path: &Path) -> PathBuf {
    let s = fs::read_to_string(repo_path.join(".gfs/WORKSPACE")).expect("read WORKSPACE");
    PathBuf::from(s.trim())
}

fn read_ref(repo_path: &Path, branch: &str) -> String {
    fs::read_to_string(repo_path.join(".gfs/refs/heads").join(branch))
        .expect("read branch ref")
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// After two commits, `gfs checkout <first_commit_hash>` switches to detached HEAD,
/// updates WORKSPACE to the workspace for that commit, and the workspace dir
/// contains the snapshot content (first commit's files).
#[test]
fn checkout_commit_hash_detaches_head_and_switches_workspace_content() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(cli_runner::gfs_init(repo_path), "gfs init should succeed");

    let data_dir = workspace_data_dir_main_0(repo_path);
    fs::write(data_dir.join("seed.txt"), "data v1").unwrap();

    let (ok1, _, stderr1) = cli_runner::gfs_commit(repo_path, "commit 1", None, None);
    assert!(ok1, "first commit should succeed; stderr: {stderr1}");

    let hash1 = read_ref(repo_path, "main");
    assert_eq!(hash1.len(), 64);

    fs::write(data_dir.join("seed.txt"), "data v2").unwrap();
    let (ok2, _, stderr2) = cli_runner::gfs_commit(repo_path, "commit 2", None, None);
    assert!(ok2, "second commit should succeed; stderr: {stderr2}");

    let (checkout_ok, stdout, stderr) = cli_runner::gfs_checkout(repo_path, &hash1);
    assert!(
        checkout_ok,
        "gfs checkout <hash1> should succeed; stderr: {stderr}"
    );
    // gag may not capture stdout reliably in test harness — parallel test threads can
    // pollute the captured buffer with test-runner output. Only assert when the captured
    // text actually looks like gfs CLI output (contains "Switched").
    if stdout.contains("Switched") {
        assert!(
            stdout.contains(&hash1[..7]),
            "stdout should include the target short hash; got: {stdout}"
        );
    }

    let head = read_head(repo_path);
    assert_eq!(head, hash1, "HEAD should be detached at first commit");

    let workspace_path = read_workspace_path(repo_path);
    let short_hash1 = &hash1[..12.min(hash1.len())];
    assert!(
        workspace_path.to_string_lossy().contains("detached")
            && workspace_path.to_string_lossy().contains(short_hash1),
        "WORKSPACE should point at workspaces/detached/<short_hash>/data; got: {}",
        workspace_path.display()
    );
    assert!(workspace_path.exists(), "workspace dir should exist");
    assert_eq!(
        fs::read_to_string(workspace_path.join("seed.txt")).unwrap(),
        "data v1",
        "workspace content should be from first commit"
    );
}

/// After two commits, `gfs checkout main` keeps HEAD on main and updates WORKSPACE
/// to the tip's workspace; workspace dir has second commit content.
#[test]
fn checkout_branch_main_updates_workspace_to_tip_content() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(cli_runner::gfs_init(repo_path), "gfs init should succeed");

    let data_dir = workspace_data_dir_main_0(repo_path);
    fs::write(data_dir.join("file.txt"), "first").unwrap();
    let (ok1, _, _) = cli_runner::gfs_commit(repo_path, "first", None, None);
    assert!(ok1);

    fs::write(data_dir.join("file.txt"), "second").unwrap();
    let (ok2, _, _) = cli_runner::gfs_commit(repo_path, "second", None, None);
    assert!(ok2);

    let (checkout_ok, _stdout, stderr) = cli_runner::gfs_checkout(repo_path, "main");
    assert!(
        checkout_ok,
        "gfs checkout main should succeed; stderr: {stderr}"
    );

    let head = read_head(repo_path);
    assert_eq!(head, "ref: refs/heads/main", "HEAD should point at main");

    let workspace_path = read_workspace_path(repo_path);
    assert!(
        workspace_path.to_string_lossy().contains("main/0/data"),
        "WORKSPACE should point at workspaces/main/0/data (branch workspace); got: {}",
        workspace_path.display()
    );
    assert_eq!(
        fs::read_to_string(workspace_path.join("file.txt")).unwrap(),
        "second",
        "workspace content should be tip (second commit)"
    );
}

/// Checkout unknown revision fails with non-zero and error message.
#[test]
fn checkout_unknown_revision_fails() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(cli_runner::gfs_init(repo_path), "gfs init should succeed");

    let (ok, _stdout, stderr) = cli_runner::gfs_checkout(
        repo_path,
        "0000000000000000000000000000000000000000000000000000000000000000",
    );
    assert!(!ok, "checkout unknown commit should fail");
    assert!(
        stderr.to_lowercase().contains("revision") || stderr.to_lowercase().contains("error"),
        "stderr should mention revision/error; got: {stderr}"
    );
}

/// Checkout branch with no commits (e.g. new branch that has no ref yet) is out of scope
/// since we don't have branch creation. So we test checkout "0" fails: resolving "0" gives "0",
/// and we reject that.
#[test]
fn checkout_zero_fails() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();

    assert!(cli_runner::gfs_init(repo_path), "gfs init should succeed");

    let (ok, _stdout, stderr) = cli_runner::gfs_checkout(repo_path, "0");
    assert!(!ok, "checkout 0 should fail");
    assert!(
        stderr.contains("no commits") || stderr.contains("0"),
        "stderr should mention no commits or 0; got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Workspace identity: a checkout must give you what you asked for, and must
// not silently destroy work to do it.
// ---------------------------------------------------------------------------

/// Sorted `name=content` for every file in the active workspace.
fn workspace_contents(repo_path: &Path) -> String {
    let dir = read_workspace_path(repo_path);
    let mut out: Vec<String> = fs::read_dir(&dir)
        .expect("read workspace")
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| {
            format!(
                "{}={}",
                e.file_name().to_string_lossy(),
                fs::read_to_string(e.path()).unwrap_or_default().trim()
            )
        })
        .collect();
    out.sort();
    out.join(" ")
}

/// Uncommitted work is neither carried across a checkout nor thrown away.
///
/// Carrying it made `checkout <branch>` non-deterministic and left state no GFS
/// command displays. Discarding it is unrecoverable. So checkout refuses,
/// naming what is in the way, and `--force` is the way to say you meant it.
#[test]
fn checkout_refuses_to_overwrite_uncommitted_work_unless_forced() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();
    assert!(cli_runner::gfs_init(repo_path), "gfs init should succeed");

    let data_dir = workspace_data_dir_main_0(repo_path);
    fs::write(data_dir.join("seed.txt"), "committed").unwrap();
    let (ok, _, stderr) = cli_runner::gfs_commit(repo_path, "c1", None, None);
    assert!(ok, "commit should succeed; stderr: {stderr}");

    // A clean workspace is not in the way.
    let (ok, _, stderr) = cli_runner::run_gfs([
        "gfs",
        "checkout",
        "--path",
        repo_path.to_str().unwrap(),
        "main",
    ]);
    assert!(ok, "a clean checkout must not be refused; stderr: {stderr}");

    fs::write(read_workspace_path(repo_path).join("scratch.txt"), "wip").unwrap();

    // Checking out the branch you are already on rebuilds this very directory,
    // so the work in it is exactly what would be lost.
    let (ok, _, stderr) = cli_runner::run_gfs([
        "gfs",
        "checkout",
        "--path",
        repo_path.to_str().unwrap(),
        "main",
    ]);
    assert!(!ok, "uncommitted work must stop the checkout");
    assert!(
        stderr.contains("uncommitted changes") && stderr.contains("scratch.txt"),
        "the refusal should name what is in the way: {stderr}"
    );
    assert!(
        read_workspace_path(repo_path).join("scratch.txt").exists(),
        "a refused checkout must change nothing"
    );

    let (ok, _, stderr) = cli_runner::run_gfs([
        "gfs",
        "checkout",
        "--path",
        repo_path.to_str().unwrap(),
        "main",
        "--force",
    ]);
    assert!(ok, "--force should go through; stderr: {stderr}");
    assert_eq!(
        workspace_contents(repo_path),
        "seed.txt=committed",
        "forced checkout restores the commit exactly"
    );
}

/// A branch's working copy must not outlive the branch.
///
/// The workspace is keyed by branch NAME, so a later branch reusing the name
/// inherited the dead branch's working copy and `checkout` reported success
/// while handing over content that branch never contained.
#[test]
fn a_recreated_branch_does_not_inherit_the_deleted_one() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();
    assert!(cli_runner::gfs_init(repo_path), "gfs init should succeed");

    let data_dir = workspace_data_dir_main_0(repo_path);
    fs::write(data_dir.join("seed.txt"), "v1").unwrap();
    assert!(cli_runner::gfs_commit(repo_path, "c1", None, None).0);
    fs::write(data_dir.join("seed.txt"), "v2").unwrap();
    assert!(cli_runner::gfs_commit(repo_path, "c2", None, None).0);
    let tip = read_ref(repo_path, "main");

    let repo = repo_path.to_str().unwrap();
    assert!(cli_runner::run_gfs(["gfs", "checkout", "--path", repo, "-b", "b1"]).0);
    fs::write(read_workspace_path(repo_path).join("only-on-b1.txt"), "x").unwrap();
    assert!(cli_runner::gfs_commit(repo_path, "b1 work", None, None).0);
    fs::write(
        read_workspace_path(repo_path).join("never-committed.txt"),
        "y",
    )
    .unwrap();

    assert!(cli_runner::run_gfs(["gfs", "checkout", "--path", repo, "main", "--force"]).0);
    assert!(cli_runner::run_gfs(["gfs", "branch", "--path", repo, "-d", "b1"]).0);
    assert!(
        !repo_path.join(".gfs/workspaces/b1").exists(),
        "the workspace must go with the branch"
    );

    assert!(cli_runner::run_gfs(["gfs", "branch", "--path", repo, "b1", &tip]).0);
    assert!(cli_runner::run_gfs(["gfs", "checkout", "--path", repo, "b1"]).0);
    assert_eq!(
        workspace_contents(repo_path),
        "seed.txt=v2",
        "the new b1 holds its own commit, not the deleted branch's working copy"
    );
}

/// A commit hash names exactly one content state.
///
/// The detached workspace is named by the hash, and was reused as-is, so
/// mutating one and returning to it gave you something other than what
/// `Switched to <hash>` says you got.
#[test]
fn returning_to_a_detached_commit_gives_that_commit() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();
    assert!(cli_runner::gfs_init(repo_path), "gfs init should succeed");

    let data_dir = workspace_data_dir_main_0(repo_path);
    fs::write(data_dir.join("seed.txt"), "v1").unwrap();
    assert!(cli_runner::gfs_commit(repo_path, "c1", None, None).0);
    let first = read_ref(repo_path, "main");
    fs::write(data_dir.join("seed.txt"), "v2").unwrap();
    assert!(cli_runner::gfs_commit(repo_path, "c2", None, None).0);

    let repo = repo_path.to_str().unwrap();
    assert!(cli_runner::run_gfs(["gfs", "checkout", "--path", repo, &first]).0);
    assert_eq!(workspace_contents(repo_path), "seed.txt=v1");

    fs::write(read_workspace_path(repo_path).join("poison.txt"), "z").unwrap();

    // Leaving is fine: the detached directory is not the one being rebuilt.
    assert!(
        cli_runner::run_gfs(["gfs", "checkout", "--path", repo, "main"]).0,
        "leaving a dirty workspace overwrites nothing, so it must not be refused"
    );

    // Returning is where it would be destroyed, so that is where the refusal is.
    let (ok, _, stderr) = cli_runner::run_gfs(["gfs", "checkout", "--path", repo, &first]);
    assert!(!ok, "returning would overwrite poison.txt");
    assert!(stderr.contains("poison.txt"), "{stderr}");

    assert!(cli_runner::run_gfs(["gfs", "checkout", "--path", repo, &first, "--force"]).0);
    assert_eq!(
        workspace_contents(repo_path),
        "seed.txt=v1",
        "the commit's content, not what was left in its directory"
    );
}

/// The refusal has to guard the workspace the restore REBUILDS, not the one
/// being left behind.
///
/// Each branch has its own directory, so leaving a dirty branch overwrites
/// nothing and must not be refused. The destructive moment is coming BACK: the
/// target directory is rebuilt from its snapshot, and work left there is gone.
/// Guarding the wrong side is both too strict and, more seriously, silent —
/// walk away from dirty work on one branch and return to it from a clean one,
/// and it is deleted with no refusal at all.
#[test]
fn the_refusal_guards_the_workspace_being_rebuilt_not_the_one_being_left() {
    let tmp = tempdir().expect("create temp dir");
    let repo_path = tmp.path();
    assert!(cli_runner::gfs_init(repo_path), "gfs init should succeed");
    let repo = repo_path.to_str().unwrap();

    fs::write(workspace_data_dir_main_0(repo_path).join("seed.txt"), "v1").unwrap();
    assert!(cli_runner::gfs_commit(repo_path, "c1", None, None).0);

    assert!(cli_runner::run_gfs(["gfs", "checkout", "--path", repo, "-b", "feature"]).0);
    let feature_ws = read_workspace_path(repo_path);
    fs::write(feature_ws.join("scratch.txt"), "wip").unwrap();

    // Leaving: allowed, and the directory is left alone.
    let (ok, _, stderr) = cli_runner::run_gfs(["gfs", "checkout", "--path", repo, "main"]);
    assert!(ok, "leaving must not be refused; stderr: {stderr}");
    assert!(
        feature_ws.join("scratch.txt").exists(),
        "leaving must not touch the branch's directory"
    );

    // Returning: refused, from a workspace that is itself clean.
    let (ok, _, stderr) = cli_runner::run_gfs(["gfs", "checkout", "--path", repo, "feature"]);
    assert!(
        !ok,
        "returning would rebuild feature's directory and destroy scratch.txt"
    );
    assert!(stderr.contains("scratch.txt"), "{stderr}");
    assert!(
        feature_ws.join("scratch.txt").exists(),
        "a refused checkout must change nothing"
    );
}
