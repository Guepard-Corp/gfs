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
//!
//! # Why a grace period, and not only a lock
//!
//! A commit creates its snapshot directory *before* writing the commit object
//! that references it. In that window the snapshot is live data nothing points
//! at, and a mark phase running then would call it garbage.
//!
//! A lock cannot be the whole answer. It substitutes for a grace period only
//! when every writer can be enumerated and forced to a quiescent point, and
//! GFS's writers are separate short-lived processes, possibly of different
//! versions, possibly killed mid-commit by a container runtime. So anything
//! younger than the cutoff is treated as a **root**, not merely skipped, the
//! way git's `reachable.c` does.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::model::commit::Commit;
use crate::model::errors::RepoError;
use crate::model::fsck::{
    Dangling, FsckReport, ObjectKind, ReclaimableWorkspace, Unreachable, Unrecognised,
};
use crate::model::layout::{
    BRANCH_WORKSPACE_SEGMENT, DELETED_REFS_DIR, GFS_DIR, HEAD_FILE, OBJECTS_DIR, REFS_DIR,
    SNAPSHOTS_DIR, WORKSPACE_FILE, WORKSPACES_DIR,
};
use crate::repo_utils::repo_layout;

/// The sentinel a ref or parent carries when there is no commit yet.
const NO_COMMIT: &str = "0";

/// How long a newly written entry is protected from being called garbage.
///
/// Larger than the longest legitimate operation, which is a full snapshot of a
/// multi-gigabyte data directory on a slow volume — hours, not minutes. Not
/// git's two weeks: git protects loose objects costing 4 KB each, this protects
/// whole database snapshots that never dedup.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

/// A value read off disk that should have been a hash, trimmed for display.
///
/// Anything can end up in a ref file — a symlink to `/etc/passwd` makes the
/// "hash" the whole file. An unbounded value read off disk never goes into a
/// report verbatim.
fn brief(value: &str) -> String {
    const MAX: usize = 64;
    let one_line: String = value.chars().take_while(|c| *c != '\n').collect();
    if one_line.is_empty() {
        "(empty)".to_string()
    } else if one_line.chars().count() > MAX {
        let head: String = one_line.chars().take(MAX).collect();
        format!("{head}\u{2026} ({} bytes)", value.len())
    } else {
        one_line
    }
}

/// A hash as it is stored on disk: 64 lowercase hex characters.
///
/// Case matters. Accepting an uppercase spelling means that on a
/// case-insensitive filesystem the read succeeds — so nothing looks dangling —
/// while the store scan yields the lowercase on-disk name, which does not match
/// a mark keyed on the literal ref. The live tip would then read as
/// collectable. Normalising keeps both sides in one spelling.
fn normalise_hash(s: &str) -> Option<String> {
    let t = s.trim();
    if t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(t.to_ascii_lowercase())
    } else {
        None
    }
}

/// True for a leaf name that could be the back 62 characters of a hash.
///
/// Filters out foreign files such as `.DS_Store`, which are not objects and
/// must not be reported as unidentifiable ones.
fn is_object_leaf(name: &str) -> bool {
    name.len() == 62 && name.chars().all(|c| c.is_ascii_hexdigit())
}

/// Every `<root>/<2>/<62>` entry, as `(hash, path)`. Foreign names are skipped.
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
        if prefix_name.len() != 2
            || !prefix_name.chars().all(|c| c.is_ascii_hexdigit())
            || !prefix_path.is_dir()
        {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&prefix_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(rest) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !is_object_leaf(&rest) {
                continue;
            }
            let hash = format!("{prefix_name}{rest}").to_ascii_lowercase();
            out.push((hash, entry.path()));
        }
    }
    out
}

