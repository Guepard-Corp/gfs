//! A checkout must not depend on which `cp` happens to be first on `PATH`.
//!
//! `cp -cRp` asks for a clonefile(2) copy, which is a BSD flag. GNU `cp` rejects
//! it outright, and Homebrew's coreutils installs a GNU `cp` that many developers
//! put ahead of `/bin`. `gfs_domain::utils::system_bin::resolve` exists so a call
//! site reaches `/bin/cp` regardless, and the storage adapters and
//! `gfs_repository::copy_dir_all` all go through it.
//!
//! The existing tests cover the resolver itself -- that it returns an absolute
//! path, that it falls back when the name is absent. None of them notice a CALL
//! SITE that stops using it, which is exactly the regression that produced the
//! defect this guards: `cp` was resolved in both storage adapters and missed in
//! `copy_dir_all`, so `commit` worked and `checkout` failed with
//! "error: io error: cp -cRp failed" and nothing about `PATH`.
//!
//! So this drives the real binary with a deliberately hostile `PATH`. Verified to
//! bite: with `copy_dir_all` reverted to a bare `Command::new("cp")` this fails
//! with the original error, and passes again when resolution is restored.

#![cfg(target_os = "macos")]

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn gfs_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().expect("test binary path");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("gfs")
}

/// A `cp` that behaves like GNU's: any flag containing `c` is rejected with GNU's
/// own message, everything else is delegated to the real `/bin/cp`.
///
/// Delegating matters. A stub that failed unconditionally would also break copies
/// that have nothing to do with clonefile, so a failure would not tell us which
/// call site was at fault.
fn shadowing_cp_dir() -> TempDir {
    let dir = TempDir::new().expect("temp dir for the fake cp");
    let script = dir.path().join("cp");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         for a in \"$@\"; do\n\
           case \"$a\" in\n\
             -*c*) echo \"cp: invalid option -- 'c'\" >&2; exit 1 ;;\n\
           esac\n\
         done\n\
         exec /bin/cp \"$@\"\n",
    )
    .expect("write the fake cp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make the fake cp executable");
    }
    dir
}

fn gfs(cwd: &Path, path_prefix: Option<&Path>, args: &[&str]) -> (i32, String) {
    let mut cmd = Command::new(gfs_bin());
    cmd.current_dir(cwd)
        .args(args)
        .env("RUST_LOG", "off")
        .env("NO_COLOR", "1")
        .env("GFS_NO_TELEMETRY", "1");
    if let Some(prefix) = path_prefix {
        let existing = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{existing}", prefix.display()));
    }
    let out = cmd.output().expect("run gfs");
    let merged = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), merged)
}

#[test]
fn a_checkout_survives_a_gnu_cp_earlier_on_path_than_bin() {
    let fake = shadowing_cp_dir();

    // The fake must actually reject the flag, or the rest of this proves nothing.
    let probe = Command::new(fake.path().join("cp"))
        .args(["-cRp", "/dev/null", "/dev/null"])
        .output()
        .expect("run the fake cp");
    assert!(
        !probe.status.success(),
        "precondition: the fake cp must reject -cRp"
    );

    // EVERY step runs with the hostile PATH, not just the checkout. `commit`
    // copies through the storage adapter and `checkout` through
    // `copy_dir_all`; running only the checkout under it would leave an adapter
    // free to regress unnoticed, which is how the two drifted apart originally.
    let hostile = Some(fake.path());

    let repo = TempDir::new().unwrap();
    assert_eq!(gfs(repo.path(), hostile, &["init", "."]).0, 0, "init");

    let data = repo.path().join(".gfs/workspaces/main/0/data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("payload"), b"content to copy").unwrap();

    let (code, output) = gfs(repo.path(), hostile, &["commit", "-m", "first"]);
    assert_eq!(
        code, 0,
        "commit copies through the storage adapter: {output}"
    );
    assert_eq!(
        gfs(repo.path(), hostile, &["branch", "feature"]).0,
        0,
        "branch"
    );

    let (code, output) = gfs(repo.path(), hostile, &["checkout", "feature"]);
    assert_eq!(
        code, 0,
        "checkout must not depend on which cp is first on PATH: {output}"
    );
    assert!(
        !output.contains("invalid option"),
        "the shadowing cp was invoked, so a call site is not resolving: {output}"
    );
}
