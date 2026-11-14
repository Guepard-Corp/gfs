//! In-process CLI runner for integration tests.
//!
//! Uses `gfs_cli::run()` instead of spawning a subprocess, so coverage is captured.
//! Captures stdout/stderr via the `gag` crate.
//!
//! Note: `gag` may not capture stdout reliably under the test harness. Use `run_gfs_subprocess`
//! for commands where stdout content must be verified (e.g. `schema diff`).

#![allow(dead_code)] // Helpers used by different test binaries

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

/// Global lock for stdout/stderr redirection. The `gag` crate's Redirect is process-global;
/// concurrent tests would otherwise get "Redirect already exists" when running in parallel.
static REDIRECT_LOCK: Mutex<()> = Mutex::new(());

/// Run gfs CLI in-process with the given args. Returns (success, stdout, stderr).
/// Args should be like ["gfs", "init", path] or ["gfs", "commit", "-m", "msg", "--path", path].
pub fn run_gfs<I, S>(args: I) -> (bool, String, String)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();

    #[cfg(unix)]
    {
        let _guard = REDIRECT_LOCK.lock().expect("redirect lock");

        let stdout_file = tempfile::NamedTempFile::new().expect("temp stdout");
        let stderr_file = tempfile::NamedTempFile::new().expect("temp stderr");
        let stdout_path = stdout_file.path().to_path_buf();
        let stderr_path = stderr_file.path().to_path_buf();

        let _stdout_redirect =
            gag::Redirect::stdout(std::fs::File::create(&stdout_path).expect("create stdout file"))
                .expect("redirect stdout");
        let _stderr_redirect =
            gag::Redirect::stderr(std::fs::File::create(&stderr_path).expect("create stderr file"))
                .expect("redirect stderr");

        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let result = rt.block_on(gfs_cli::run(args.iter().map(|s| s.as_str())));

        drop(_stdout_redirect);
        drop(_stderr_redirect);

        let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
        let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();

        let (ok, stderr) = match result {
            Ok(_) => (true, stderr),
            Err(e) => (false, format!("{stderr}{e:#}")),
        };
        (ok, stdout, stderr)
    }

    #[cfg(not(unix))]
    {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let result = rt.block_on(gfs_cli::run(args.iter().map(|s| s.as_str())));
        let (ok, stderr) = match &result {
            Ok(_) => (true, String::new()),
            Err(e) => (false, format!("{e:#}")),
        };
        (ok, String::new(), stderr)
    }
}

/// Run gfs CLI as a subprocess. Returns (success, stdout, stderr).
/// Use when stdout content must be verified; gag may not capture reliably in the test harness.
/// Success means the process exited normally (exit codes 0, 1, 2 are all considered success).
pub fn run_gfs_subprocess<I, S>(args: I) -> (bool, String, String)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
    let output = Command::new(env!("CARGO_BIN_EXE_gfs"))
        .args(&args[1..]) // skip "gfs" prefix
        .output()
        .expect("failed to execute gfs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    // Consider success if process exited (schema diff uses 0/1/2; only panic/signal = None)
    let ok = output.status.code().is_some();
    (ok, stdout, stderr)
}

/// Run gfs CLI as a subprocess where success means exit status 0.
/// For commands whose stdout is asserted on: `run_gfs` captures the whole
/// process's stdout, so libtest's "has been running for over 60 seconds" notice
/// (or another thread's output) printed meanwhile becomes the command's stdout.
pub fn run_gfs_subprocess_checked<I, S>(args: I) -> (bool, String, String)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
    let output = Command::new(env!("CARGO_BIN_EXE_gfs"))
        .args(&args[1..]) // skip "gfs" prefix
        .output()
        .expect("failed to execute gfs");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (output.status.success(), stdout, stderr)
}

/// Convenience: gfs init <path>
pub fn gfs_init(path: &Path) -> bool {
    let (ok, _, _) = run_gfs(["gfs", "init", path.to_str().unwrap()]);
    ok
}

/// Convenience: gfs init --database-provider postgres --database-version 17 <path>
pub fn gfs_init_with_db(path: &Path) -> bool {
    gfs_init_with_provider(path, "postgres", "17")
}

/// Convenience: gfs init --database-provider <provider> --database-version <version> <path>
pub fn gfs_init_with_provider(path: &Path, provider: &str, version: &str) -> bool {
    let (ok, stdout, stderr) = run_gfs([
        "gfs",
        "init",
        "--database-provider",
        provider,
        "--database-version",
        version,
        path.to_str().unwrap(),
    ]);
    if !ok {
        eprintln!("gfs init failed:\nstdout: {}\nstderr: {}", stdout, stderr);
    }
    ok
}

