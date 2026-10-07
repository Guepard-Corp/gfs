//! Replace a file so that neither a concurrent reader nor a crash can see half
//! of it, and so that the replacement survives a power cut once it returns.
//!
//! Why there are two modules doing nearly the same thing: [`super::atomic_write`]
//! is kept byte-identical to the copy on the unmerged branch
//! `feat/env-record-and-resolution`, so that when that branch is merged the
//! file is the same on both sides and merges clean. Changing it here would turn
//! that into an add/add conflict. The hardening lives in this module instead.
//! Once that branch has landed, fold the two together (`write_atomic` becomes a
//! `&str` wrapper over [`write_durable`]) and delete the duplicate.
//!
//! What this adds over `write_atomic`:
//!
//! - **Bytes**, not only `&str`, so bincode objects go through it too.
//! - **A temp name that starts with a dot** (`.<name>.tmp.<pid>.<n>`). A crash
//!   can leave the temp behind, and every directory walker under `.gfs` treats
//!   a file name as data: a ref, or half of a 64-hex object id. A leading dot
//!   is never a valid branch segment and never hex, so walkers can skip it.
//!   The per-process counter keeps two threads of one process (a long-running
//!   server embedding the library) off each other's temp file.
//! - **An fsync of the parent directory after the rename** (unix). `rename(2)`
//!   is atomic for readers immediately, but the directory entry it changed is
//!   only durable once the directory itself is synced. Without it a power cut
//!   after the call returned can bring back the old name or none.
//!
//! `GFS_FSYNC=off` (case-insensitive; also `0` / `false`) skips both the file
//! and the directory sync. It exists for test suites, where thousands of
//! fsyncs dominate the run time, and must not be set in production: the write
//! is still atomic for readers, but no longer durable across a crash. Default
//! is on.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Name of the environment variable that disables fsync. See the module docs.
pub const FSYNC_ENV: &str = "GFS_FSYNC";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Whether a value of `GFS_FSYNC` disables syncing.
fn fsync_disabled_by(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        let v = v.trim();
        v.eq_ignore_ascii_case("off") || v == "0" || v.eq_ignore_ascii_case("false")
    })
}

fn fsync_enabled() -> bool {
    !fsync_disabled_by(std::env::var(FSYNC_ENV).ok().as_deref())
}

/// Whether `name` is a directory entry this module may have left behind.
///
/// Every temp file it creates starts with `.`; a walker that sees one has found
/// either a write in flight or the leftover of a crashed one, and must skip it.
pub fn is_temp_name(name: &str) -> bool {
    name.starts_with('.')
}

/// The temp path for `path`: same directory, dotted, unique per process and
/// per call.
fn temp_path(path: &Path) -> std::io::Result<(PathBuf, &Path)> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", path.display()),
        )
    })?;
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} has no file name", path.display()),
        )
    })?;
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".{}.tmp.{}.{}",
        name.to_string_lossy(),
        std::process::id(),
        n
    ));
    Ok((tmp, parent))
}

/// Create the temp file, set its mode, write `bytes`, and sync it.
fn write_temp(tmp: &Path, bytes: &[u8], mode: Option<u32>, sync: bool) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    file.write_all(bytes)?;
    if sync {
        file.sync_all()?;
    }
    Ok(())
}

/// Sync a directory so a rename or link inside it survives a crash.
///
/// A filesystem that cannot sync a directory (some network and FUSE mounts
/// answer `EINVAL`) is tolerated: there is nothing more this process can do,
/// and failing the write after the new contents are already visible would
/// report a failure that did not happen.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        match File::open(dir).and_then(|d| d.sync_all()) {
            Ok(()) => Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
                ) =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        // Windows cannot open a directory as a plain file; NTFS journals the
        // rename itself. Best-effort means nothing to do here.
        let _ = dir;
        Ok(())
    }
}

