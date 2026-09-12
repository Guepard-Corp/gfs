//! Will this copy share extents, or silently duplicate every byte?
//!
//! The storage adapters copy a data directory with `cp --reflink=auto`, which
//! is *specified* to fall back to a full byte copy when the filesystem cannot
//! clone, and to do so **silently**: exit status 0, nothing on stderr. So on a
//! filesystem without reflink support every snapshot is a complete second copy
//! — time proportional to the data directory, disk equal to it — while the
//! command reports success. Measured on ext4: 190 MiB consumed for a 190 MiB
//! file, exit 0. On a ZFS pool with block cloning disabled, three branch clones
//! cost 1,157 MiB this way against 73 KiB through `zfs clone`.
//!
//! This module answers the question before the copy runs, so the caller can say
//! so.
//!
//! # Why a probe, and not a filesystem-name check
//!
//! The filesystem's name does not determine the answer:
//!
//! * **XFS** supports reflink only when created with `reflink=1`. Measured:
//!   `stat -f -c %T` returns `xfs` for both a capable and an incapable one.
//! * **ZFS 2.2** reports `feature@block_cloning = enabled` at the *pool* level
//!   while `/sys/module/zfs/parameters/zfs_bclone_enabled = 0` disables it in
//!   the *module*. Distributions ship it off because of the block-cloning
//!   defect in 2.2.0–2.2.2, so a pool advertises a feature the kernel refuses.
//!   Measured on such a pool: `FICLONE` returns `EOPNOTSUPP`, matching what
//!   `cp --reflink` actually does.
//! * **ext4** never supports it.
//!
//! A name check answers "ZFS, so yes" and is wrong. Only attempting a clone is
//! right, so that is what this does.
//!
//! # Why `st_dev` is not the cross-device test either
//!
//! Comparing device ids looks like a cheap way to detect a cross-filesystem
//! copy, and it is wrong in the direction that matters: **btrfs gives every
//! subvolume its own anonymous `st_dev`, and ZFS gives every dataset one**, yet
//! cloning works across them within a pool. Measured on btrfs — two subvolumes
//! with different `st_dev`, and `cp --reflink=always` between them succeeds.
//! Cloning from a file that genuinely lives on the source filesystem is what
//! makes `EXDEV` mean something.
//!
//! # Why nothing is left behind
//!
//! The destination inode is opened with `O_TMPFILE`, which has no name at all:
//! there is nothing for a crash to leave and nothing a later snapshot could
//! capture. That matters because the destination here is a snapshot directory
//! or a live data directory, and a stray file in either gets committed. Where
//! `O_TMPFILE` is unsupported (overlayfs rejects it at `open`), the fallback
//! creates a randomly-named file and unlinks it immediately, keeping only the
//! descriptor — so the exposure is two syscalls wide, on a zero-length file.

use std::path::Path;

/// Why a copy will duplicate bytes instead of sharing extents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoReflink {
    /// The destination filesystem cannot clone file extents.
    Unsupported,
    /// Source and destination are on different filesystems.
    CrossDevice,
}

impl NoReflink {
    /// Operator-facing explanation: what is true, and what to do about it.
    pub fn explain(self) -> &'static str {
        match self {
            Self::Unsupported => {
                "this filesystem cannot clone file extents, so every snapshot is a \
                 full copy of the data directory: it takes time proportional to the \
                 data size and consumes that much additional disk. Copy-on-write \
                 needs btrfs, XFS created with reflink=1, or ZFS with block cloning \
                 enabled in the module (zfs_bclone_enabled=1, ZFS >= 2.2.3). The \
                 kubernetes runtime uses volume snapshots instead and is unaffected"
            }
            Self::CrossDevice => {
                "the snapshot directory is on a different filesystem from the \
                 workspace, so no clone is possible and every snapshot is a full \
                 copy. Put the repository and its snapshots on one filesystem to \
                 restore copy-on-write"
            }
        }
    }
}

