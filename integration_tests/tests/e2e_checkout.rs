//! End-to-end tests for `gfs checkout`.
//!
//! Invoke the **CLI binary** via `std::process::Command`. Same pattern as
//! `e2e_commit.rs`: we run `cargo run --package gfs --bin gfs checkout ...`
//! and assert on the resulting filesystem (HEAD, WORKSPACE, workspace dir content).
//!
//! macOS-only: commit uses the APFS storage backend.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::tempdir;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn gfs_init(path: &Path) -> bool {
    let status = Command::new("cargo")
        .args([
            "run",
            "--package",
            "gfs-cli",
            "--bin",
            "gfs",
            "init",
            path.to_str().unwrap(),
        ])
        .current_dir(workspace_root())
        .status()
        .expect("run gfs init");
    status.success()
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

/// Run `gfs checkout <revision> [--path <path>]` via Command.
/// Returns (success, stdout, stderr).
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

    assert!(gfs_init(repo_path), "gfs init should succeed");

    let data_dir = workspace_data_dir_main_0(repo_path);
    fs::write(data_dir.join("seed.txt"), "data v1").unwrap();

    let (ok1, _, stderr1) = gfs_commit(repo_path, "commit 1", None, None);
    assert!(ok1, "first commit should succeed; stderr: {stderr1}");

    let hash1 = read_ref(repo_path, "main");
    assert_eq!(hash1.len(), 64);

    fs::write(data_dir.join("seed.txt"), "data v2").unwrap();
    let (ok2, _, stderr2) = gfs_commit(repo_path, "commit 2", None, None);
    assert!(ok2, "second commit should succeed; stderr: {stderr2}");

    let (checkout_ok, stdout, stderr) = gfs_checkout(repo_path, &hash1);
    assert!(checkout_ok, "gfs checkout <hash1> should succeed; stderr: {stderr}");
    assert!(
        stdout.contains("Switched to") && stdout.contains(&hash1[..7]),
        "stdout should show switched message; got: {stdout}"
    );

    let head = read_head(repo_path);
    assert_eq!(head, hash1, "HEAD should be detached at first commit");

    let workspace_path = read_workspace_path(repo_path);
    let short_hash1 = &hash1[..12.min(hash1.len())];
    assert!(
        workspace_path.to_string_lossy().contains("detached") && workspace_path.to_string_lossy().contains(short_hash1),
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

    assert!(gfs_init(repo_path), "gfs init should succeed");

    let data_dir = workspace_data_dir_main_0(repo_path);
    fs::write(data_dir.join("file.txt"), "first").unwrap();
    let (ok1, _, _) = gfs_commit(repo_path, "first", None, None);
    assert!(ok1);

    fs::write(data_dir.join("file.txt"), "second").unwrap();
    let (ok2, _, _) = gfs_commit(repo_path, "second", None, None);
    assert!(ok2);

    let (checkout_ok, _stdout, stderr) = gfs_checkout(repo_path, "main");
    assert!(checkout_ok, "gfs checkout main should succeed; stderr: {stderr}");

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

    assert!(gfs_init(repo_path), "gfs init should succeed");

    let (ok, _stdout, stderr) = gfs_checkout(
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

    assert!(gfs_init(repo_path), "gfs init should succeed");

    let (ok, _stdout, stderr) = gfs_checkout(repo_path, "0");
    assert!(!ok, "checkout 0 should fail");
    assert!(
        stderr.contains("no commits") || stderr.contains("0"),
        "stderr should mention no commits or 0; got: {stderr}"
    );
}
