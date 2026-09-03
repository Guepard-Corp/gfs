//! Reachability and integrity check over a repository's object graph.
//!
//! This is the mark phase. It walks from every root, records what it reaches,
//! then enumerates what is on disk and reports the difference. It opens files
//! read-only and creates nothing, so it is safe to run on a repository that is
//! already broken — which is the situation it exists for.
//!
//! `cmd_log::collect_dag` walks the same graph and is deliberately not reused:
//! it does `Err(_) => continue` on an unreadable commit, silently skipping the
//! exact condition this module has to report.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::model::commit::Commit;
use crate::model::errors::RepoError;
use crate::model::fsck::{Dangling, FsckReport, ObjectKind, Unreachable, Unrecognised};
use crate::model::layout::{GFS_DIR, OBJECTS_DIR, SNAPSHOTS_DIR};
use crate::repo_utils::repo_layout;

/// The sentinel a ref or parent carries when there is no commit yet.
const NO_COMMIT: &str = "0";

/// True for the 64-char lowercase-or-uppercase hex a hash should be.
///
/// Checked before any hash reaches a path builder. `repo_layout` resolves a
/// hash with `split_at(2)`, which panics on a shorter string, and fsck reads
/// ref files that may well be corrupt — a one-character ref would otherwise
/// panic the tool you run *because* the repository is broken.
fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Every `<root>/<2>/<62>` entry, as `(hash, path)`.
///
/// Entries whose shape does not match are skipped rather than reported: a
/// stray file directly under `objects/`, or a prefix directory that is not two
/// characters, was not written by us and naming it as a finding would be noise.
fn two_level(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(prefixes) = std::fs::read_dir(root) else {
        return out;
    };
    for prefix in prefixes.flatten() {
        let prefix_path = prefix.path();
        let Some(prefix_name) = prefix_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if prefix_name.len() != 2 || !prefix_path.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&prefix_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(rest) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            out.push((format!("{prefix_name}{rest}"), entry.path()));
        }
    }
    out
}

/// Commit hashes every walk starts from.
///
/// Today: every branch tip, plus HEAD. HEAD is included separately because a
/// detached HEAD points at a commit no branch names, and dropping it would
/// report the commit you currently have checked out as garbage.
///
/// Soft-deleted refs inside their retention window belong here too and are not
/// yet available — `refs/deleted` arrives with the recoverable-`branch -d`
/// work. When it lands, add the source to this function and nothing else
/// changes.
pub fn roots(repo_path: &Path) -> Result<Vec<String>, RepoError> {
    let mut out: Vec<String> = Vec::new();

    for (_, tip) in repo_layout::list_branches(repo_path)? {
        let tip = tip.trim().to_string();
        if tip != NO_COMMIT && !tip.is_empty() {
            out.push(tip);
        }
    }

    // A repository with no commits has no HEAD commit; that is not an error.
    if let Ok(head) = repo_layout::get_current_commit_id(repo_path) {
        let head = head.trim().to_string();
        if head != NO_COMMIT && !head.is_empty() {
            out.push(head);
        }
    }

    out.sort();
    out.dedup();
    Ok(out)
}

/// Whether snapshot trees for this repository live on the local filesystem.
///
/// On the Kubernetes runtime a snapshot is a `VolumeSnapshot` object and
/// `.gfs/snapshots/<2>/<62>` is never created, so checking the filesystem would
/// report every commit as dangling. `GfsRepository::checkout` already treats a
/// missing snapshot directory as normal there for the same reason.
///
/// An unreadable or absent config means a plain local repository, which is the
/// safe reading: it only ever adds checks.
fn snapshots_are_local(repo_path: &Path) -> bool {
    !matches!(
        repo_layout::get_runtime_config(repo_path)
            .ok()
            .flatten()
            .map(|r| r.runtime_provider.to_ascii_lowercase()),
        Some(ref p) if p == "kubernetes" || p == "k8s"
    )
}

/// What a marking walk reached, kept so the sweep can subtract it from disk.
#[derive(Default)]
struct Marks {
    objects: HashSet<String>,
    snapshots: HashSet<String>,
    commits: usize,
    file_lists: usize,
}