/// What a copy from a source tree into a destination directory will do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The copy will share extents.
    Clones,
    /// The copy will duplicate every byte, for this reason.
    FullCopy(NoReflink),
    /// The probe could not run, so the answer is genuinely unknown.
    ///
    /// Distinct from [`Self::FullCopy`] on purpose. A probe that fails because
    /// the disk is full, the mount is read-only, or the process lacks
    /// permission says nothing about whether the filesystem can clone, and
    /// reporting it as a capability problem would answer a question nobody
    /// asked — telling someone who has run out of disk that they should migrate
    /// to btrfs. Callers stay silent on this.
    Unknown,
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{NoReflink, Outcome};
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// `_IOW(0x94, 9, int)` — the FICLONE ioctl. Defined here rather than taken
    /// from `libc` so the probe does not depend on which libc release exposes
    /// it.
    const FICLONE: libc::c_ulong = 0x4004_9409;

    struct Fd(libc::c_int);

    impl Drop for Fd {
        fn drop(&mut self) {
            if self.0 >= 0 {
                unsafe { libc::close(self.0) };
            }
        }
    }

    fn cstr(p: &Path) -> Option<CString> {
        CString::new(p.as_os_str().as_bytes()).ok()
    }

    /// Open any regular file in `src` read-only, for use as a clone source.
    ///
    /// Cloning from a file that actually lives on the source filesystem is what
    /// makes an `EXDEV` answer meaningful. `O_NOATIME` avoids perturbing the
    /// source, and is dropped on `EPERM` because it requires ownership.
    fn open_source(src: &Path) -> Option<Fd> {
        fn first_regular_file(dir: &Path, budget: &mut u32) -> Option<std::path::PathBuf> {
            super::find_probe_source(dir, budget)
        }

        // Bounded so an enormous or pathological tree cannot make the probe
        // cost more than the copy it is describing.
        let mut budget = 512u32;
        let path = if src.is_file() {
            src.to_path_buf()
        } else {
            first_regular_file(src, &mut budget)?
        };
        let c = cstr(&path)?;
        for flags in [libc::O_RDONLY | libc::O_NOATIME, libc::O_RDONLY] {
            let fd = unsafe { libc::open(c.as_ptr(), flags) };
            if fd >= 0 {
                return Some(Fd(fd));
            }
        }
        None
    }

    /// Open an unnamed inode on `dir`'s filesystem to clone into.
    fn open_dest(dir: &Path) -> Option<Fd> {
        let c = cstr(dir)?;
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_TMPFILE | libc::O_RDWR, 0o600) };
        if fd >= 0 {
            return Some(Fd(fd));
        }
        // overlayfs and some network filesystems reject O_TMPFILE at open().
        // Fall back to a named file that is unlinked before anything is written
        // to it, so the window in which a name exists is two syscalls wide.
        let mut seed = [0u8; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut seed))
            .ok()?;
        let name: String = seed.iter().map(|b| format!("{b:02x}")).collect();
        let probe = dir.join(format!(".gfs-clone-probe-{name}"));
        let pc = cstr(&probe)?;
        let fd = unsafe {
            libc::open(
                pc.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )
        };
        if fd < 0 {
            return None;
        }
        // Unlink immediately: the descriptor keeps the inode alive, the name
        // does not outlive this call even if the process is killed.
        unsafe { libc::unlink(pc.as_ptr()) };
        Some(Fd(fd))
    }

    pub fn check(src: &Path, dst_dir: &Path) -> Outcome {
        let (Some(s), Some(d)) = (open_source(src), open_dest(dst_dir)) else {
            return Outcome::Unknown;
        };
        let rc = unsafe { libc::ioctl(d.0, FICLONE, s.0) };
        if rc == 0 {
            return Outcome::Clones;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            // The filesystem does not implement extent cloning.
            Some(libc::EOPNOTSUPP) | Some(libc::EINVAL) => {
                Outcome::FullCopy(NoReflink::Unsupported)
            }
            // Source and destination are genuinely on different filesystems.
            Some(libc::EXDEV) => Outcome::FullCopy(NoReflink::CrossDevice),
            // Anything else — ENOSPC, EROFS, EACCES, EPERM, EDQUOT, EBADF — is a
            // fact about this moment, not about the filesystem's capability.
            _ => Outcome::Unknown,
        }
    }
}

