//! Temporary directories a container runtime can actually mount.
//!
//! A container runtime that runs in a virtual machine shares only part of the
//! host filesystem. Colima shares the home directory; it does not share `/tmp`
//! or the macOS per-user temp directory that `std::env::temp_dir()` returns.
//! Docker on Linux has the same restriction for `/tmp`.
//!
//! A bind mount whose source is outside the shared set is not refused: the path
//! is created inside the VM, the database writes there, and the host directory
//! stays empty. A test that only checks the database responds then passes while
//! its data never reached this machine, which is a green that means nothing.
//!
//! So every test that provisions a container takes its repository directory from
//! here rather than from the system temp directory.

use std::path::PathBuf;

use tempfile::TempDir;

/// A base directory the container runtime is expected to share with the host.
///
/// Prefers `$HOME`, which both Colima and Docker Desktop share by default,
/// then `/var/tmp`, which Docker on Linux can usually mount. Falls back to the
/// system temp directory so a machine with neither still runs the suite — there
/// the adapter's own check reports the unshared path.
pub fn shareable_base() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let base = PathBuf::from(home).join(".gfs-test-tmp");
        if std::fs::create_dir_all(&base).is_ok() {
            return base;
        }
    }
    let var_tmp = PathBuf::from("/var/tmp");
    if var_tmp.is_dir() {
        return var_tmp;
    }
    std::env::temp_dir()
}

/// A temporary directory inside [`shareable_base`], removed when dropped.
///
/// Drop-in replacement for `tempfile::tempdir()` in any test that provisions a
/// container.
pub fn shared_tempdir() -> std::io::Result<TempDir> {
    TempDir::new_in(shareable_base())
}