/// Walk from `roots`, marking everything reachable and collecting dangling
/// references found on the way.
fn mark(
    repo_path: &Path,
    roots: &[String],
    check_snapshots: bool,
    dangling: &mut Vec<Dangling>,
) -> Marks {
    let mut marks = Marks::default();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut seen: HashSet<String> = HashSet::new();

    let objects_dir = repo_path.join(GFS_DIR).join(OBJECTS_DIR);
    let snapshots_dir = repo_path.join(GFS_DIR).join(SNAPSHOTS_DIR);

    for r in roots {
        if seen.insert(r.clone()) {
            queue.push_back(r.clone());
        }
    }

    while let Some(hash) = queue.pop_front() {
        if !is_hash(&hash) {
            // A ref or parent that is not a hash at all. Reported against
            // itself, since there is no sensible "from" commit to blame.
            dangling.push(Dangling {
                from_commit: hash.clone(),
                kind: ObjectKind::Commit,
                missing: hash.clone(),
            });
            continue;
        }

        let commit: Commit = match repo_layout::get_commit_from_hash(repo_path, &hash) {
            Ok(c) => c,
            Err(_) => {
                // Missing or unreadable. Either way a reachable commit is not
                // there, which is the finding; the walk cannot continue past it.
                dangling.push(Dangling {
                    from_commit: hash.clone(),
                    kind: ObjectKind::Commit,
                    missing: hash.clone(),
                });
                continue;
            }
        };

        marks.objects.insert(hash.clone());
        marks.commits += 1;

        // Snapshot: a directory, not a file.
        let snap = commit.snapshot_hash.trim();
        if is_hash(snap) && check_snapshots {
            let (a, b) = snap.split_at(2);
            if snapshots_dir.join(a).join(b).is_dir() {
                marks.snapshots.insert(snap.to_string());
            } else {
                dangling.push(Dangling {
                    from_commit: hash.clone(),
                    kind: ObjectKind::Snapshot,
                    missing: snap.to_string(),
                });
            }
        }

        // File list and schema both live in the object store.
        for (maybe, kind) in [
            (commit.files_ref.as_deref(), ObjectKind::FileList),
            (commit.schema_hash.as_deref(), ObjectKind::Schema),
        ] {
            let Some(reference) = maybe.map(str::trim).filter(|r| !r.is_empty()) else {
                continue;
            };
            if !is_hash(reference) {
                continue;
            }
            let (a, b) = reference.split_at(2);
            if objects_dir.join(a).join(b).exists() {
                marks.objects.insert(reference.to_string());
                if kind == ObjectKind::FileList {
                    marks.file_lists += 1;
                }
            } else {
                dangling.push(Dangling {
                    from_commit: hash.clone(),
                    kind,
                    missing: reference.to_string(),
                });
            }
        }

        for parent in commit.parents.iter().flatten() {
            let parent = parent.trim();
            if parent == NO_COMMIT || parent.is_empty() {
                continue;
            }
            if seen.insert(parent.to_string()) {
                queue.push_back(parent.to_string());
            }
        }
    }

    marks
}

/// Identify an unmarked entry in the object store.
///
/// Order matters: a schema object is a directory, so that is settled by a
/// `stat` before anything is parsed.
fn identify_object(path: &Path) -> Result<ObjectKind, String> {
    if path.is_dir() {
        return if path.join("schema.json").is_file() {
            Ok(ObjectKind::Schema)
        } else {
            Err("directory in the object store with no schema.json".to_string())
        };
    }
    let Ok(bytes) = std::fs::read(path) else {
        return Err("could not be read".to_string());
    };
    if serde_json::from_slice::<Commit>(&bytes).is_ok() {
        return Ok(ObjectKind::Commit);
    }
    if repo_layout::decode_file_entries(&bytes).is_ok() {
        return Ok(ObjectKind::FileList);
    }
    Err("not a commit, file list or schema object".to_string())
}

