//! The result of an integrity check over a repository's object graph.
//!
//! Three findings, kept apart because they mean different things and call for
//! different actions:
//!
//! - **unreachable** — stored and valid, but no walk from a root reached it.
//!   This is the collectable set, and it is normal for a repository to have
//!   some.
//! - **dangling** — a reachable commit names something that is *missing*. This
//!   is corruption, and a collector must refuse to run against it.
//! - **unrecognised** — an entry under `objects/` that parses as no known
//!   object type. Surfaced rather than ignored, following `git count-objects
//!   -v`, which reports `garbage` — "files in the ODB that are neither valid
//!   loose objects nor valid packs" — as its own line.

use serde::{Deserialize, Serialize};

/// What a stored entry is, once identified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    /// A commit object: JSON under `objects/<2>/<62>`.
    Commit,
    /// A file list: bincode-encoded `Vec<FileEntry>` under `objects/<2>/<62>`.
    FileList,
    /// A schema object, which is a *directory* holding `schema.json` and
    /// `schema.sql` rather than a single file.
    Schema,
    /// A snapshot tree under `snapshots/<2>/<62>`.
    Snapshot,
}

impl ObjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Commit => "commit",
            ObjectKind::FileList => "file list",
            ObjectKind::Schema => "schema",
            ObjectKind::Snapshot => "snapshot",
        }
    }
}

/// Something on disk that no walk from a root reached.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unreachable {
    pub kind: ObjectKind,
    pub hash: String,
    /// The commit's message, when the entry is a commit — enough for a human to
    /// recognise what they are about to lose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// On-disk size. See [`FsckReport::reclaimable_bytes`] for what this does
    /// and does not promise.
    pub bytes: u64,
}

/// A reachable commit that names something which is not there.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dangling {
    /// The commit holding the reference.
    pub from_commit: String,
    /// What kind of thing is missing.
    pub kind: ObjectKind,
    /// The hash that resolved to nothing.
    pub missing: String,
}

/// An entry under `objects/` that could not be identified.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unrecognised {
    pub hash: String,
    /// Why identification failed, for a human. Never a parser error dump.
    pub reason: String,
    pub bytes: u64,
}

/// A working copy on disk that nothing needs any more.
///
/// Workspaces are not part of the object graph — they are rebuildable caches,
/// restored from a snapshot on the next checkout — so they are reported apart
/// from `unreachable` and their bytes are counted separately. They are also
/// usually the largest thing in a repository: a full copy of the data
/// directory, one per branch plus one per detached checkout, and nothing
/// removes them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleWorkspace {
    /// Path relative to `.gfs/`, e.g. `workspaces/feature/0`.
    pub path: String,
    /// Why nothing needs it.
    pub reason: String,
    pub bytes: u64,
}

/// The outcome of `gfs fsck`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsckReport {
    pub checked_commits: usize,
    pub checked_snapshots: usize,
    pub checked_file_lists: usize,

    pub unreachable: Vec<Unreachable>,
    pub dangling: Vec<Dangling>,
    pub unrecognised: Vec<Unrecognised>,

    /// Sum of the on-disk sizes of the unreachable entries.
    ///
    /// This is what `du` would report for those trees, **not** what `df` would
    /// give back. On a filesystem with reflink — APFS, and Btrfs or XFS with
    /// reflink enabled — a snapshot shares blocks with the tree it was cloned
    /// from, so the space actually freed by removing it is somewhere between
    /// zero and this number. On a filesystem without reflink the two agree.
    pub reclaimable_bytes: u64,

    /// Whether snapshot trees were checked on the local filesystem.
    ///
    /// False on the Kubernetes runtime, where a snapshot is a `VolumeSnapshot`
    /// object rather than a directory: `.gfs/snapshots/<2>/<62>` is never
    /// created, so a filesystem walk would call every commit dangling. When
    /// this is false the snapshot half of the report is simply absent, and no
    /// conclusion about snapshots should be drawn from it.
    pub snapshots_checked_on_disk: bool,

    /// How many entries were left out of `unreachable` only because they are
    /// newer than the grace cutoff.
    ///
    /// A commit creates its snapshot before writing the object that references
    /// it, so anything recent may belong to an operation still in flight. These
    /// are protected rather than reported; a non-zero count here means a later
    /// run may find more.
    pub protected_by_grace: usize,

    /// The grace period this run applied, in seconds.
    pub grace_seconds: u64,

    /// Working copies no branch or reachable commit needs. Reported apart from
    /// `unreachable` because they are caches rather than graph objects.
    pub stale_workspaces: Vec<StaleWorkspace>,

    /// Bytes held by [`Self::stale_workspaces`], counted apart from
    /// `reclaimable_bytes` for the same reason.
    pub stale_workspace_bytes: u64,
}