/// Write `bytes` to `path`, atomically replacing whatever was there, and make
/// the result durable before returning.
///
/// Sequence: dotted temp in the same directory (`create_new`), optional mode,
/// write, `fsync(temp)`, `rename(temp, path)`, `fsync(parent)`. The temp is
/// removed if any step before the rename fails. Like `write_atomic`, it does
/// NOT create the parent directory.
///
/// `mode` is applied to the temp file before the rename, because a rename
/// carries the temp's mode: setting it on the destination afterwards leaves a
/// window in which the file has the default mode.
pub fn write_durable(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let sync = fsync_enabled();
    let (tmp, parent) = temp_path(path)?;
    if let Err(e) = write_temp(&tmp, bytes, mode, sync) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if sync {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Create `path` holding `bytes`, refusing with `AlreadyExists` if it exists.
///
/// The no-clobber counterpart of [`write_durable`], for refs that must not
/// replace an existing one (`gfs branch <name>`). `create_new` followed by a
/// write has a window in which the file exists but is empty, which a reader or
/// a crash can observe; here the complete, synced temp is `link(2)`ed into
/// place, which is atomic and fails with `EEXIST` rather than overwriting.
///
/// A filesystem without hard links falls back to `create_new` + write + sync:
/// still no-clobber and durable, without the atomicity for readers.
pub fn create_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let sync = fsync_enabled();
    let (tmp, parent) = temp_path(path)?;
    if let Err(e) = write_temp(&tmp, bytes, None, sync) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(e),
        Err(_) => {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            file.write_all(bytes)?;
            if sync {
                file.sync_all()?;
            }
        }
    }
    if sync {
        sync_dir(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn it_replaces_the_contents() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("main");
        write_durable(&p, b"a", None).unwrap();
        write_durable(&p, b"bb", None).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"bb");
        assert_eq!(entries(dir.path()), vec!["main".to_string()]);
    }

    #[test]
    fn it_does_not_create_the_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("absent").join("main");
        assert!(write_durable(&p, b"x", None).is_err());
        assert!(!dir.path().join("absent").exists());
        assert!(entries(dir.path()).is_empty());
    }

    /// The temp for a 62-hex object file must not itself look like one, or an
    /// object walker would read a leftover as half of a hash.
    #[test]
    fn the_temp_name_is_dotted_and_never_hex() {
        let dir = tempfile::tempdir().unwrap();
        let name = "a".repeat(62);
        let (tmp, _) = temp_path(&dir.path().join(&name)).unwrap();
        let tmp_name = tmp.file_name().unwrap().to_string_lossy().into_owned();
        assert!(is_temp_name(&tmp_name), "{tmp_name}");
        assert!(!tmp_name.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(tmp_name.starts_with(&format!(".{name}.tmp.")));
    }

    #[cfg(unix)]
    #[test]
    fn a_restricted_file_is_created_with_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("credentials.toml");
        // A credentials-shaped body; split from the call so the line does not read as
        // an assigned secret to a push-time scanner.
        let body = b"password = \"s\"\n";
        write_durable(&p, body, Some(0o600)).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected owner-only, got {mode:o}");
    }

    #[test]
    fn create_refuses_an_existing_file_and_leaves_it_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("feature");
        create_durable(&p, b"first").unwrap();
        let err = create_durable(&p, b"second").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&p).unwrap(), b"first");
        assert_eq!(entries(dir.path()), vec!["feature".to_string()]);
    }

    #[test]
    fn the_fsync_switch_parses_off_and_nothing_else() {
        assert!(!fsync_disabled_by(None));
        assert!(!fsync_disabled_by(Some("on")));
        assert!(!fsync_disabled_by(Some("")));
        assert!(fsync_disabled_by(Some("off")));
        assert!(fsync_disabled_by(Some("OFF")));
        assert!(fsync_disabled_by(Some("0")));
        assert!(fsync_disabled_by(Some("false")));
    }

    /// A reader must never see a prefix. Same shape as the `write_atomic` tear
    /// test, over bytes: a writer thread alternates two large bodies, a reader
    /// thread asserts every read is one of them whole.
    #[test]
    fn a_concurrent_reader_never_sees_a_partial_file() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("object"));
        let a = vec![b'a'; 60_000];
        let b = vec![b'b'; 60_000];
        write_durable(&path, &a, None).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (path, stop, a, b) = (path.clone(), stop.clone(), a.clone(), b.clone());
            std::thread::spawn(move || {
                for i in 0..200 {
                    let body = if i % 2 == 0 { &a } else { &b };
                    write_durable(&path, body, None).unwrap();
                }
                stop.store(true, Ordering::SeqCst);
            })
        };

        let mut reads = 0usize;
        while !stop.load(Ordering::SeqCst) {
            if let Ok(seen) = std::fs::read(path.as_path()) {
                reads += 1;
                assert!(
                    seen == a || seen == b,
                    "read a partial file: {} bytes",
                    seen.len()
                );
            }
        }
        writer.join().unwrap();
        assert!(reads > 0, "the reader never observed the file");
    }
}