/// Walk the repository and report what is unreachable, dangling or
/// unidentifiable. Reads only; creates and removes nothing.
pub fn check(repo_path: &Path) -> Result<FsckReport, RepoError> {
    let objects_dir = repo_path.join(GFS_DIR).join(OBJECTS_DIR);
    let snapshots_dir = repo_path.join(GFS_DIR).join(SNAPSHOTS_DIR);

    let check_snapshots = snapshots_are_local(repo_path);
    let mut dangling: Vec<Dangling> = Vec::new();
    let roots = roots(repo_path)?;
    let marks = mark(repo_path, &roots, check_snapshots, &mut dangling);

    let mut unreachable: Vec<Unreachable> = Vec::new();
    let mut unrecognised: Vec<Unrecognised> = Vec::new();
    let mut reclaimable_bytes: u64 = 0;

    for (hash, path) in two_level(&objects_dir) {
        if marks.objects.contains(&hash) {
            continue;
        }
        let bytes = object_size(&path);
        match identify_object(&path) {
            Ok(kind) => {
                let summary = if kind == ObjectKind::Commit {
                    std::fs::read(&path)
                        .ok()
                        .and_then(|b| serde_json::from_slice::<Commit>(&b).ok())
                        .map(|c| c.message)
                } else {
                    None
                };
                reclaimable_bytes += bytes;
                unreachable.push(Unreachable {
                    kind,
                    hash,
                    summary,
                    bytes,
                });
            }
            Err(reason) => unrecognised.push(Unrecognised {
                hash,
                reason,
                bytes,
            }),
        }
    }

    let mut checked_snapshots = 0usize;
    for (hash, path) in two_level(&snapshots_dir)
        .into_iter()
        .filter(|_| check_snapshots)
    {
        checked_snapshots += 1;
        if marks.snapshots.contains(&hash) {
            continue;
        }
        let bytes = repo_layout::directory_physical_size_bytes(&path).unwrap_or(0);
        reclaimable_bytes += bytes;
        unreachable.push(Unreachable {
            kind: ObjectKind::Snapshot,
            hash,
            summary: None,
            bytes,
        });
    }

    unreachable.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.hash.cmp(&b.hash)));
    dangling.sort_by(|a, b| a.from_commit.cmp(&b.from_commit));
    unrecognised.sort_by(|a, b| a.hash.cmp(&b.hash));

    Ok(FsckReport {
        checked_commits: marks.commits,
        checked_snapshots,
        checked_file_lists: marks.file_lists,
        unreachable,
        dangling,
        unrecognised,
        reclaimable_bytes,
        snapshots_checked_on_disk: check_snapshots,
    })
}