/// Commit hashes every walk starts from, plus any problem found reading a ref.
///
/// Today: every branch tip, plus HEAD. HEAD is included separately because a
/// detached HEAD points at a commit no branch names, and dropping it would
/// report the commit you currently have checked out as garbage.
///
/// Soft-deleted refs inside their retention window belong here too and are not
/// yet available — `refs/deleted` arrives with the recoverable-`branch -d`
/// work. When it lands, add the source here and nothing else changes.
pub fn roots(repo_path: &Path) -> Result<(Vec<String>, Vec<Dangling>), RepoError> {
    let mut out: Vec<String> = Vec::new();
    let mut problems: Vec<Dangling> = Vec::new();

    for (name, tip) in repo_layout::list_branches(repo_path)? {
        let tip = tip.trim();
        if tip == NO_COMMIT || tip.is_empty() {
            continue;
        }
        match normalise_hash(tip) {
            Some(h) => out.push(h),
            // Named against the ref, so the report says which branch is broken
            // rather than echoing an unusable value back at the reader.
            None => problems.push(Dangling {
                from_commit: format!("refs/heads/{name}"),
                kind: ObjectKind::Commit,
                missing: brief(tip),
            }),
        }
    }

    // Soft-deleted branches are recoverable, so their commits are live.
    for h in soft_deleted_roots(repo_path) {
        out.push(h);
    }

    // HEAD is read raw rather than resolved. An *attached* HEAD names a branch
    // whose tip the loop above already covered, so resolving it would report a
    // broken branch twice — once against the ref and once against HEAD. Only a
    // detached HEAD contributes a root the branches do not.
    let head_raw = std::fs::read_to_string(repo_path.join(GFS_DIR).join(HEAD_FILE))
        .map(|h| h.trim().to_string())
        .unwrap_or_default();
    if !head_raw.is_empty() && !head_raw.starts_with("ref:") && head_raw != NO_COMMIT {
        match normalise_hash(&head_raw) {
            Some(h) => out.push(h),
            None => problems.push(Dangling {
                from_commit: "HEAD".to_string(),
                kind: ObjectKind::Commit,
                missing: brief(&head_raw),
            }),
        }
    }

    out.sort();
    out.dedup();
    Ok((out, problems))
}

/// Every commit hash held by a soft-deleted ref under `refs/deleted/`.
///
/// `gfs branch -d` moves a ref aside rather than unlinking it, to
/// `refs/deleted/<unix_millis>/<branch path>`, so the branch stays restorable.
/// Those commits are therefore **not** garbage, and a mark phase that ignored
/// them would report them collectable — and a collector acting on that report
/// would delete exactly the data `gfs branch --restore` promises to give back.
///
/// Read straight off disk rather than through the branch-recovery API, on
/// purpose. That API lives on an unmerged branch based on an older `main`, and
/// depending on it would drag this work backwards. The on-disk layout is the
/// contract here; if it ever changes, `deleted_refs_are_roots` fails.
///
/// **Age is deliberately ignored.** A ref past its retention window is still
/// treated as a root while its file exists. fsck's job is to avoid proposing
/// the collection of anything that still has a recovery record on disk, and
/// over-retaining is the safe direction; expiry is `branch -d`'s to enforce by
/// removing the entry. This also means fsck needs no access to the retention
/// setting, which does not exist on this branch, and cannot disagree with
/// `branch -d` about the window.
fn soft_deleted_roots(repo_path: &Path) -> Vec<String> {
    let base = repo_path
        .join(GFS_DIR)
        .join(REFS_DIR)
        .join(DELETED_REFS_DIR);
    let mut out = Vec::new();
    // Absent before the branch-recovery work lands, which must be a no-op.
    let mut stack = vec![base];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
            } else if let Ok(body) = std::fs::read_to_string(&path)
                && let Some(h) = normalise_hash(&body)
            {
                out.push(h);
            }
        }
    }
    out
}

/// Where the truth about a snapshot's existence comes from.
///
/// A snapshot is only a directory on some backends. On the Kubernetes runtime
/// it is a `VolumeSnapshot` object and `.gfs/snapshots/<2>/<62>` is never
/// created, so walking the filesystem there would report every commit as
/// dangling. Rather than skip the check and say nothing — which left the
/// backend that matters most unverified — the caller supplies the set of
/// snapshots the backend holds, and the same reachability logic runs against
/// it unchanged.
pub enum SnapshotSource<'a> {
    /// Walk `.gfs/snapshots/`. Correct for the file, APFS and btrfs backends.
    Filesystem,
    /// Exactly these snapshot hashes exist, as reported by the backend, mapped
    /// to the bytes deleting each would actually free. Used for Kubernetes,
    /// where the adapter lists ready `VolumeSnapshot`s and joins them to ZFS.
    ///
    /// The value is the backend's *exclusive* figure — ZFS's `used` — not the
    /// `referenced` number a filesystem walk produces. It is the only honest
    /// answer to "how much would I get back", and it is available only because
    /// the backend computes it; nothing GFS can do from a directory tree
    /// reproduces it.
    ///
    /// A hash present with no size is not free, only unmeasured.
    Known(&'a HashMap<String, Option<u64>>),
    /// The backend could not be asked. Snapshots are neither verified nor
    /// reported, and the report says so rather than implying they are fine.
    Unavailable,
}