/// Convenience: gfs config --path <path> <key> [value]
pub fn gfs_config(path: &Path, key: &str, value: Option<&str>) -> (bool, String, String) {
    let args: Vec<&str> = match value {
        Some(v) => vec!["gfs", "config", "--path", path.to_str().unwrap(), key, v],
        None => vec!["gfs", "config", "--path", path.to_str().unwrap(), key],
    };
    run_gfs(args)
}

/// Convenience: gfs commit -m <msg> --path <path> [--author X] [--author-email Y]
pub fn gfs_commit(
    path: &Path,
    message: &str,
    author: Option<&str>,
    author_email: Option<&str>,
) -> (bool, String, String) {
    let mut args = vec![
        "gfs",
        "commit",
        "-m",
        message,
        "--path",
        path.to_str().unwrap(),
    ];
    if let Some(a) = author {
        args.extend(["--author", a]);
    }
    if let Some(e) = author_email {
        args.extend(["--author-email", e]);
    }
    run_gfs(args)
}

/// Convenience: gfs checkout --path <path> <revision>
pub fn gfs_checkout(path: &Path, revision: &str) -> (bool, String, String) {
    run_gfs([
        "gfs",
        "checkout",
        "--path",
        path.to_str().unwrap(),
        revision,
    ])
}

/// Convenience: gfs checkout -b <branch> [start_revision] --path <path>
pub fn gfs_checkout_b(
    path: &Path,
    branch_name: &str,
    start_revision: Option<&str>,
) -> (bool, String, String) {
    let mut args = vec![
        "gfs",
        "checkout",
        "--path",
        path.to_str().unwrap(),
        "-b",
        branch_name,
    ];
    if let Some(rev) = start_revision {
        args.push(rev);
    }
    run_gfs(args)
}

/// [`gfs_checkout`] as a subprocess, for tests that assert on its stdout.
pub fn gfs_checkout_subprocess(path: &Path, revision: &str) -> (bool, String, String) {
    run_gfs_subprocess_checked([
        "gfs",
        "checkout",
        "--path",
        path.to_str().unwrap(),
        revision,
    ])
}

/// [`gfs_checkout_b`] as a subprocess, for tests that assert on its stdout.
pub fn gfs_checkout_b_subprocess(
    path: &Path,
    branch_name: &str,
    start_revision: Option<&str>,
) -> (bool, String, String) {
    let mut args = vec![
        "gfs",
        "checkout",
        "--path",
        path.to_str().unwrap(),
        "-b",
        branch_name,
    ];
    if let Some(rev) = start_revision {
        args.push(rev);
    }
    run_gfs_subprocess_checked(args)
}

/// Convenience: gfs log --path <path> [-n N]
pub fn gfs_log(path: &Path, max_count: Option<usize>) -> (bool, String, String) {
    let mut args: Vec<String> = vec![
        "gfs".into(),
        "log".into(),
        "--path".into(),
        path.to_str().unwrap().into(),
    ];
    if let Some(n) = max_count {
        args.push("-n".into());
        args.push(n.to_string());
    }
    run_gfs(args)
}

/// gfs log via subprocess so stdout is reliably captured (use when verifying log output).
pub fn gfs_log_subprocess(path: &Path, max_count: Option<usize>) -> (bool, String, String) {
    let mut args: Vec<String> = vec![
        "gfs".into(),
        "log".into(),
        "--path".into(),
        path.to_str().unwrap().into(),
    ];
    if let Some(n) = max_count {
        args.push("-n".into());
        args.push(n.to_string());
    }
    run_gfs_subprocess(args)
}

/// Convenience: gfs compute --path <path> status
pub fn gfs_compute_status(path: &Path) -> (bool, String, String) {
    run_gfs(["gfs", "compute", "--path", path.to_str().unwrap(), "status"])
}

/// Convenience: gfs compute --path <path> stop
pub fn gfs_compute_stop(path: &Path) -> (bool, String, String) {
    run_gfs(["gfs", "compute", "--path", path.to_str().unwrap(), "stop"])
}

/// Convenience: gfs import --path <path> --file <file> [--format fmt]
pub fn gfs_import(path: &Path, file: &Path, format: Option<&str>) -> (bool, String, String) {
    let mut args = vec![
        "gfs",
        "import",
        "--path",
        path.to_str().unwrap(),
        "--file",
        file.to_str().unwrap(),
    ];
    if let Some(fmt) = format {
        args.extend(["--format", fmt]);
    }
    run_gfs(args)
}

/// Convenience: gfs schema extract --path <path> [--output <path>] [--compact]
pub fn gfs_schema_extract(
    path: &Path,
    output: Option<&Path>,
    compact: bool,
) -> (bool, String, String) {
    let mut args = vec!["gfs", "schema", "extract", "--path", path.to_str().unwrap()];
    if let Some(o) = output {
        args.extend(["--output", o.to_str().unwrap()]);
    }
    if compact {
        args.push("--compact");
    }
    run_gfs(args)
}
