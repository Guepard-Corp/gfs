//! Exclusive advisory lock serialising the operations that mutate a repository.
//!
//! Commit and checkout both read repository state, act on it, and then write it
//! back, and neither is atomic. Interleaving them loses data. Demonstrated on
//! two branches with a workspace large enough to make the snapshot copy take
//! about half a second, launching `gfs checkout other` 50 ms into a
//! `gfs commit` on `main`:
//!
//! 1. commit reads HEAD's commit as the new commit's PARENT — `main`'s tip;
//! 2. commit copies the workspace, which takes as long as the data is large;
//! 3. checkout moves HEAD to `other`;
//! 4. commit finishes and advances *the current branch* — now `other` — to a
//!    commit whose parent is `main`'s tip.
//!
//! `other`'s previous tip is then unreachable from any ref. Four runs of ten
//! rounds each produced the lost commit every time. Nothing warns: both
//! commands report success.
//!
//! An embedded provider was accidentally partly protected, which is why this
//! took a database-less workspace to show. Checkout quiesces the database
//! before restoring, and that `BEGIN IMMEDIATE` blocks on the write lock the
//! commit's snapshot guard already holds — measured at 550 ms against 34 ms
//! unobstructed. It is not a fix: it does not exist before the first write, it
//! does not cover container-backed providers, and it leaves the window between
//! the commit reading its parent and acquiring the guard.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::model::layout::GFS_DIR;

/// Name of the lock file, kept from when only commit took it. Renaming it would
/// mean an old `gfs commit` and a new `gfs checkout` locking different files,
/// which is worse than an inaccurate name.
const LOCK_FILE: &str = "commit.lock";

/// The lock is held by another process.
#[derive(Debug)]
pub struct Busy {
    pub lock_path: std::path::PathBuf,
}

#[derive(Debug)]
pub enum LockError {
    Busy(Busy),
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Busy(busy) => write!(
                f,
                "another operation is already running on this repository \
                 (lock held at {}); retry once it finishes",
                busy.lock_path.display()
            ),
            LockError::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Held for the duration of one repository-mutating operation.
///
/// Released on drop via explicit `unlock`; if the process is killed the kernel
/// releases the `flock` when the descriptor closes, so a crash never wedges the
/// repository.
#[derive(Debug)]
pub struct RepoLock {
    file: File,
}

impl RepoLock {
    fn open(repo_path: &Path) -> Result<(File, std::path::PathBuf), LockError> {
        let gfs_dir = repo_path.join(GFS_DIR);
        std::fs::create_dir_all(&gfs_dir).map_err(LockError::Io)?;
        let lock_path = gfs_dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(LockError::Io)?;
        Ok((file, lock_path))
    }

    /// Take the lock, or fail immediately if someone else holds it.
    ///
    /// What commit does. Queueing a commit behind another commit is not useful:
    /// by the time the first finishes, the second would snapshot a state the
    /// first already captured, so failing loudly is the more honest answer.
    pub fn try_acquire(repo_path: &Path) -> Result<Self, LockError> {
        let (file, lock_path) = Self::open(repo_path)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(LockError::Busy(Busy { lock_path })),
            Err(std::fs::TryLockError::Error(e)) => Err(LockError::Io(e)),
        }
    }

    /// Take the lock, waiting up to `timeout` for whoever holds it.
    ///
    /// What checkout does, and the difference from `try_acquire` is deliberate.
    /// A commit is the long operation, and a user who asks to switch branches
    /// while one is running wants the switch, not an error telling them to run
    /// it again in a minute. The wait is bounded rather than indefinite so a
    /// long-lived daemon cannot be parked forever by a stuck commit.
    pub fn acquire_waiting(repo_path: &Path, timeout: Duration) -> Result<Self, LockError> {
        let deadline = Instant::now() + timeout;
        loop {
            match Self::try_acquire(repo_path) {
                Err(LockError::Busy(busy)) if Instant::now() < deadline => {
                    let _ = busy;
                    std::thread::sleep(Duration::from_millis(25));
                }
                other => return other,
            }
        }
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        // Best-effort: the kernel releases it on close regardless.
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_is_refused_while_the_first_lives() {
        let dir = tempfile::tempdir().unwrap();
        let _first = RepoLock::try_acquire(dir.path()).expect("first acquire");
        assert!(
            matches!(RepoLock::try_acquire(dir.path()), Err(LockError::Busy(_))),
            "a second holder must not get in"
        );
    }

    #[test]
    fn the_lock_is_available_again_after_the_holder_drops() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _first = RepoLock::try_acquire(dir.path()).unwrap();
        }
        RepoLock::try_acquire(dir.path()).expect("released on drop");
    }

    #[test]
    fn waiting_gives_up_rather_than_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let _held = RepoLock::try_acquire(dir.path()).unwrap();
        let started = Instant::now();
        let result = RepoLock::acquire_waiting(dir.path(), Duration::from_millis(120));
        assert!(matches!(result, Err(LockError::Busy(_))));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the wait must be bounded: took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn waiting_succeeds_once_the_holder_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let held = RepoLock::try_acquire(dir.path()).unwrap();
        let path = dir.path().to_path_buf();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            drop(held);
        });
        RepoLock::acquire_waiting(&path, Duration::from_secs(5)).expect("should get it");
        releaser.join().unwrap();
    }
}
