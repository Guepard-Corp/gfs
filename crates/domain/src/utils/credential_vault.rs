//! Per-repository credential vault (RFC 008): a database's passwords live in
//! `{repo}/.gfs/secrets/<name>` on the data-plane node, at mode `0600` — never in
//! the cloud. This is a dumb typed key/value store over the filesystem; it holds
//! no tenant, policy, or encryption concept — the node filesystem plus `0600` are
//! the trust boundary (RFC 008 §7). Mirrors the plain `io::Result` style of the
//! sibling `utils/data_dir.rs`.

use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// Secrets directory relative to a database's GFS repository root.
const SECRETS_DIR: &str = ".gfs/secrets";

/// A node-local credential store rooted at a database's GFS repository.
pub struct RepoCredentialVault;

impl RepoCredentialVault {
    /// Write `value` to `{repo}/.gfs/secrets/<name>` at mode `0600` (creating the
    /// secrets directory at `0700` if needed). Overwrites an existing secret and
    /// re-pins its mode.
    ///
    /// # Errors
    /// Invalid `name` (see [`secret_path`]) or an underlying filesystem error.
    pub fn put(repo: &Path, name: &str, value: &[u8]) -> io::Result<()> {
        let path = secret_path(repo, name)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
            set_mode(dir, 0o700)?;
        }
        write_secret_file(&path, value)
    }

    /// Read `{repo}/.gfs/secrets/<name>`; `Ok(None)` when it does not exist.
    ///
    /// # Errors
    /// Invalid `name` or an underlying filesystem error other than not-found.
    pub fn get(repo: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
        let path = secret_path(repo, name)?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Remove `{repo}/.gfs/secrets/<name>`; idempotent (a missing secret is `Ok`).
    ///
    /// # Errors
    /// Invalid `name` or an underlying filesystem error other than not-found.
    pub fn delete(repo: &Path, name: &str) -> io::Result<()> {
        let path = secret_path(repo, name)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Validate `name` and join it under `{repo}/.gfs/secrets/`. Names are
/// `[a-z0-9_]+` (non-empty) — no path separators, `..`, dots, or uppercase — so a
/// name can never escape the secrets directory.
fn secret_path(repo: &Path, name: &str) -> io::Result<PathBuf> {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid secret name: {name:?} (expected [a-z0-9_]+)"),
        ));
    }
    Ok(repo.join(SECRETS_DIR).join(name))
}

#[cfg(unix)]
fn write_secret_file(path: &Path, value: &[u8]) -> io::Result<()> {
    use std::io::Write;
    // `mode(0o600)` applies only when this call creates the file; on an overwrite
    // it is ignored, so re-pin the mode explicitly afterwards.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(value)?;
    set_mode(path, 0o600)
}

#[cfg(not(unix))]
fn write_secret_file(path: &Path, value: &[u8]) -> io::Result<()> {
    std::fs::write(path, value)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(mode);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn put_then_get_roundtrips() {
        let dir = repo();
        RepoCredentialVault::put(dir.path(), "owner_password", b"s3cr3t").unwrap();
        assert_eq!(
            RepoCredentialVault::get(dir.path(), "owner_password").unwrap(),
            Some(b"s3cr3t".to_vec())
        );
    }

    #[test]
    fn get_missing_is_none() {
        let dir = repo();
        assert_eq!(
            RepoCredentialVault::get(dir.path(), "absent").unwrap(),
            None
        );
    }

    #[test]
    fn delete_is_idempotent() {
        let dir = repo();
        RepoCredentialVault::put(dir.path(), "owner_password", b"x").unwrap();
        RepoCredentialVault::delete(dir.path(), "owner_password").unwrap();
        assert_eq!(
            RepoCredentialVault::get(dir.path(), "owner_password").unwrap(),
            None
        );
        // Second delete on the now-absent secret is still Ok.
        RepoCredentialVault::delete(dir.path(), "owner_password").unwrap();
    }

    #[test]
    fn put_overwrites() {
        let dir = repo();
        RepoCredentialVault::put(dir.path(), "owner_password", b"first").unwrap();
        RepoCredentialVault::put(dir.path(), "owner_password", b"second").unwrap();
        assert_eq!(
            RepoCredentialVault::get(dir.path(), "owner_password").unwrap(),
            Some(b"second".to_vec())
        );
    }

    #[cfg(unix)]
    #[test]
    fn secret_file_is_0600_and_dir_0700() {
        let dir = repo();
        RepoCredentialVault::put(dir.path(), "admin_password", b"x").unwrap();
        let file_mode = std::fs::metadata(dir.path().join(".gfs/secrets/admin_password"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600, "secret must be 0600, got {file_mode:o}");
        let dir_mode = std::fs::metadata(dir.path().join(".gfs/secrets"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "secrets dir must be 0700, got {dir_mode:o}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn overwrite_keeps_0600_even_if_prior_mode_was_loose() {
        let dir = repo();
        RepoCredentialVault::put(dir.path(), "owner_password", b"first").unwrap();
        let path = dir.path().join(".gfs/secrets/owner_password");
        // Loosen the mode behind the vault's back, then overwrite.
        set_mode(&path, 0o644).unwrap();
        RepoCredentialVault::put(dir.path(), "owner_password", b"second").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "overwrite must re-pin 0600, got {mode:o}");
    }

    #[test]
    fn rejects_names_that_could_escape_the_vault() {
        let dir = repo();
        for bad in [
            "",
            "../escape",
            "a/b",
            "UPPER",
            "with space",
            "dot.name",
            "..",
        ] {
            assert!(
                RepoCredentialVault::put(dir.path(), bad, b"x").is_err(),
                "put must reject name {bad:?}"
            );
            assert!(
                RepoCredentialVault::get(dir.path(), bad).is_err(),
                "get must reject name {bad:?}"
            );
            assert!(
                RepoCredentialVault::delete(dir.path(), bad).is_err(),
                "delete must reject name {bad:?}"
            );
        }
    }
}