/// Size of one object entry, whether it is a file or a schema directory.
fn object_size(path: &Path) -> u64 {
    if path.is_dir() {
        repo_layout::directory_physical_size_bytes(path).unwrap_or(0)
    } else {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::layout::{HEADS_DIR, REFS_DIR};
    use std::fs;

    /// A minimal `.gfs` with one branch and no commits.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let gfs = dir.path().join(GFS_DIR);
        fs::create_dir_all(gfs.join(REFS_DIR).join(HEADS_DIR)).unwrap();
        fs::create_dir_all(gfs.join(OBJECTS_DIR)).unwrap();
        fs::create_dir_all(gfs.join(SNAPSHOTS_DIR)).unwrap();
        fs::write(
            gfs.join("HEAD"),
            format!("ref: {REFS_DIR}/{HEADS_DIR}/main"),
        )
        .unwrap();
        fs::write(gfs.join("config.toml"), "version = \"1\"\n").unwrap();
        fs::write(gfs.join(REFS_DIR).join(HEADS_DIR).join("main"), NO_COMMIT).unwrap();
        dir
    }

    fn hash_of(seed: &str) -> String {
        let mut h = seed.to_string();
        while h.len() < 64 {
            h.push('0');
        }
        h[..64].to_string()
    }

    /// Write a commit object, plus optionally the snapshot tree it names.
    fn write_commit(
        repo: &Path,
        seed: &str,
        message: &str,
        parent: Option<&str>,
        snap: bool,
    ) -> String {
        let hash = hash_of(seed);
        let snapshot_hash = hash_of(&format!("5{seed}"));
        let commit = serde_json::json!({
            "hash": hash,
            "message": message,
            "timestamp": "2026-01-01T00:00:00Z",
            "parents": parent.map(|p| vec![p.to_string()]).unwrap_or_default(),
            "snapshot_hash": snapshot_hash,
            "author": "t", "author_date": "2026-01-01T00:00:00Z",
            "committer": "t", "committer_date": "2026-01-01T00:00:00Z",
        });
        let objects = repo.join(GFS_DIR).join(OBJECTS_DIR);
        fs::create_dir_all(objects.join(&hash[..2])).unwrap();
        fs::write(
            objects.join(&hash[..2]).join(&hash[2..]),
            serde_json::to_string_pretty(&commit).unwrap(),
        )
        .unwrap();
        if snap {
            let snaps = repo.join(GFS_DIR).join(SNAPSHOTS_DIR);
            let dir = snaps.join(&snapshot_hash[..2]).join(&snapshot_hash[2..]);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("data.txt"), "x").unwrap();
        }
        hash
    }

    fn set_branch(repo: &Path, name: &str, hash: &str) {
        fs::write(
            repo.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR).join(name),
            hash,
        )
        .unwrap();
    }

    #[test]
    fn a_repository_whose_every_object_is_reachable_is_clean() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "only commit", None, true);
        set_branch(d.path(), "main", &h);

        let r = check(d.path()).unwrap();
        assert!(r.is_clean(), "expected clean, got {r:?}");
        assert_eq!(r.checked_commits, 1);
        assert_eq!(r.exit_code(), 0);
    }

    #[test]
    fn a_commit_no_ref_reaches_is_unreachable_with_its_snapshot() {
        let d = repo();
        let live = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &live);
        write_commit(d.path(), "bb", "stranded", None, true);

        let r = check(d.path()).unwrap();
        assert_eq!(r.exit_code(), 1, "garbage, not corruption: {r:?}");
        assert!(r.dangling.is_empty());
        // the stranded commit object and the snapshot it names
        assert_eq!(r.unreachable.len(), 2, "{:?}", r.unreachable);
        assert!(
            r.unreachable
                .iter()
                .any(|u| u.summary.as_deref() == Some("stranded")),
            "the message should be reported so a human can recognise it: {:?}",
            r.unreachable
        );
    }

    /// The whole point of walking parents rather than only tips.
    #[test]
    fn a_parent_reached_only_through_its_child_is_not_reported() {
        let d = repo();
        let parent = write_commit(d.path(), "aa", "parent", None, true);
        let child = write_commit(d.path(), "bb", "child", Some(&parent), true);
        set_branch(d.path(), "main", &child);

        let r = check(d.path()).unwrap();
        assert!(r.is_clean(), "parent should be reachable: {r:?}");
        assert_eq!(r.checked_commits, 2);
    }

    #[test]
    fn a_commit_whose_snapshot_is_gone_is_dangling_not_unreachable() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "no snapshot", None, false);
        set_branch(d.path(), "main", &h);

        let r = check(d.path()).unwrap();
        assert_eq!(r.exit_code(), 2, "corruption: {r:?}");
        assert_eq!(r.dangling.len(), 1);
        assert_eq!(r.dangling[0].kind, ObjectKind::Snapshot);
    }

    #[test]
    fn an_entry_that_parses_as_nothing_is_reported_rather_than_ignored() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);
        let objects = d.path().join(GFS_DIR).join(OBJECTS_DIR);
        fs::create_dir_all(objects.join("cd")).unwrap();
        fs::write(
            objects.join("cd").join(&hash_of("cd")[2..]),
            b"\xff\xfe junk",
        )
        .unwrap();

        let r = check(d.path()).unwrap();
        assert_eq!(r.unrecognised.len(), 1, "{r:?}");
        assert_eq!(r.exit_code(), 2);
    }

    /// fsck is the tool you run *because* the repository is broken, so a ref
    /// too short to shard must not reach `split_at(2)`.
    #[test]
    fn a_ref_that_is_not_a_hash_is_a_finding_not_a_panic() {
        let d = repo();
        set_branch(d.path(), "main", "x");

        let r = check(d.path()).unwrap();
        assert_eq!(r.dangling.len(), 1, "{r:?}");
        assert_eq!(r.exit_code(), 2);
    }

    /// On Kubernetes a snapshot is a VolumeSnapshot object, so the directory is
    /// never created and a filesystem walk would call every commit dangling.
    #[test]
    fn the_kubernetes_runtime_suppresses_the_snapshot_check() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "no local snapshot", None, false);
        set_branch(d.path(), "main", &h);
        fs::write(
            d.path().join(GFS_DIR).join("config.toml"),
            "version = \"1\"\n[runtime]\nruntime_provider = \"kubernetes\"\n\
             runtime_version = \"1\"\ncontainer_name = \"c\"\n",
        )
        .unwrap();

        let r = check(d.path()).unwrap();
        assert!(!r.snapshots_checked_on_disk);
        assert!(
            r.dangling.is_empty(),
            "a missing snapshot directory is normal on k8s: {:?}",
            r.dangling
        );
        assert!(r.is_clean(), "{r:?}");
    }

    #[test]
    fn a_detached_head_keeps_its_commit_reachable() {
        let d = repo();
        let live = write_commit(d.path(), "aa", "on a branch", None, true);
        set_branch(d.path(), "main", &live);
        let detached = write_commit(d.path(), "bb", "detached", None, true);
        fs::write(d.path().join(GFS_DIR).join("HEAD"), &detached).unwrap();

        let r = check(d.path()).unwrap();
        assert!(
            r.is_clean(),
            "the checked-out commit must not be reported as garbage: {r:?}"
        );
    }

    #[test]
    fn checking_creates_and_removes_nothing() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);
        write_commit(d.path(), "bb", "garbage", None, true);

        let before: Vec<PathBuf> = walkdir(&d.path().join(GFS_DIR));
        let _ = check(d.path()).unwrap();
        let after: Vec<PathBuf> = walkdir(&d.path().join(GFS_DIR));
        assert_eq!(before, after, "fsck must not touch the repository");
    }

    fn walkdir(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(p) = stack.pop() {
            if let Ok(entries) = fs::read_dir(&p) {
                for e in entries.flatten() {
                    let path = e.path();
                    if path.is_dir() {
                        stack.push(path.clone());
                    }
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }
}
