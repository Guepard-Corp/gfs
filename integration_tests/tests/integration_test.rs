//! Integration tests for the gfs CLI.
//!
//! Uses simple temp directories only (no diskutil or storage backends).

use std::path::PathBuf;
use std::process::Command;

use gfs_domain::repo_utils::repo_layout::validate_repo_layout;
use tempfile::tempdir;

/// Workspace root (parent of integration_tests).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// Checks that `gfs init <path>` creates a valid `.gfs` directory layout.
#[test]
fn gfs_init_creates_valid_repo_layout() {
    let temp_dir = tempdir().expect("create temp dir");
    let work_dir = temp_dir.path().to_path_buf();

    // Run gfs init (one-shot, simple folders only)
    let status = Command::new("cargo")
        .args([
            "run",
            "--package",
            "gfs-cli",
            "--bin",
            "gfs",
            "init",
            work_dir.to_str().unwrap(),
        ])
        .current_dir(workspace_root())
        .status()
        .expect("run gfs init");

    assert!(status.success(), "gfs init should exit 0");

    let gfs_path: PathBuf = work_dir.join(".gfs");
    assert!(gfs_path.exists(), ".gfs directory should exist");
    assert!(gfs_path.is_dir(), ".gfs should be a directory");

    assert!(
        validate_repo_layout(&gfs_path).is_ok(),
        ".gfs layout should be valid"
    );
}
