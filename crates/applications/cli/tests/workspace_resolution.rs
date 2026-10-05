//! `.gfs/WORKSPACE` must be resolved against the repository, not the process
//! working directory.
//!
//! `gfs init .` records a relative path while `checkout` records an absolute
//! one, so a caller that locates the repository without changing into it — which
//! `--path` does, and which a long-lived MCP server does on every request — used
//! to get a path that does not exist. `commit` failed in the storage adapter
//! with `cp -cRp './.gfs/workspaces/main/0/data' ... No such file or directory`.
//!
//! No container runtime is needed: the defect reproduces with no database
//! provider at all, which is why these run alongside the hermetic tests rather
//! than the `e2e_*` suites.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn gfs_bin() -> &'static str {
    env!("CARGO_BIN_EXE_gfs")
}

fn run_gfs(cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(gfs_bin())
        .current_dir(cwd)
        .args(args)
        .env("RUST_LOG", "off")
        .env("NO_COLOR", "1")
        .env("GFS_NO_TELEMETRY", "1")
        .output()
        .expect("failed to run gfs");
    let code = out.status.code().unwrap_or(1);
    (
        code,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Seed a workspace file so `commit` has something to snapshot, and return the
/// recorded workspace so a test can assert on its form.
fn seed(repo: &Path) -> String {
    let recorded =
        std::fs::read_to_string(repo.join(".gfs/WORKSPACE")).expect("init must record a workspace");
    let data = repo.join(recorded.trim());
    std::fs::create_dir_all(&data).expect("workspace data dir");
    std::fs::write(data.join("seed.txt"), b"seed").expect("seed file");
    recorded.trim().to_owned()
}

#[test]
fn commit_with_path_succeeds_from_an_unrelated_working_directory() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert_eq!(run_gfs(&repo, &["init", "."]).0, 0, "init .");

    // The precondition this test exists for. `init .` records a RELATIVE path,
    // and if that ever changes this test would still pass while exercising a
    // repository shape that was never broken — so assert it rather than assume.
    let recorded = seed(&repo);
    assert!(
        recorded.starts_with("./"),
        "`init .` is expected to record a relative workspace, got {recorded:?}; \
         this test no longer covers the case it was written for"
    );

    // Somewhere that is emphatically not the repository.
    let elsewhere = tmp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();

    let (code, stdout, stderr) = run_gfs(
        &elsewhere,
        &[
            "commit",
            "-m",
            "from elsewhere",
            "--path",
            repo.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "commit --path failed: {stderr}{stdout}");
    assert!(
        !stderr.contains("No such file or directory"),
        "the recorded workspace was resolved against the cwd: {stderr}"
    );
}

#[test]
fn commit_from_the_repository_root_is_unaffected() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert_eq!(run_gfs(&repo, &["init", "."]).0, 0, "init .");
    seed(&repo);

    let (code, stdout, stderr) = run_gfs(&repo, &["commit", "-m", "from root"]);
    assert_eq!(code, 0, "commit from the root failed: {stderr}{stdout}");
}

/// The form `checkout` writes must keep working, since `Path::join` is only
/// correct here because it returns an absolute argument unchanged.
#[test]
fn an_absolute_recorded_workspace_still_commits_from_elsewhere() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert_eq!(run_gfs(&repo, &["init", "."]).0, 0, "init .");
    let recorded = seed(&repo);

    let absolute = repo.join(recorded);
    std::fs::write(
        repo.join(".gfs/WORKSPACE"),
        absolute.to_string_lossy().as_ref(),
    )
    .unwrap();

    let elsewhere = tmp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let (code, stdout, stderr) = run_gfs(
        &elsewhere,
        &["commit", "-m", "absolute", "--path", repo.to_str().unwrap()],
    );
    assert_eq!(code, 0, "absolute workspace regressed: {stderr}{stdout}");
}