/// Find a small regular file under `dir` to use as a clone source.
///
/// Small on purpose. On macOS `clonefile` produces a real clone of the source,
/// so a process killed between the clone and the unlink would leave behind a
/// file the size of whatever was picked. A data directory almost always has a
/// tiny file near the top (PostgreSQL has `PG_VERSION`, three bytes), so
/// preferring the smallest candidate seen keeps that residue negligible. The
/// walk is bounded so a pathological tree cannot make the probe cost more than
/// the copy it describes.
#[cfg(unix)]
pub(crate) fn find_probe_source(dir: &Path, budget: &mut u32) -> Option<std::path::PathBuf> {
    const SMALL_ENOUGH: u64 = 64 * 1024;
    let mut best: Option<(u64, std::path::PathBuf)> = None;
    let mut subdirs = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        match entry.file_type() {
            Ok(t) if t.is_file() => {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
                if size <= SMALL_ENOUGH {
                    return Some(entry.path());
                }
                if best.as_ref().is_none_or(|(b, _)| size < *b) {
                    best = Some((size, entry.path()));
                }
            }
            Ok(t) if t.is_dir() => subdirs.push(entry.path()),
            _ => {}
        }
    }
    for d in subdirs {
        if *budget == 0 {
            break;
        }
        if let Some(found) = find_probe_source(&d, budget) {
            return Some(found);
        }
    }
    best.map(|(_, p)| p)
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{NoReflink, Outcome};
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    unsafe extern "C" {
        /// `clonefile(2)` — APFS copy-on-write clone. Path-based, and the
        /// destination must not exist, so unlike Linux there is no way to clone
        /// into an unnamed inode.
        fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32)
        -> libc::c_int;
    }

    fn cstr(p: &Path) -> Option<CString> {
        CString::new(p.as_os_str().as_bytes()).ok()
    }

    pub fn check(src: &Path, dst_dir: &Path) -> Outcome {
        let mut budget = 512u32;
        let src_file = if src.is_file() {
            src.to_path_buf()
        } else {
            match super::find_probe_source(src, &mut budget) {
                Some(f) => f,
                None => return Outcome::Unknown,
            }
        };

        let mut seed = [0u8; 16];
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut seed))
            .is_err()
        {
            return Outcome::Unknown;
        }
        let name: String = seed.iter().map(|b| format!("{b:02x}")).collect();
        let probe = dst_dir.join(format!(".gfs-clone-probe-{name}"));

        let (Some(s), Some(d)) = (cstr(&src_file), cstr(&probe)) else {
            return Outcome::Unknown;
        };
        let rc = unsafe { clonefile(s.as_ptr(), d.as_ptr(), 0) };
        let err = std::io::Error::last_os_error().raw_os_error();
        // Remove it whatever happened; a failed clone may still have created it.
        let _ = std::fs::remove_file(&probe);

        if rc == 0 {
            return Outcome::Clones;
        }
        match err {
            // Measured on a real HFS+ volume: ENOTSUP.
            Some(libc::ENOTSUP) => Outcome::FullCopy(NoReflink::Unsupported),
            // Measured across an APFS/HFS+ volume boundary, both directions.
            Some(libc::EXDEV) => Outcome::FullCopy(NoReflink::CrossDevice),
            // Anything else says nothing about the filesystem's capability.
            _ => Outcome::Unknown,
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use super::Outcome;
    use std::path::Path;

    /// Windows has no clone concept at all — robocopy always copies bytes —
    /// so there is no capability to probe and nothing conditional to report.
    pub fn check(_src: &Path, _dst_dir: &Path) -> Outcome {
        Outcome::Unknown
    }
}

/// What a copy from `src` into `dst_dir` will do.
///
/// `dst_dir` must be an existing directory on the destination filesystem —
/// the probe writes an unnamed inode into it.
pub fn check(src: &Path, dst_dir: &Path) -> Outcome {
    imp::check(src, dst_dir)
}

/// Report, once per call site, when a copy will duplicate rather than share.
///
/// Deliberately a warning and not an error: a full copy is slow and costly but
/// correct, and refusing would break every user on a filesystem without
/// reflink — ext4, the default on most Linux distributions. The defect being
/// fixed is the silence, not the fallback.
pub fn warn_if_full_copy(src: &Path, dst_dir: &Path) {
    if let Outcome::FullCopy(reason) = check(src, dst_dir) {
        tracing::warn!(
            destination = %dst_dir.display(),
            "snapshot will be a full copy, not a copy-on-write clone: {}",
            reason.explain()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explain_names_a_remedy_for_each_reason() {
        assert!(NoReflink::Unsupported.explain().contains("btrfs"));
        assert!(
            NoReflink::Unsupported
                .explain()
                .contains("zfs_bclone_enabled")
        );
        assert!(NoReflink::CrossDevice.explain().contains("one filesystem"));
    }

    /// The distinction the previous attempt at this module got wrong: a probe
    /// that cannot run must not be reported as a filesystem that cannot clone.
    #[test]
    fn an_unprobeable_path_is_unknown_not_full_copy() {
        let missing = Path::new("/nonexistent-gfs-reflink-probe-path");
        assert_eq!(check(missing, missing), Outcome::Unknown);
        // And therefore silent.
        warn_if_full_copy(missing, missing);
    }

    /// A source directory with no regular file anywhere in it cannot produce a
    /// clone source, which is unknowable rather than incapable.
    #[test]
    fn an_empty_source_tree_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("empty");
        std::fs::create_dir_all(&src).unwrap();
        assert_eq!(check(&src, dir.path()), Outcome::Unknown);
    }

    /// Whatever the answer, the probe must leave nothing behind — the
    /// destination here is a snapshot directory or a live data directory, and a
    /// stray file in either would be captured by the next commit.
    #[test]
    fn the_probe_leaves_no_trace_in_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("f"), vec![0xA5u8; 4096]).unwrap();

        let _ = check(&src, &dst);

        let leftovers: Vec<_> = std::fs::read_dir(&dst)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "probe left {leftovers:?} behind");
    }

    /// The source must not be modified or consumed by being probed.
    #[test]
    fn the_probe_does_not_disturb_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let f = src.join("data");
        std::fs::write(&f, b"original").unwrap();

        let _ = check(&src, dir.path());

        assert_eq!(std::fs::read(&f).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(&src).unwrap().count(), 1);
    }
}