impl SnapshotSource<'_> {
    fn contains(&self, hash: &str, snapshots_dir: &Path) -> bool {
        match self {
            SnapshotSource::Filesystem => {
                let (a, b) = hash.split_at(2);
                snapshots_dir.join(a).join(b).is_dir()
            }
            SnapshotSource::Known(map) => map.contains_key(hash),
            // Nothing is provably missing when nothing can be asked.
            SnapshotSource::Unavailable => true,
        }
    }

    fn is_checked(&self) -> bool {
        !matches!(self, SnapshotSource::Unavailable)
    }
}

/// The default source for a repository, from its configured runtime.
///
/// Kubernetes returns `Unavailable` here: the caller must supply the set,
/// because only an adapter can ask the cluster.
pub fn default_snapshot_source(repo_path: &Path) -> SnapshotSource<'static> {
    let k8s = matches!(
        repo_layout::get_runtime_config(repo_path)
            .ok()
            .flatten()
            .map(|r| r.runtime_provider.to_ascii_lowercase()),
        Some(ref p) if p == "kubernetes" || p == "k8s"
    );
    if k8s {
        SnapshotSource::Unavailable
    } else {
        SnapshotSource::Filesystem
    }
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
    snapshots: &SnapshotSource<'_>,
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
        let commit: Commit = match repo_layout::get_commit_from_hash(repo_path, &hash) {
            Ok(c) => c,
            Err(_) => {
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

        if snapshots.is_checked() {
            match normalise_hash(&commit.snapshot_hash) {
                Some(snap) => {
                    if snapshots.contains(&snap, &snapshots_dir) {
                        marks.snapshots.insert(snap);
                    } else {
                        dangling.push(Dangling {
                            from_commit: hash.clone(),
                            kind: ObjectKind::Snapshot,
                            missing: snap,
                        });
                    }
                }
                // Reported, never skipped. Skipping leaves the real snapshot
                // unmarked, so the store scan then calls live data garbage
                // while the report claims the repository is consistent.
                None => dangling.push(Dangling {
                    from_commit: hash.clone(),
                    kind: ObjectKind::Snapshot,
                    missing: brief(&commit.snapshot_hash),
                }),
            }
        }

        for (maybe, kind) in [
            (commit.files_ref.as_deref(), ObjectKind::FileList),
            (commit.schema_hash.as_deref(), ObjectKind::Schema),
        ] {
            let Some(raw) = maybe.map(str::trim).filter(|r| !r.is_empty()) else {
                continue;
            };
            match normalise_hash(raw) {
                Some(reference) => {
                    let (a, b) = reference.split_at(2);
                    if objects_dir.join(a).join(b).exists() {
                        if kind == ObjectKind::FileList {
                            marks.file_lists += 1;
                        }
                        marks.objects.insert(reference);
                    } else {
                        dangling.push(Dangling {
                            from_commit: hash.clone(),
                            kind,
                            missing: reference,
                        });
                    }
                }
                None => dangling.push(Dangling {
                    from_commit: hash.clone(),
                    kind,
                    missing: brief(raw),
                }),
            }
        }

        for parent in commit.parents.iter().flatten() {
            let parent = parent.trim();
            if parent == NO_COMMIT || parent.is_empty() {
                continue;
            }
            match normalise_hash(parent) {
                Some(p) => {
                    if seen.insert(p.clone()) {
                        queue.push_back(p);
                    }
                }
                None => dangling.push(Dangling {
                    from_commit: hash.clone(),
                    kind: ObjectKind::Commit,
                    missing: brief(parent),
                }),
            }
        }
    }

    marks
}

/// Identify an unmarked entry in the object store.
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

/// Whether `path` was modified after `cutoff`, and so is protected.
///
/// An unreadable mtime counts as protected: refusing to collect something that
/// cannot be dated is the safe direction.
/// `None` means no window at all, so nothing is protected — including an entry
/// whose mtime is in the future, which `--grace 0` must be able to override.
/// A window so large that the cutoff saturates at the epoch protects
/// everything, which is the correct reading of "keep for a thousand years".
fn newer_than(path: &Path, cutoff: Option<SystemTime>) -> bool {
    let Some(cutoff) = cutoff else {
        return false;
    };
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| t > cutoff)
        .unwrap_or(true)
}

/// Walk the repository and report what is unreachable, dangling or
/// unidentifiable. Reads only; creates and removes nothing.
///
/// `grace` protects recently written entries. See the module docs for why that
/// is a correctness requirement rather than a convenience.
pub fn check(repo_path: &Path, grace: Duration) -> Result<FsckReport, RepoError> {
    check_with(repo_path, grace, &default_snapshot_source(repo_path))
}

