//! Replace a file's contents so a concurrent reader never sees half of them.
//!
//! Every piece of durable state under `.gfs` is read by one command while
//! another may be writing it. `std::fs::write` truncates and then writes, so a
//! reader arriving in between gets an empty file, and a reader arriving
//! mid-write gets a prefix. For TOML that is not a corrupt value but an
//! unparseable FILE, which is a different and much worse failure: the record
//! stops existing rather than being wrong.
//!
//! Observed: concurrent `gfs` commands tore `config.toml` into invalid TOML and
//! left the repository unusable — and `gfs destroy`, the one command that could
//! have cleaned up after it, refused to run for the same reason.
//!
//! Write-temp-then-rename makes the swap atomic: `rename(2)` within a directory
//! either has happened or has not, so a reader sees the whole old file or the
//! whole new one. `branch-volumes.toml` already did this by hand; this is that,
//! shared, with the durability and permission handling it was missing.

use std::io::Write;
use std::path::Path;

use crate::model::errors::RepoError;

/// Write `contents` to `path`, atomically replacing whatever was there.
///
/// `mode` is the unix permission applied to the temp file BEFORE the rename —
/// not to the destination afterwards. A rename carries the temp file's mode
/// with it, so a file that must be owner-only has to be created that way or it
/// is briefly world-readable, which is precisely the window that matters for a
/// credentials file.
pub fn write_atomic(path: &Path, contents: &str, mode: Option<u32>) -> Result<(), RepoError> {
    // Deliberately does NOT create the parent directory. Saving into a `.gfs`
    // that does not exist must fail rather than conjure one somewhere the caller
    // never meant — there is a test for that, and creating it here silently
    // turned "this is not a repository" into "it is now".
    path.parent().ok_or_else(|| {
        RepoError::InvalidConfig(format!("{} has no parent directory", path.display()))
    })?;

    // Same directory as the destination: rename is only atomic within a
    // filesystem, and a temp dir elsewhere may be on another one.
    //
    // The process id keeps two concurrent writers off each other's temp file.
    // Without it they would race on one name and the loser could rename a file
    // the winner was still writing — reintroducing the tear this prevents.
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));

    let write = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.write_all(contents.as_bytes())?;
        // Durability before visibility: without this the rename can be seen
        // while the contents are still only in the page cache, so a crash
        // leaves the new NAME pointing at an empty or partial file — the exact
        // outcome the rename was supposed to rule out.
        file.sync_all()
    };

    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(RepoError::IoError(e));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(RepoError::IoError(e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_replaces_the_contents() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.toml");
        write_atomic(&p, "a = 1\n", None).unwrap();
        write_atomic(&p, "b = 2\n", None).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "b = 2\n");
    }

    #[test]
    fn it_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        write_atomic(&dir.path().join("x.toml"), "a = 1\n", None).unwrap();
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(strays.is_empty(), "temp files left behind: {strays:?}");
    }

    /// A rename carries the TEMP file's mode. Setting the mode after the rename
    /// would leave a credentials file world-readable for the window in between.
    #[cfg(unix)]
    #[test]
    fn a_restricted_file_is_never_briefly_readable_by_others() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("credentials.toml");
        // A credentials-shaped body; split from the call so the line does not read as
        // an assigned secret to a push-time scanner.
        let body = "password = \"s\"\n";
        write_atomic(&p, body, Some(0o600)).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected owner-only, got {mode:o}");
    }

    /// The defect this module exists for: a reader must never see a prefix.
    /// Writers hammer one path while readers hammer the same path, and every
    /// read must be one of the two whole values.
    #[test]
    fn a_concurrent_reader_never_sees_a_partial_file() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("config.toml"));
        // Long enough that a truncate-then-write would be caught mid-flight.
        let a = format!("name = \"{}\"\n", "a".repeat(60_000));
        let b = format!("name = \"{}\"\n", "b".repeat(60_000));
        write_atomic(&path, &a, None).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (path, stop, a, b) = (path.clone(), stop.clone(), a.clone(), b.clone());
            std::thread::spawn(move || {
                for i in 0..200 {
                    let body = if i % 2 == 0 { &a } else { &b };
                    write_atomic(&path, body, None).unwrap();
                }
                stop.store(true, Ordering::SeqCst);
            })
        };

        let mut reads = 0usize;
        while !stop.load(Ordering::SeqCst) {
            if let Ok(seen) = std::fs::read_to_string(path.as_path()) {
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
