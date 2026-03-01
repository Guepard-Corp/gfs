//! In-process CLI runner for integration tests.
//!
//! Uses `gfs_cli::run()` instead of spawning a subprocess, so coverage is captured.
//! Captures stdout/stderr via the `gag` crate.

#![allow(dead_code)] // Helpers used by different test binaries

use std::path::Path;

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

/// Convenience: gfs init <path>
pub fn gfs_init(path: &Path) -> bool {
    let (ok, _, _) = run_gfs(["gfs", "init", path.to_str().unwrap()]);
    ok
}

/// Convenience: gfs init --database-provider postgres --database-version 17 <path>
pub fn gfs_init_with_db(path: &Path) -> bool {
    let (ok, _, _) = run_gfs([
        "gfs",
        "init",
        "--database-provider",
        "postgres",
        "--database-version",
        "17",
        path.to_str().unwrap(),
    ]);
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