impl FsckReport {
    /// Process exit code, following `gfs schema diff`'s three-way convention so
    /// a script can branch on the outcome without parsing output.
    ///
    /// - `0` — consistent, nothing to collect
    /// - `1` — unreachable objects found; a collector would have work to do
    /// - `2` — corruption found, which takes priority over `1`
    pub fn exit_code(&self) -> i32 {
        if !self.dangling.is_empty() || !self.unrecognised.is_empty() {
            2
        } else if !self.unreachable.is_empty() || !self.stale_workspaces.is_empty() {
            1
        } else {
            0
        }
    }

    /// True when nothing at all was found.
    pub fn is_clean(&self) -> bool {
        self.exit_code() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> FsckReport {
        FsckReport {
            checked_commits: 3,
            checked_snapshots: 3,
            checked_file_lists: 3,
            unreachable: Vec::new(),
            dangling: Vec::new(),
            unrecognised: Vec::new(),
            reclaimable_bytes: 0,
            snapshots_checked_on_disk: true,
            protected_by_grace: 0,
            grace_seconds: 0,
            stale_workspaces: Vec::new(),
            stale_workspace_bytes: 0,
        }
    }

    #[test]
    fn a_clean_repository_exits_zero() {
        assert_eq!(report().exit_code(), 0);
        assert!(report().is_clean());
    }

    #[test]
    fn unreachable_objects_exit_one() {
        let mut r = report();
        r.unreachable.push(Unreachable {
            kind: ObjectKind::Snapshot,
            hash: "a".repeat(64),
            summary: None,
            bytes: 1024,
        });
        assert_eq!(r.exit_code(), 1);
        assert!(!r.is_clean());
    }

    /// Corruption outranks garbage: a repository with both must not look like
    /// one that merely needs collecting.
    #[test]
    fn corruption_outranks_unreachable() {
        let mut r = report();
        r.unreachable.push(Unreachable {
            kind: ObjectKind::Snapshot,
            hash: "a".repeat(64),
            summary: None,
            bytes: 1024,
        });
        r.dangling.push(Dangling {
            from_commit: "b".repeat(64),
            kind: ObjectKind::Snapshot,
            missing: "c".repeat(64),
        });
        assert_eq!(r.exit_code(), 2);
    }

    #[test]
    fn an_unidentifiable_entry_is_corruption_not_garbage() {
        let mut r = report();
        r.unrecognised.push(Unrecognised {
            hash: "d".repeat(64),
            reason: "not a commit, file list or schema object".into(),
            bytes: 12,
        });
        assert_eq!(r.exit_code(), 2);
    }

    /// Absent optionals are omitted rather than emitted as null, matching the
    /// convention `model::status` establishes and its tests assert.
    #[test]
    fn a_summary_that_is_absent_is_omitted_from_json() {
        let u = Unreachable {
            kind: ObjectKind::Snapshot,
            hash: "a".repeat(64),
            summary: None,
            bytes: 0,
        };
        let json = serde_json::to_string(&u).unwrap();
        assert!(!json.contains("summary"), "unexpected key in {json}");
        assert!(json.contains("\"kind\":\"snapshot\""), "got {json}");
    }
}