/// As [`check`], but the caller says where snapshot truth comes from.
///
/// The Kubernetes path goes through here: the CLI asks the storage adapter for
/// the ready `VolumeSnapshot`s and passes them as [`SnapshotSource::Known`], so
/// the same reachability logic verifies that backend too.
pub fn check_with(
    repo_path: &Path,
    grace: Duration,
    snapshots: &SnapshotSource<'_>,
) -> Result<FsckReport, RepoError> {
    let gfs_dir = repo_path.join(GFS_DIR);
    if !gfs_dir.is_dir() {
        return Err(RepoError::NoRepoFound(repo_path.to_path_buf()));
    }
    let objects_dir = gfs_dir.join(OBJECTS_DIR);
    let snapshots_dir = gfs_dir.join(SNAPSHOTS_DIR);

    // Taken before the walk, so anything written while this runs is newer than
    // the cutoff by construction and cannot be collected on this pass.
    let cutoff = if grace.is_zero() {
        None
    } else {
        Some(
            SystemTime::now()
                .checked_sub(grace)
                .unwrap_or(SystemTime::UNIX_EPOCH),
        )
    };

    let check_snapshots = snapshots.is_checked();
    let (roots, mut dangling) = roots(repo_path)?;
    let marks = mark(repo_path, &roots, snapshots, &mut dangling);

    let mut unreachable: Vec<Unreachable> = Vec::new();
    let mut unrecognised: Vec<Unrecognised> = Vec::new();
    let mut referenced_bytes: u64 = 0;
    let mut exclusive_bytes: u64 = 0;
    let mut protected: usize = 0;

    for (hash, path) in two_level(&objects_dir) {
        if marks.objects.contains(&hash) {
            continue;
        }
        let bytes = object_size(&path);
        match identify_object(&path) {
            // The grace check sits *inside* the Ok arm on purpose. It exists to
            // stop live data being called collectable, not to stop corruption
            // being reported. An entry that parses as nothing is a finding at
            // any age — and corruption is most likely to be recent, because the
            // usual cause is a commit that died partway, so filtering by age
            // would silence it exactly when it matters. `dangling`, the sibling
            // corruption class, has never been age-filtered; this makes the two
            // consistent.
            Ok(kind) if newer_than(&path, cutoff) => {
                let _ = kind;
                protected += 1;
            }
            Ok(kind) => {
                let summary = if kind == ObjectKind::Commit {
                    std::fs::read(&path)
                        .ok()
                        .and_then(|b| serde_json::from_slice::<Commit>(&b).ok())
                        .map(|c| brief(&c.message))
                } else {
                    None
                };
                referenced_bytes += bytes;
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
    if let SnapshotSource::Known(map) = snapshots {
        // The backend is the inventory, and it also knows what each snapshot
        // exclusively holds — which a directory walk cannot. These bytes are
        // the real thing: what deleting it would free, not what `du` shows.
        for (hash, size) in map.iter() {
            checked_snapshots += 1;
            if marks.snapshots.contains(hash) {
                continue;
            }
            let bytes = size.unwrap_or(0);
            exclusive_bytes += bytes;
            unreachable.push(Unreachable {
                kind: ObjectKind::Snapshot,
                hash: hash.clone(),
                summary: None,
                bytes,
            });
        }
    } else if check_snapshots {
        for (hash, path) in two_level(&snapshots_dir) {
            checked_snapshots += 1;
            if marks.snapshots.contains(&hash) {
                continue;
            }
            if newer_than(&path, cutoff) {
                protected += 1;
                continue;
            }
            let bytes = repo_layout::directory_physical_size_bytes(&path).unwrap_or(0);
            referenced_bytes += bytes;
            unreachable.push(Unreachable {
                kind: ObjectKind::Snapshot,
                hash,
                summary: None,
                bytes,
            });
        }
    }

    let live_branches: HashSet<String> = repo_layout::list_branches(repo_path)
        .unwrap_or_default()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let (reclaimable_workspaces, reclaimable_workspace_bytes, ws_protected, ws_unreadable) =
        reclaimable_workspaces(repo_path, &live_branches, &marks.objects, cutoff);
    protected += ws_protected;
    // An unreadable workspace directory is a finding, not a clean result: every
    // other directory we cannot read produces one.
    for path in ws_unreadable {
        unrecognised.push(Unrecognised {
            hash: path,
            reason: "workspace directory could not be read".to_string(),
            bytes: 0,
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
        referenced_bytes,
        exclusive_bytes: if matches!(snapshots, SnapshotSource::Known(_)) {
            Some(exclusive_bytes)
        } else {
            None
        },
        snapshots_checked: check_snapshots,
        protected_by_grace: protected,
        grace_seconds: grace.as_secs(),
        reclaimable_workspaces,
        reclaimable_workspace_bytes,
    })
}

/// Working copies nothing needs any more.
///
/// A workspace is not a graph object; it is a cache that `checkout` rebuilds
/// from a snapshot. But it is a full copy of the data directory, one per branch
/// plus one per detached checkout, nothing ever removes one, and on a real
/// database it dwarfs everything else — so a report that omits them can say
/// "0 reclaimable" for a repository that is mostly waste.
///
/// A branch workspace is identified by shape rather than by depth: any
/// directory holding a [`BRANCH_WORKSPACE_SEGMENT`] child is one, and the
/// branch it belongs to is its path relative to `workspaces/`. That is what
/// makes a nested `team/beta` visible while `team/alpha` is live — keying on
/// top-level entries alone would hide every workspace under a prefix that any
/// live branch shares.
///
/// Three rules, in order:
/// - the directory named by `.gfs/WORKSPACE` is live, whatever else is true;
/// - `workspaces/<branch>/` is live while a ref of exactly that name exists;
/// - `workspaces/detached/<prefix>/` is live while a reachable commit starts
///   with that prefix.
fn reclaimable_workspaces(
    repo_path: &Path,
    live_branches: &HashSet<String>,
    reachable: &HashSet<String>,
    cutoff: Option<SystemTime>,
) -> (Vec<ReclaimableWorkspace>, u64, usize, Vec<String>) {
    let root = repo_path.join(GFS_DIR).join(WORKSPACES_DIR);
    let active = std::fs::read_to_string(repo_path.join(GFS_DIR).join(WORKSPACE_FILE))
        .map(|s| PathBuf::from(s.trim().to_string()))
        .unwrap_or_default();

    let mut out = Vec::new();
    let mut total = 0u64;
    let mut protected = 0usize;
    let mut unreadable = Vec::new();

    if !root.exists() {
        return (out, total, protected, unreadable);
    }

    // Depth-first over real directories only. Symlinks are never followed: a
    // link into the filesystem would otherwise be measured as reclaimable and,
    // worse, written into a collection plan naming a path outside the
    // repository entirely.
    let mut stack = vec![(root.clone(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            unreadable.push(format!("{WORKSPACES_DIR}/{rel}"));
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() || !meta.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let child_rel = if rel.is_empty() {
                name.to_string()
            } else {
                format!("{rel}/{name}")
            };

            if child_rel == "detached" {
                collect_detached(
                    &path,
                    &active,
                    reachable,
                    cutoff,
                    &mut out,
                    &mut total,
                    &mut protected,
                );
                continue;
            }

            // A branch workspace, not a directory that merely holds them.
            if path.join(BRANCH_WORKSPACE_SEGMENT).is_dir() {
                if active.starts_with(&path) || live_branches.contains(&child_rel) {
                    continue;
                }
                if newer_than(&path, cutoff) {
                    protected += 1;
                    continue;
                }
                let bytes = repo_layout::directory_physical_size_bytes(&path).unwrap_or(0);
                total += bytes;
                out.push(ReclaimableWorkspace {
                    path: format!("{WORKSPACES_DIR}/{child_rel}"),
                    reason: "no branch of this name exists".to_string(),
                    bytes,
                    idle_days: idle_days(&path),
                });
                continue;
            }

            stack.push((path, child_rel));
        }
    }

    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    (out, total, protected, unreadable)
}

/// Detached working copies, keyed by a 12-character commit prefix rather than a
/// full hash, so liveness is a prefix match against the reachable set.
fn collect_detached(
    dir: &Path,
    active: &Path,
    reachable: &HashSet<String>,
    cutoff: Option<SystemTime>,
    out: &mut Vec<ReclaimableWorkspace>,
    total: &mut u64,
    protected: &mut usize,
) {
    let Ok(kids) = std::fs::read_dir(dir) else {
        return;
    };
    for kid in kids.flatten() {
        let path = kid.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            continue;
        }
        let Some(prefix) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if active.starts_with(&path) {
            continue;
        }
        let prefix_lc = prefix.to_ascii_lowercase();
        if reachable.iter().any(|c| c.starts_with(&prefix_lc)) {
            continue;
        }
        if newer_than(&path, cutoff) {
            *protected += 1;
            continue;
        }
        let bytes = repo_layout::directory_physical_size_bytes(&path).unwrap_or(0);
        *total += bytes;
        out.push(ReclaimableWorkspace {
            path: format!("{WORKSPACES_DIR}/detached/{prefix}"),
            reason: "no reachable commit starts with this hash".to_string(),
            bytes,
            idle_days: idle_days(&path),
        });
    }
}

/// Days since `path` was last written to, from its mtime.
///
/// Last *write*, not last read: `relatime`/`noatime` make atime unreliable, and
/// a verification pass would itself look like access.
fn idle_days(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
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

        let r = check(d.path(), Duration::ZERO).unwrap();
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

        let r = check(d.path(), Duration::ZERO).unwrap();
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

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert!(r.is_clean(), "parent should be reachable: {r:?}");
        assert_eq!(r.checked_commits, 2);
    }

    #[test]
    fn a_commit_whose_snapshot_is_gone_is_dangling_not_unreachable() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "no snapshot", None, false);
        set_branch(d.path(), "main", &h);

        let r = check(d.path(), Duration::ZERO).unwrap();
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

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert_eq!(r.unrecognised.len(), 1, "{r:?}");
        assert_eq!(r.exit_code(), 2);
    }

    /// fsck is the tool you run *because* the repository is broken, so a ref
    /// too short to shard must not reach `split_at(2)`.
    #[test]
    fn a_ref_that_is_not_a_hash_is_a_finding_not_a_panic() {
        let d = repo();
        set_branch(d.path(), "main", "x");

        let r = check(d.path(), Duration::ZERO).unwrap();
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

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert!(!r.snapshots_checked);
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

        let r = check(d.path(), Duration::ZERO).unwrap();
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
        let _ = check(d.path(), Duration::ZERO).unwrap();
        let after: Vec<PathBuf> = walkdir(&d.path().join(GFS_DIR));
        assert_eq!(before, after, "fsck must not touch the repository");
    }

    /// The window the grace period exists for: a commit writes its snapshot
    /// before the object referencing it, so a mark phase running then would
    /// otherwise call live data garbage.
    #[test]
    fn a_recently_written_object_is_protected_rather_than_collected() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);
        write_commit(d.path(), "bb", "just written", None, true);

        let generous = check(d.path(), Duration::from_secs(3600)).unwrap();
        assert!(
            generous.unreachable.is_empty(),
            "nothing written seconds ago may be collected: {:?}",
            generous.unreachable
        );
        assert!(generous.protected_by_grace > 0);
        assert!(generous.is_clean());

        let none = check(d.path(), Duration::ZERO).unwrap();
        assert!(
            !none.unreachable.is_empty(),
            "with no grace the same entries are reported"
        );
    }

    /// A malformed reference must be reported. Skipping it leaves the real
    /// object unmarked, so the store scan calls live data garbage while the
    /// report claims the repository is consistent.
    #[test]
    fn a_malformed_snapshot_reference_is_reported_not_skipped() {
        let d = repo();
        let h = hash_of("aa");
        let objects = d.path().join(GFS_DIR).join(OBJECTS_DIR);
        fs::create_dir_all(objects.join(&h[..2])).unwrap();
        let commit = serde_json::json!({
            "hash": h, "message": "bad ref", "timestamp": "2026-01-01T00:00:00Z",
            "parents": [], "snapshot_hash": "",
            "author": "t", "author_date": "2026-01-01T00:00:00Z",
            "committer": "t", "committer_date": "2026-01-01T00:00:00Z",
        });
        fs::write(
            objects.join(&h[..2]).join(&h[2..]),
            serde_json::to_string_pretty(&commit).unwrap(),
        )
        .unwrap();
        set_branch(d.path(), "main", &h);

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert_eq!(r.dangling.len(), 1, "{r:?}");
        assert_eq!(r.exit_code(), 2);
    }

    /// On a case-insensitive filesystem an uppercase ref reads fine, so nothing
    /// looks dangling, while the store scan yields the lowercase name. Keyed on
    /// the literal string, the live tip would read as collectable.
    #[test]
    fn an_uppercase_ref_still_marks_the_object_it_names() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h.to_ascii_uppercase());

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert!(
            r.unreachable.is_empty(),
            "the live tip must not be collectable: {:?}",
            r.unreachable
        );
        assert!(r.dangling.is_empty(), "{:?}", r.dangling);
    }

    #[test]
    fn a_foreign_file_in_a_shard_is_not_mistaken_for_an_object() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);
        fs::write(
            d.path()
                .join(GFS_DIR)
                .join(OBJECTS_DIR)
                .join(&h[..2])
                .join(".DS_Store"),
            b"junk",
        )
        .unwrap();

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert!(r.is_clean(), "a foreign file is not corruption: {r:?}");
    }

    #[test]
    fn a_directory_that_is_not_a_repository_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(
            check(d.path(), Duration::ZERO).is_err(),
            "fsck must not report a non-repository as consistent"
        );
    }

    /// A ref can be anything, including a symlink to a large file. An unbounded
    /// value read off disk must never reach the report verbatim.
    #[test]
    fn an_enormous_ref_value_is_truncated_in_the_report() {
        let d = repo();
        set_branch(d.path(), "main", &"z".repeat(50_000));

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert_eq!(r.dangling.len(), 1);
        assert!(
            r.dangling[0].missing.len() < 200,
            "value was not trimmed: {} chars",
            r.dangling[0].missing.len()
        );
        assert_eq!(r.dangling[0].from_commit, "refs/heads/main");
    }

    /// Workspaces are usually the largest thing in a repository and nothing
    /// removes them, so a report that omits them can say "0 reclaimable" for a
    /// repository that is mostly waste.
    #[test]
    fn a_workspace_whose_branch_is_gone_is_reported_but_the_live_one_is_not() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);

        let ws = d.path().join(GFS_DIR).join("workspaces");
        for name in ["main", "deleted-branch"] {
            let dir = ws.join(name).join("0").join("data");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("payload"), vec![0u8; 4096]).unwrap();
        }
        fs::write(
            d.path().join(GFS_DIR).join("WORKSPACE"),
            ws.join("main")
                .join("0")
                .join("data")
                .to_string_lossy()
                .as_ref(),
        )
        .unwrap();

        let r = check(d.path(), Duration::ZERO).unwrap();
        let paths: Vec<&str> = r
            .reclaimable_workspaces
            .iter()
            .map(|w| w.path.as_str())
            .collect();
        assert_eq!(
            paths,
            vec!["workspaces/deleted-branch"],
            "only the workspace with no branch is stale: {paths:?}"
        );
        assert!(r.reclaimable_workspace_bytes > 0);
        // Waste, not corruption.
        assert_eq!(r.exit_code(), 1);
    }

    /// A branch directory that merely contains nested branches is not stale.
    #[test]
    fn a_workspace_directory_holding_a_nested_branch_is_kept() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);
        fs::create_dir_all(
            d.path()
                .join(GFS_DIR)
                .join(REFS_DIR)
                .join(HEADS_DIR)
                .join("team"),
        )
        .unwrap();
        set_branch(d.path(), "team/alpha", &h);

        let dir = d.path().join(GFS_DIR).join("workspaces").join("team");
        fs::create_dir_all(dir.join("alpha").join("0")).unwrap();

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert!(
            r.reclaimable_workspaces.is_empty(),
            "team/ holds a live branch: {:?}",
            r.reclaimable_workspaces
        );
    }

    /// The grace period protects live data from being *collected*. It must not
    /// stop corruption being *reported* — and corruption is usually recent,
    /// since the usual cause is a commit that died partway.
    #[test]
    fn a_freshly_written_unidentifiable_object_is_still_reported() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);
        let objects = d.path().join(GFS_DIR).join(OBJECTS_DIR);
        fs::create_dir_all(objects.join("cd")).unwrap();
        fs::write(objects.join("cd").join("c".repeat(62)), b"\xff\xfe junk").unwrap();

        // Written a moment ago, so a generous window would hide it if the check
        // were applied before identification.
        let r = check(d.path(), Duration::from_secs(3600)).unwrap();
        assert_eq!(
            r.unrecognised.len(),
            1,
            "corruption must survive grace: {r:?}"
        );
        assert_eq!(r.exit_code(), 2);
    }

    /// Keying on top-level entries alone hides every workspace under a prefix
    /// that any live branch happens to share.
    #[test]
    fn a_nested_workspace_is_visible_even_when_a_sibling_branch_is_live() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);
        fs::create_dir_all(
            d.path()
                .join(GFS_DIR)
                .join(REFS_DIR)
                .join(HEADS_DIR)
                .join("team"),
        )
        .unwrap();
        set_branch(d.path(), "team/alpha", &h);

        let ws = d.path().join(GFS_DIR).join("workspaces");
        for name in ["team/alpha", "team/beta"] {
            let dir = ws.join(name).join("0").join("data");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("payload"), vec![0u8; 2048]).unwrap();
        }

        let r = check(d.path(), Duration::ZERO).unwrap();
        let paths: Vec<&str> = r
            .reclaimable_workspaces
            .iter()
            .map(|w| w.path.as_str())
            .collect();
        assert_eq!(
            paths,
            vec!["workspaces/team/beta"],
            "the live sibling must be kept and the dead one found: {paths:?}"
        );
    }

    /// A window of zero must protect nothing, and a window so large the cutoff
    /// saturates must protect everything. Both land on the epoch, so they have
    /// to be distinguished explicitly.
    #[test]
    fn the_two_extreme_grace_windows_mean_opposite_things() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);
        write_commit(d.path(), "bb", "garbage", None, true);

        assert!(
            !check(d.path(), Duration::ZERO)
                .unwrap()
                .unreachable
                .is_empty(),
            "a zero window protects nothing"
        );
        assert!(
            check(d.path(), Duration::from_secs(u64::MAX / 2))
                .unwrap()
                .unreachable
                .is_empty(),
            "an enormous window protects everything"
        );
    }

    /// A soft-deleted branch is recoverable, so its commits are live. Without
    /// this, fsck would report them collectable and a collector acting on that
    /// report would delete exactly what `gfs branch --restore` promises back.
    #[test]
    fn deleted_refs_are_roots() {
        let d = repo();
        let live = write_commit(d.path(), "aa", "on main", None, true);
        set_branch(d.path(), "main", &live);
        let deleted = write_commit(d.path(), "bb", "on a deleted branch", None, true);

        // Without the recovery record it is garbage.
        let before = check(d.path(), Duration::ZERO).unwrap();
        assert!(
            !before.unreachable.is_empty(),
            "unreferenced commit should be collectable"
        );

        // The on-disk layout `gfs branch -d` writes:
        // refs/deleted/<unix_millis>/<branch path>, holding the tip.
        let entry = d
            .path()
            .join(GFS_DIR)
            .join(REFS_DIR)
            .join(DELETED_REFS_DIR)
            .join("1788452232388")
            .join("feature");
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        fs::write(&entry, &deleted).unwrap();

        let after = check(d.path(), Duration::ZERO).unwrap();
        assert!(
            after.is_clean(),
            "a recoverable branch must not be reported collectable: {:?}",
            after.unreachable
        );
        assert_eq!(after.checked_commits, 2);
    }

    /// Nested branch names nest here too, and the whole thing must be a no-op
    /// before the branch-recovery work lands and the directory exists at all.
    #[test]
    fn nested_deleted_refs_are_found_and_an_absent_directory_is_harmless() {
        let d = repo();
        let live = write_commit(d.path(), "aa", "on main", None, true);
        set_branch(d.path(), "main", &live);

        // No refs/deleted at all: unchanged behaviour.
        assert!(check(d.path(), Duration::ZERO).unwrap().is_clean());

        let deep = write_commit(d.path(), "bb", "team/alpha", None, true);
        let entry = d
            .path()
            .join(GFS_DIR)
            .join(REFS_DIR)
            .join(DELETED_REFS_DIR)
            .join("1788452232999")
            .join("team")
            .join("alpha");
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        fs::write(&entry, &deep).unwrap();

        assert!(
            check(d.path(), Duration::ZERO).unwrap().is_clean(),
            "a nested deleted ref is still a root"
        );
    }

    /// Age is ignored on purpose: while a recovery record exists on disk, the
    /// data it names must not be proposed for collection. Expiry is
    /// `branch -d`'s job, by removing the entry.
    #[test]
    fn an_expired_looking_deleted_ref_is_still_a_root() {
        let d = repo();
        let live = write_commit(d.path(), "aa", "on main", None, true);
        set_branch(d.path(), "main", &live);
        let old = write_commit(d.path(), "bb", "deleted long ago", None, true);

        // A timestamp from 2020, far outside any retention window.
        let entry = d
            .path()
            .join(GFS_DIR)
            .join(REFS_DIR)
            .join(DELETED_REFS_DIR)
            .join("1577836800000")
            .join("ancient");
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        fs::write(&entry, &old).unwrap();

        assert!(
            check(d.path(), Duration::ZERO).unwrap().is_clean(),
            "over-retaining is the safe direction"
        );
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
