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
    Dangling, FsckReport, ObjectKind, ReclaimableWorkspace, SnapshotFacts, Unreachable,
    Unrecognised,
};
use crate::model::layout::{
    BRANCH_WORKSPACE_SEGMENT, DELETED_REFS_DIR, GFS_DIR, HEAD_FILE, HEADS_DIR, OBJECTS_DIR,
    REFS_DIR, SNAPSHOTS_DIR, WORKSPACE_FILE, WORKSPACES_DIR,
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

/// The smallest grace a caller may choose without explicitly disabling the
/// check, as RFC 009 D4 requires following Dolt's `BackupPruneMinGracePeriod`.
///
/// One hour, not a round guess: a commit writes its snapshot tree before the
/// object that references it, so any window shorter than the longest plausible
/// commit lets an in-flight snapshot be called garbage. Measured commits here
/// run in seconds, so an hour leaves three orders of magnitude of headroom
/// while still allowing a deliberate shortening on a quiet repository.
///
/// This is policy, not mechanism: `check` still accepts any `Duration`, so the
/// domain stays usable from tests and future callers that know what they want.
pub const MIN_GRACE: Duration = Duration::from_secs(60 * 60);

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
/// Every object leaf under a two-level shard, and separately everything else
/// that is in there.
///
/// The second list is not a curiosity. `Unrecognised` exists because
/// `git count-objects -v` reports `garbage` — "files in the ODB that are
/// neither valid loose objects nor valid packs" — rather than ignoring it, and
/// this walker was quietly doing the opposite: every non-conforming name hit a
/// `continue` and vanished. A stray file in the object store was reported as
/// nothing at all, which is the one outcome worse than a false alarm.
fn two_level(
    repo_path: &Path,
    root: &Path,
    blind: &mut Blind,
) -> (Vec<(String, PathBuf)>, Vec<PathBuf>) {
    let mut out = Vec::new();
    let mut junk = Vec::new();
    let prefixes = match std::fs::read_dir(root) {
        Ok(p) => p,
        // An object store we cannot open is not an empty one. Returning nothing
        // here used to make a repository whose store was unreadable report
        // "checked 0 objects" and then "consistent".
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (out, junk),
        Err(e) => {
            blind.at(repo_path, root, format!("could not be listed: {e}"));
            return (out, junk);
        }
    };
    for prefix in prefixes {
        let prefix = match prefix {
            Ok(p) => p,
            Err(e) => {
                blind.at(repo_path, root, format!("an entry could not be read: {e}"));
                continue;
            }
        };
        let prefix_path = prefix.path();
        let Some(prefix_name) = prefix_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if is_incidental(prefix_name) {
            continue;
        }
        if prefix_name.len() != 2 || !prefix_name.chars().all(|c| c.is_ascii_hexdigit()) {
            junk.push(prefix_path);
            continue;
        }
        // `is_dir()` was the whole test here, and it is false both for "not a
        // directory" and for "could not be stat'd" -- so an object store the
        // process cannot traverse (mode 444: readable, not executable) turned
        // every well-formed shard in it into "unexpected entry in the object
        // store". The reader was handed a list of corruption to repair, all of it
        // an artefact of the one permission we lacked.
        let meta = match std::fs::symlink_metadata(&prefix_path) {
            Ok(m) => m,
            // Raced away between listing and stat: gone is gone, and there is
            // nothing left to report about it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                blind.at(
                    repo_path,
                    &prefix_path,
                    format!("shard could not be stat'd: {e}"),
                );
                continue;
            }
        };
        // A shard is a directory GFS created. A symlink in its place is an
        // anomaly worth naming, and following it walks the object store out of
        // the repository -- reporting paths under a directory that only looks
        // like it is inside, and putting them in front of a collector.
        if meta.file_type().is_symlink() || !meta.is_dir() {
            junk.push(prefix_path);
            continue;
        }
        let entries = match std::fs::read_dir(&prefix_path) {
            Ok(e) => e,
            Err(e) => {
                // A whole shard invisible: every object in it silently absent.
                blind.at(
                    repo_path,
                    &prefix_path,
                    format!("shard could not be listed: {e}"),
                );
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    blind.at(
                        repo_path,
                        &prefix_path,
                        format!("an entry could not be read: {e}"),
                    );
                    continue;
                }
            };
            let Some(rest) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if is_incidental(&rest) {
                continue;
            }
            if !is_object_leaf(&rest) {
                junk.push(entry.path());
                continue;
            }
            let hash = format!("{prefix_name}{rest}").to_ascii_lowercase();
            out.push((hash, entry.path()));
        }
    }
    (out, junk)
}

/// Files an operating system leaves lying about, which are not findings.
///
/// Narrow and explicit on purpose. `.DS_Store` appears in any directory a macOS
/// Finder window has been opened on, and treating it as an unidentifiable object
/// would make a routine check report corruption — and refuse to write a plan —
/// on a repository where nothing is wrong. Every name here is a known artefact
/// of a file browser or archiver, never anything a partial write produces.
fn is_incidental(name: &str) -> bool {
    matches!(
        name,
        ".DS_Store" | "._.DS_Store" | "Thumbs.db" | "desktop.ini" | ".gitkeep" | ".keep"
    )
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
pub fn roots(
    repo_path: &Path,
    blind: &mut Blind,
) -> Result<(Vec<String>, Vec<Dangling>), RepoError> {
    let mut out: Vec<String> = Vec::new();
    let mut problems: Vec<Dangling> = Vec::new();

    let heads = repo_path.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR);
    // `list_branches` answers `NotFound` with an empty list, which is a fair
    // reading for a caller that wants to print branches. It is the wrong reading
    // here, for the reason its own doc comment gives about the unreadable case:
    // branch tips are the roots of every walk, so "no branches" and "the branch
    // store is gone" differ by the entire repository. `init` always writes
    // `refs/heads/main`, so there is no repository in which this is legitimately
    // absent -- a partial copy or a careless restore is what leaves it missing,
    // and that is exactly when every object looks unreferenced.
    if !heads.exists() {
        blind.at(
            repo_path,
            &heads,
            "is missing, so no branch could be used as a root",
        );
    }
    let mut named: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (name, tip) in repo_layout::list_branches(repo_path)? {
        named.insert(name.clone());
        // A ref is a file. A symlink where one should be sends the walk outside
        // the repository to decide what is live inside it, and the sibling
        // walkers in this codebase already refuse to follow one. Reported here
        // rather than followed, and not resolved for a tip.
        if std::fs::symlink_metadata(heads.join(&name))
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            problems.push(Dangling {
                from_commit: format!("refs/heads/{name}"),
                kind: ObjectKind::Commit,
                missing: "(symbolic link, not a ref)".to_string(),
            });
            continue;
        }
        let tip = tip.trim();
        // `"0"` is the sentinel `init` writes before the first commit, and is
        // the only legitimate non-hash value. An *empty* ref is not a sentinel:
        // it is what a crash during a ref write leaves behind. Skipping it
        // silently removed a root, and with one branch that emptied the root set
        // and made every object in the repository look collectable.
        if tip == NO_COMMIT {
            continue;
        }
        if tip.is_empty() {
            problems.push(Dangling {
                from_commit: format!("refs/heads/{name}"),
                kind: ObjectKind::Commit,
                missing: "(empty ref file)".to_string(),
            });
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

    // The window the double read exists for: a rename landing here used to be
    // invisible to both reads. Test-only seam; a no-op in a release build.
    between_ref_reads(repo_path);

    // Soft-deleted branches are recoverable, so their commits are live.
    for h in soft_deleted_roots(repo_path, &mut problems, blind) {
        out.push(h);
    }

    // Second read of the branch store, union'd with the first.
    //
    // A branch lives in exactly one namespace at a time, and both `branch -d`
    // and `branch --restore` move it by a single rename -- in opposite
    // directions. So no read ORDER is safe against both: heads-then-deleted
    // misses a restore that lands between the two reads (gone from heads, gone
    // from deleted), and deleted-then-heads misses a delete the same way. The
    // fix is not to reorder but to read one namespace twice: a branch absent
    // from all of heads, deleted, and heads-again would have to have moved
    // deleted->heads and then heads->deleted inside one walk, which takes two
    // operations rather than one.
    //
    // Roots only. The pass above owns the reporting -- a symlinked ref, an empty
    // ref, an unparsable tip -- and repeating it here would show every such
    // finding twice and send the reader looking for a second fault. This pass
    // exists to avoid dropping a root, so it collects tips and says nothing.
    //
    // Not atomicity. The marked set is advisory and already stale by the time a
    // collector reads `plan.json`, so `gfs gc` must re-validate under the
    // repository lock immediately before each unlink regardless -- RFC 009 D4
    // requires exactly that for mtime, and reachability needs the same. This
    // narrows the report's hole; it does not close the gap between mark and
    // sweep, and nothing here should be read as if it did.
    // Deliberately unconditional: no `named.contains` skip. Guarding on "the
    // first pass already saw this branch" makes the whole pass dead code in every
    // run without a concurrent rename, so the one path that exists to prevent a
    // lost root would ship unexercised. Measured with a probe under
    // `--nocapture`: guarded, 0 executions across the fsck suite; unconditional,
    // 64. Running it always costs one `normalise_hash` per branch, and `out` is
    // sorted and deduped below, so a tip seen twice is still one root.
    if let Ok(again) = repo_layout::list_branches(repo_path) {
        for (_name, tip) in again {
            let tip = tip.trim();
            if tip == NO_COMMIT || tip.is_empty() {
                continue;
            }
            if let Some(h) = normalise_hash(tip) {
                out.push(h);
            }
        }
    }

    // HEAD is read raw rather than resolved. An *attached* HEAD names a branch
    // whose tip the loop above already covered, so resolving it would report a
    // broken branch twice — once against the ref and once against HEAD. Only a
    // detached HEAD contributes a root the branches do not.
    let head_path = repo_path.join(GFS_DIR).join(HEAD_FILE);
    let head_raw = match std::fs::read_to_string(&head_path) {
        Ok(h) => h.trim().to_string(),
        // Every failure here is a hole, absence included. Elsewhere NotFound is a
        // real answer -- `refs/deleted` does not exist until a branch is soft-deleted
        // -- but `init` always writes HEAD, so there is no repository in which it is
        // legitimately missing. A HEAD we did not read may have been detached, and a
        // detached HEAD is the only root its commit has, so treating the failure as
        // "no detached HEAD" drops that root and offers a live commit for collection.
        // That is the whole root set gone on one absent file, reported as a clean
        // walk, because from here nothing looks wrong.
        Err(e) => {
            blind.at(repo_path, &head_path, format!("could not be read: {e}"));
            String::new()
        }
    };
    // An attached HEAD is skipped below because the branch loop already covered
    // its tip -- but only if that branch was actually listed. When the ref is
    // gone and HEAD still names it, nothing covered it, and the commit it
    // pointed at has lost its only root while the walk still reads as complete.
    if let Some(branch) = head_raw
        .strip_prefix("ref:")
        .map(str::trim)
        .and_then(|r| r.strip_prefix(&format!("{REFS_DIR}/{HEADS_DIR}/")))
        .map(str::trim)
        && !branch.is_empty()
        && !named.contains(branch)
    {
        blind.at(
            repo_path,
            &heads.join(branch),
            "is named by HEAD but was not found, so its history has no root",
        );
    }
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

/// Test-only seam: run something in the window between the two ref reads.
///
/// The race this guards against needs a rename to land between reading
/// `refs/heads` and reading `refs/deleted`. On an unmodified binary that window
/// is too narrow to hit -- a randomised search got 0 hits in 150 delete/restore cycles
/// against 64 runs -- so without a seam the fix is untestable. This compiles
/// only under `cfg(test)`: nothing is shipped, and the production build has no
/// branch here at all.
///
/// Keyed on the repository path because tests run in parallel and a bare global
/// would fire inside unrelated ones.
/// A repository to match, and what to run when the walk reaches the window.
#[cfg(test)]
type RefReadHook = (std::path::PathBuf, std::sync::Arc<dyn Fn() + Send + Sync>);

#[cfg(test)]
pub(crate) static BETWEEN_REF_READS: std::sync::Mutex<Option<RefReadHook>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn between_ref_reads(repo_path: &Path) {
    let hook = BETWEEN_REF_READS.lock().ok().and_then(|g| {
        g.as_ref()
            .filter(|(p, _)| p == repo_path)
            .map(|(_, f)| f.clone())
    });
    if let Some(f) = hook {
        f();
    }
}

#[cfg(not(test))]
#[inline(always)]
fn between_ref_reads(_repo_path: &Path) {}

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
fn soft_deleted_roots(
    repo_path: &Path,
    problems: &mut Vec<Dangling>,
    blind: &mut Blind,
) -> Vec<String> {
    let base = repo_path
        .join(GFS_DIR)
        .join(REFS_DIR)
        .join(DELETED_REFS_DIR);
    let mut out = Vec::new();
    // Absent before the branch-recovery work lands, which must be a no-op.
    let mut stack = vec![base];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            // Absent is a real answer: `refs/deleted` exists only once a branch
            // has been soft-deleted, so most repositories have none.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                // Unreadable is not. These are the roots that keep a
                // soft-deleted branch alive, and failing to list them silently
                // un-protects every branch under this directory.
                blind.at(repo_path, &dir, format!("could not be listed: {e}"));
                continue;
            }
        };
        for entry in entries {
            // Each of the three failures below used to `continue`. Every one of
            // them drops a root, and this is the function whose own contract is
            // that a collector acting on its report "would delete exactly the
            // data `gfs branch --restore` promises to give back". `roots` already
            // reports a symlinked `refs/heads` entry rather than skipping it;
            // these are the same input reaching the opposite answer.
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    blind.at(repo_path, &dir, format!("could not be walked: {e}"));
                    continue;
                }
            };
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                blind.at(repo_path, &path, "could not be stat'd");
                continue;
            };
            if meta.file_type().is_symlink() {
                blind.at(
                    repo_path,
                    &path,
                    "is a symbolic link, not a recovery record, so the branch it                      would have protected has no root",
                );
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
            } else {
                match std::fs::read_to_string(&path) {
                    Ok(body) if body.trim() == NO_COMMIT => {
                        // A branch deleted before its first commit. `roots` skips
                        // the same value for a live branch, and `restore_deleted_
                        // branch_ref` accepts it, so it is a tombstone with nothing
                        // behind it rather than a reference to a missing commit.
                        // Reporting it as dangling condemns a healthy repository
                        // and suppresses the whole collectable report with it.
                    }
                    Ok(body) => match normalise_hash(&body) {
                        Some(h) => out.push(h),
                        // Unreadable as a hash. Reported rather than dropped:
                        // silently ignoring it stops protecting a branch the
                        // user was told is restorable, which is the one thing
                        // this function exists to prevent.
                        None => problems.push(Dangling {
                            from_commit: path
                                .strip_prefix(repo_path)
                                .unwrap_or(&path)
                                .to_string_lossy()
                                .to_string(),
                            kind: ObjectKind::Commit,
                            missing: brief(&body),
                        }),
                    },
                    Err(_) => problems.push(Dangling {
                        from_commit: path
                            .strip_prefix(repo_path)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .to_string(),
                        kind: ObjectKind::Commit,
                        missing: "(unreadable)".to_string(),
                    }),
                }
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
    /// Exactly these snapshots exist and belong to this repository, as the
    /// backend reports them. Used for Kubernetes, where the adapter lists the
    /// `VolumeSnapshot`s carrying this repo's owner annotation and joins them
    /// to ZFS.
    ///
    /// Each carries whether it is finished, when it was made, and what it holds
    /// exclusively. All three matter: an unfinished snapshot is present but
    /// uncollectable, the creation time is the only clock this backend has, and
    /// an unmeasured size is not a zero one.
    Known(&'a HashMap<String, SnapshotFacts>),
    /// The backend could not be asked. Snapshots are neither verified nor
    /// reported, and the report says so rather than implying they are fine.
    Unavailable,
}

/// Every place this run could not see, and the one channel for saying so.
///
/// **The rule this type exists to enforce.** A question about the filesystem has
/// three answers — yes, no, and *I could not look* — but the standard library
/// hands back two. `Path::exists` is `false` for a directory you lack permission
/// to open; `read_dir(..).flatten()` drops the entries that failed; every
/// `unwrap_or_default()` on a read turns an unreadable file into an empty one.
/// Each of those collapses the third answer into the second, and in a
/// reachability check the second answer is the dangerous one: absent means
/// *collectable*, or *corrupt*.
///
/// That single fault produced four separate defects in this file before it was
/// named — an empty ref read as a sentinel, unverified snapshots reported as a
/// clean repository, an unopenable object reported as missing, and an
/// unparseable commit condemning its own ancestors. All four were fixed the same
/// way, so the fix is now the type: anything that reads the filesystem takes a
/// `&mut Blind`, and anywhere the answer is "could not look" says so here rather
/// than picking one of the other two.
///
/// Noting something here is not cosmetic. It forces exit 3 and suppresses the
/// collectable list, because a walk with a hole in it cannot name garbage — what
/// it could not see is exactly what would look unreferenced.
#[derive(Default)]
pub struct Blind {
    notes: Vec<Unrecognised>,
}

impl Blind {
    /// `what` names the thing, relative to the repository where possible.
    fn note(&mut self, what: impl Into<String>, why: impl Into<String>) {
        let what = what.into();
        // One thing we could not look at is one finding, however many walks trip
        // over it. Two paths reach the same unreadable object -- the mark phase
        // and the store scan -- and listing it twice reads as two problems, which
        // sends the reader looking for a second fault that does not exist. First
        // reason wins: it comes from the walk that needed the object, so it says
        // what the failure cost.
        if self.notes.iter().any(|n| n.hash == what) {
            return;
        }
        self.notes.push(Unrecognised {
            hash: what,
            reason: why.into(),
            bytes: 0,
        });
    }

    fn at(&mut self, repo_path: &Path, path: &Path, why: impl Into<String>) {
        let shown = path
            .strip_prefix(repo_path)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        self.note(shown, why);
    }
}

/// Whether a thing is there, absent, or beyond our reach.
///
/// The third case used to collapse into the second, because `Path::exists`
/// answers `false` for both. An intact object inside a directory the caller
/// cannot open was therefore reported as *missing* — corruption — and on a host
/// where repositories are root-owned that is what a non-root operator is told
/// about a perfectly healthy repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    Present,
    Absent,
    Unreadable,
}

fn presence(path: &Path) -> Presence {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Presence::Absent,
        // Permission denied, a broken mount, an I/O error: all mean we did not
        // get to look, which is not the same as having looked and found nothing.
        Err(_) => return Presence::Unreadable,
    };
    // `metadata` is `stat`, which answers about the directory entry and ignores
    // the file's own mode. A mode-000 file stats perfectly well and cannot be
    // opened — the exact case `Unreadable` exists to name — so `stat` alone makes
    // this a two-valued answer wearing a three-valued type. A file is therefore
    // probed by opening it. Directories are not: traversal failure surfaces at
    // the `read_dir` that needs it, and opening every directory here would cost a
    // syscall per entry to learn something no caller asks at this point.
    if meta.is_file() && std::fs::File::open(path).is_err() {
        return Presence::Unreadable;
    }
    Presence::Present
}

impl SnapshotSource<'_> {
    fn contains(&self, hash: &str, snapshots_dir: &Path) -> Presence {
        match self {
            SnapshotSource::Filesystem => {
                let (a, b) = hash.split_at(2);
                presence(&snapshots_dir.join(a).join(b))
            }
            // Present is present: a snapshot the backend is still writing is
            // not a missing one, so a commit naming it is not dangling. The map
            // deliberately carries unfinished snapshots for this reason; the
            // sweep below is where readiness matters.
            SnapshotSource::Known(map) => {
                if map.contains_key(hash) {
                    Presence::Present
                } else {
                    Presence::Absent
                }
            }
            // Nothing is provably missing when nothing can be asked.
            SnapshotSource::Unavailable => Presence::Present,
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
    match repo_layout::get_runtime_config(repo_path) {
        // No runtime section: a local repository, whose snapshots are
        // directories.
        Ok(None) => SnapshotSource::Filesystem,
        Ok(Some(r)) => {
            let p = r.runtime_provider.trim().to_ascii_lowercase();
            if p == "kubernetes" || p == "k8s" {
                SnapshotSource::Unavailable
            } else {
                SnapshotSource::Filesystem
            }
        }
        // Unreadable config, so the backend is unknown. Not `Filesystem`:
        // guessing wrong in that direction walks a directory that a Kubernetes
        // repository never creates, finds nothing, and reports every commit in
        // the repository as dangling — corruption invented out of an unreadable
        // config file. `Unavailable` says the snapshots were not verified, which
        // is exactly what happened.
        Err(_) => SnapshotSource::Unavailable,
    }
}

/// What a marking walk reached, kept so the sweep can subtract it from disk.
#[derive(Default)]
struct Marks {
    objects: HashSet<String>,
    snapshots: HashSet<String>,
    commits: usize,
    file_lists: usize,
    /// Places the walk could not continue through.
    ///
    /// Only a *commit* can truncate a walk, because only a commit has children
    /// to follow. A missing file list or snapshot is a leaf: the commit naming
    /// it was still read, its parents were still queued, and reachability is
    /// unaffected. A commit that will not parse is different — everything
    /// behind it becomes invisible, and therefore looks collectable.
    truncated: usize,
    /// Objects reached and marked, with the kind the referring commit said they
    /// were. Validated after the walk; see [`check_with`].
    expected: Vec<(String, PathBuf, ObjectKind)>,
    /// For each reachable commit: the snapshot it names, and the file list that
    /// records what that snapshot should contain.
    ///
    /// The pair is needed together. An empty snapshot directory on its own
    /// proves nothing — a commit of an empty data directory legitimately has
    /// one, and flagging it made a freshly initialised repository report
    /// corruption. It is only wrong when the commit's own file list says files
    /// should be there.
    snapshot_contents: Vec<(String, String, Option<String>)>,
}

/// Walk from `roots`, marking everything reachable and collecting dangling
/// references found on the way.
fn mark(
    repo_path: &Path,
    roots: &[String],
    snapshots: &SnapshotSource<'_>,
    dangling: &mut Vec<Dangling>,
    blind: &mut Blind,
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
            // Absent is corruption; unreachable-by-permissions is not.
            Err(_)
                if presence(&objects_dir.join(&hash[..2]).join(&hash[2..]))
                    == Presence::Unreadable =>
            {
                blind.note(hash.clone(), "commit object could not be read");
                continue;
            }
            Err(_) => {
                marks.truncated += 1;
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
                    match snapshots.contains(&snap, &snapshots_dir) {
                        // Marked in both cases: it is there, we simply could not
                        // open it, and marking keeps the sweep from proposing it
                        // for collection.
                        found @ (Presence::Present | Presence::Unreadable) => {
                            if found == Presence::Unreadable {
                                blind.note(snap.clone(), "snapshot could not be read");
                            }
                            marks.snapshot_contents.push((
                                hash.clone(),
                                snap.clone(),
                                commit
                                    .files_ref
                                    .as_deref()
                                    .map(str::trim)
                                    .filter(|r| !r.is_empty())
                                    .and_then(normalise_hash),
                            ));
                            marks.snapshots.insert(snap);
                        }
                        Presence::Absent => dangling.push(Dangling {
                            from_commit: hash.clone(),
                            kind: ObjectKind::Snapshot,
                            missing: snap,
                        }),
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
                    match presence(&objects_dir.join(a).join(b)) {
                        Presence::Present => {
                            if kind == ObjectKind::FileList {
                                marks.file_lists += 1;
                            }
                            marks.expected.push((
                                reference.clone(),
                                objects_dir.join(a).join(b),
                                kind,
                            ));
                            marks.objects.insert(reference);
                        }
                        Presence::Absent => dangling.push(Dangling {
                            from_commit: hash.clone(),
                            kind,
                            missing: reference,
                        }),
                        Presence::Unreadable => {
                            blind.note(reference, format!("{} could not be read", kind.as_str()));
                        }
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
                None => {
                    marks.truncated += 1;
                    dangling.push(Dangling {
                        from_commit: hash.clone(),
                        kind: ObjectKind::Commit,
                        missing: brief(parent),
                    });
                }
            }
        }
    }

    marks
}

/// Identify an unmarked entry in the object store.
/// Why an entry in the object store could not be named.
///
/// The two are not interchangeable and collapsing them is the fault this module
/// exists to avoid. `Malformed` is a claim about the bytes: we read them and they
/// are not an object. `Unreadable` is a claim about *us*: the bytes may be
/// perfectly intact and we never saw them, which is a hole in the walk and not
/// corruption in the repository.
enum Unidentified {
    Unreadable(String),
    Malformed(String),
}

fn identify_object(path: &Path) -> Result<ObjectKind, Unidentified> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => {
            return if path.join("schema.json").is_file() {
                Ok(ObjectKind::Schema)
            } else {
                Err(Unidentified::Malformed(
                    "directory in the object store with no schema.json".to_string(),
                ))
            };
        }
        Ok(_) => {}
        Err(e) => return Err(Unidentified::Unreadable(format!("could not be read: {e}"))),
    }
    // `metadata` is `stat`, which answers about the directory entry and ignores
    // the file's own mode, so a mode-000 object passes the check above and fails
    // here. Reading is the only probe that settles it, which is why the read
    // error is classified rather than folded into "not an object".
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return Err(Unidentified::Unreadable(format!("could not be read: {e}"))),
    };
    if serde_json::from_slice::<Commit>(&bytes).is_ok() {
        return Ok(ObjectKind::Commit);
    }
    if repo_layout::decode_file_entries(&bytes).is_ok() {
        return Ok(ObjectKind::FileList);
    }
    Err(Unidentified::Malformed(
        "not a commit, file list or schema object".to_string(),
    ))
}

/// Whether `path` was modified after `cutoff`, and so is protected.
///
/// An unreadable mtime counts as protected: refusing to collect something that
/// cannot be dated is the safe direction.
/// `None` means no window at all, so nothing is protected — including an entry
/// whose mtime is in the future, which `--grace 0` must be able to override.
/// A window so large that the cutoff saturates at the epoch protects
/// everything, which is the correct reading of "keep for a thousand years".
/// Whether a backend-supplied creation time falls inside the grace window.
///
/// An absent time counts as recent. Same direction as the filesystem walk: what
/// cannot be dated cannot be shown to be old, and the cost of being wrong is
/// asymmetric — protecting garbage wastes space, collecting live data does not.
fn within_grace(created: Option<SystemTime>, cutoff: Option<SystemTime>) -> bool {
    let Some(cutoff) = cutoff else {
        return false;
    };
    created.map(|t| t > cutoff).unwrap_or(true)
}

fn newer_than(path: &Path, cutoff: Option<SystemTime>) -> bool {
    let Some(cutoff) = cutoff else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(path) else {
        // Cannot be dated, so cannot be shown to be old.
        return true;
    };
    let mtime = meta.modified().ok();

    // `ctime`, not just `mtime`, and this is the whole point. A snapshot is made
    // by copying a data directory, and both `cp -cRp` and a clone preserve the
    // *source's* mtime — so a snapshot taken seconds ago inherits whatever mtime
    // the database directory happened to carry, which on a repository that has
    // been running a while is hours or days old. The snapshot is then born
    // already outside its own grace window, and the protection that exists
    // specifically to cover an in-flight commit does not cover it.
    //
    // Measured: with an aged data directory, 14% of concurrent fsck runs
    // reported a live in-flight snapshot as collectable at the default grace.
    // With a freshly created one, 0%.
    //
    // `ctime` is the inode's own change time. It cannot be back-dated by a copy,
    // so a newly created entry always carries a recent one. The later of the two
    // is used because either can be the meaningful one: mtime catches content
    // written after creation, ctime catches creation itself.
    #[cfg(unix)]
    let ctime = {
        use std::os::unix::fs::MetadataExt;
        let secs = meta.ctime();
        if secs >= 0 {
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64))
        } else {
            None
        }
    };
    #[cfg(not(unix))]
    let ctime: Option<SystemTime> = None;

    match (mtime, ctime) {
        (Some(m), Some(c)) => m.max(c) > cutoff,
        (Some(t), None) | (None, Some(t)) => t > cutoff,
        // Undatable: protected, which is the safe direction.
        (None, None) => true,
    }
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
    let mut blind = Blind::default();
    let (roots, mut dangling) = roots(repo_path, &mut blind)?;
    // A root that does not resolve is not one missing answer among many: the
    // walk never started from where it should have, so *every* object that root
    // led to now looks unreached. Reachability is therefore unsound for this
    // run, and the unreachable set is suppressed below rather than printed —
    // a wrong answer presented confidently is the failure mode a check exists
    // to prevent. Same reasoning as `snapshots_checked`.
    let reachability_complete = dangling.is_empty();
    let marks = mark(repo_path, &roots, snapshots, &mut dangling, &mut blind);

    let mut unreachable: Vec<Unreachable> = Vec::new();
    let mut unrecognised: Vec<Unrecognised> = Vec::new();
    let mut referenced_bytes: u64 = 0;
    let mut exclusive_bytes: u64 = 0;
    let mut protected: usize = 0;

    let (object_leaves, object_junk) = two_level(repo_path, &objects_dir, &mut blind);
    for path in object_junk {
        unrecognised.push(Unrecognised {
            hash: path
                .strip_prefix(repo_path)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string(),
            reason: "unexpected entry in the object store".to_string(),
            bytes: object_size(&path),
        });
    }
    for (hash, path) in object_leaves {
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
            Err(Unidentified::Malformed(reason)) => unrecognised.push(Unrecognised {
                hash,
                reason,
                bytes,
            }),
            // Not corruption: an object we could not open may be intact, and
            // calling it corrupt sends the reader to repair a file that is fine.
            Err(Unidentified::Unreadable(reason)) => blind.note(hash, reason),
        }
    }

    // Live objects, checked. The store scan below only looks at what is *not*
    // marked, so until now a reachable file list could be replaced with
    // arbitrary bytes and the report would call the repository consistent: the
    // one object nobody was allowed to lose was the one nobody looked at.
    //
    // This is the cheap half of content checking and belongs here: it costs one
    // parse per referenced object, so it scales with history, which is what the
    // rest of this walk already scales with. Verifying that the *files inside* a
    // snapshot still match their recorded entries scales with data size instead,
    // and stays out of scope.
    for (hash, path, expected) in &marks.expected {
        match identify_object(path) {
            Ok(actual) if actual == *expected => {}
            Ok(actual) => unrecognised.push(Unrecognised {
                hash: hash.clone(),
                reason: format!(
                    "referenced as {} but stored as {}",
                    expected.as_str(),
                    actual.as_str()
                ),
                bytes: object_size(path),
            }),
            Err(Unidentified::Malformed(reason)) => unrecognised.push(Unrecognised {
                hash: hash.clone(),
                reason: format!("referenced as {} but {reason}", expected.as_str()),
                bytes: object_size(path),
            }),
            Err(Unidentified::Unreadable(reason)) => blind.note(
                hash.clone(),
                format!("referenced as {} but {reason}", expected.as_str()),
            ),
        }
    }

    // A snapshot directory that exists but holds nothing, *when its commit says
    // it should hold something*. The file list is the only thing that can tell
    // the two apart: a commit of an empty data directory has an empty snapshot
    // and an empty file list, and is perfectly sound.
    if matches!(snapshots, SnapshotSource::Filesystem) {
        for (commit, snap, files_ref) in &marks.snapshot_contents {
            let (a, b) = snap.split_at(2);
            let dir = snapshots_dir.join(a).join(b);
            let empty = match std::fs::read_dir(&dir) {
                Ok(mut e) => e.next().is_none(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => {
                    blind.at(repo_path, &dir, format!("could not be listed: {e}"));
                    false
                }
            };
            if !empty {
                continue;
            }
            let Some(files_ref) = files_ref else {
                continue;
            };
            let (fa, fb) = files_ref.split_at(2);
            let expected_files = std::fs::read(objects_dir.join(fa).join(fb))
                .ok()
                .and_then(|b| repo_layout::decode_file_entries(&b).ok())
                .map(|entries| entries.len())
                .unwrap_or(0);
            if expected_files > 0 {
                dangling.push(Dangling {
                    from_commit: commit.clone(),
                    kind: ObjectKind::Snapshot,
                    missing: format!(
                        "{snap} (directory is empty, but {expected_files} file(s) recorded)"
                    ),
                });
            }
        }
    }

    let mut checked_snapshots = 0usize;
    // Set when the backend held a snapshot it could not size. The total is then
    // a floor over an unknown remainder rather than a complete figure, and
    // saying so is the difference between "nothing to reclaim" and "we could
    // not measure".
    let mut exclusive_incomplete = false;
    if let SnapshotSource::Known(map) = snapshots {
        // The backend is the inventory, and it also knows what each snapshot
        // exclusively holds — which a directory walk cannot. These bytes are
        // the real thing: what deleting it would free, not what `du` shows.
        for (hash, facts) in map.iter() {
            checked_snapshots += 1;
            if marks.snapshots.contains(hash) {
                continue;
            }
            // Unfinished, so an operation is still writing it. Not garbage,
            // whatever its age — the backend is telling us it is in flight.
            if !facts.ready {
                protected += 1;
                continue;
            }
            // The grace window applies here exactly as it does to a directory,
            // and used not to apply at all: this branch read no clock, so on the
            // Kubernetes backend `--grace` did nothing whatsoever and a snapshot
            // seconds old was offered for collection every single time.
            if within_grace(facts.created, cutoff) {
                protected += 1;
                continue;
            }
            match facts.bytes {
                Some(b) => exclusive_bytes += b,
                None => exclusive_incomplete = true,
            }
            unreachable.push(Unreachable {
                kind: ObjectKind::Snapshot,
                hash: hash.clone(),
                summary: None,
                bytes: facts.bytes.unwrap_or(0),
            });
        }
    } else if check_snapshots {
        let (snapshot_trees, snapshot_junk) = two_level(repo_path, &snapshots_dir, &mut blind);
        for path in snapshot_junk {
            unrecognised.push(Unrecognised {
                hash: path
                    .strip_prefix(repo_path)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string(),
                reason: "unexpected entry in the snapshot store".to_string(),
                bytes: 0,
            });
        }
        for (hash, path) in snapshot_trees {
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

    // Not `unwrap_or_default()`. An empty set here does not mean "no branches";
    // on a failed read it means "we could not tell", and every workspace would
    // then look like one whose branch is gone — the whole list reported as
    // reclaimable because a directory would not open.
    let live_branches: HashSet<String> = match repo_layout::list_branches(repo_path) {
        Ok(b) => b.into_iter().map(|(name, _)| name).collect(),
        Err(e) => {
            blind.note("refs/heads", format!("could not be listed: {e}"));
            HashSet::new()
        }
    };
    let (reclaimable_workspaces, reclaimable_workspace_bytes, ws_protected, ws_unreadable) =
        reclaimable_workspaces(
            repo_path,
            &live_branches,
            &marks.objects,
            cutoff,
            &mut blind,
        );
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

    // Computed, then discarded: the walk had to finish before we could know
    // what an intact root would have reached, and the arithmetic is cheap
    // beside the risk of shipping a list nobody should act on.
    let (mut unreachable, mut referenced_bytes, mut exclusive_bytes) =
        (unreachable, referenced_bytes, exclusive_bytes);
    let (mut reclaimable_workspaces, mut reclaimable_workspace_bytes) =
        (reclaimable_workspaces, reclaimable_workspace_bytes);
    // An object we could not open is a hole in the walk just as much as a root
    // we could not resolve: whatever it referenced is now unmarked and looks
    // collectable.
    // A commit the walk could not read hides everything behind it, exactly as a
    // ref it could not resolve does. Both make the unreachable set a list of
    // live data, so both suppress it.
    let reachability_complete =
        reachability_complete && blind.notes.is_empty() && marks.truncated == 0;
    if !reachability_complete {
        unreachable.clear();
        reclaimable_workspaces.clear();
        referenced_bytes = 0;
        exclusive_bytes = 0;
        reclaimable_workspace_bytes = 0;
        // A count of what was held back from a list that is no longer reported.
        protected = 0;
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
        exclusive_is_partial: exclusive_incomplete,
        snapshots_checked: check_snapshots,
        reachability_complete,
        unreadable: blind.notes,
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
    blind: &mut Blind,
) -> (Vec<ReclaimableWorkspace>, u64, usize, Vec<String>) {
    let root = repo_path.join(GFS_DIR).join(WORKSPACES_DIR);
    // The one file that says which workspace is live. Unreadable used to become
    // an empty path, matching nothing, so the checked-out workspace was reported
    // as unneeded — the single most valuable directory in the repository.
    let active_path = repo_path.join(GFS_DIR).join(WORKSPACE_FILE);
    let active = match std::fs::read_to_string(&active_path) {
        Ok(s) => PathBuf::from(s.trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => PathBuf::new(),
        Err(e) => {
            blind.at(repo_path, &active_path, format!("could not be read: {e}"));
            PathBuf::new()
        }
    };

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
                    safe_to_remove: false,
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
            safe_to_remove: false,
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

    /// A shard is a directory GFS created. A symlink in its place is an anomaly,
    /// and following it walks the object store out of the repository -- naming
    /// paths under a directory that only *looks* like it is inside, and putting
    /// them in front of whatever consumes the report.
    #[test]
    fn a_symlinked_shard_is_named_and_not_followed() {
        let d = repo();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("precious.txt"), b"not mine").unwrap();
        let shard = d.path().join(GFS_DIR).join(OBJECTS_DIR).join("ab");
        std::os::unix::fs::symlink(outside.path(), &shard).unwrap();

        let r = check(d.path(), Duration::ZERO).unwrap();

        assert_eq!(
            r.unrecognised.len(),
            1,
            "the link itself is the finding: {r:?}"
        );
        assert!(
            r.unrecognised[0].hash.ends_with("ab"),
            "names the shard, not what is behind it: {:?}",
            r.unrecognised[0].hash
        );
        let all = format!("{r:?}");
        assert!(
            !all.contains("precious"),
            "nothing outside the repository may appear in the report: {all}"
        );
    }

    /// `is_dir()` is false both for "not a directory" and for "could not stat
    /// it", so an object store the process cannot traverse (mode 444: readable,
    /// not executable) turned every well-formed shard into corruption. The
    /// reader got a list of objects to repair that were all perfectly intact.
    ///
    /// Root ignores mode bits, so this announces a skip rather than passing
    /// vacuously.
    #[test]
    fn shards_behind_a_dir_we_cannot_traverse_are_a_hole_not_corruption() {
        use std::os::unix::fs::PermissionsExt;

        let d = repo();
        let hash = write_commit(d.path(), "b", "c", None, true);
        set_branch(d.path(), "main", &hash);
        let objects = d.path().join(GFS_DIR).join(OBJECTS_DIR);

        fs::set_permissions(&objects, fs::Permissions::from_mode(0o444)).unwrap();
        let blocked = fs::metadata(objects.join(&hash[..2])).is_err();
        if !blocked {
            let _ = fs::set_permissions(&objects, fs::Permissions::from_mode(0o755));
            eprintln!("SKIP: this uid can traverse a mode-444 directory (root); nothing exercised");
            return;
        }

        let r = check(d.path(), Duration::ZERO).unwrap();
        let _ = fs::set_permissions(&objects, fs::Permissions::from_mode(0o755));

        assert!(
            r.unrecognised.is_empty(),
            "a shard we could not stat is not an unexpected entry: {:?}",
            r.unrecognised
        );
        assert!(!r.reachability_complete);
        assert_eq!(r.exit_code(), 3, "could-not-complete, not corruption");
    }

    /// An object we cannot open is not an object we looked at and found broken.
    /// `stat` succeeds on a mode-000 file, so a check built on `metadata` alone
    /// calls it present, fails to read it, and reports corruption: a dangling
    /// entry naming itself, plus an "unrecognised" line about a file that may be
    /// perfectly intact. It sends the reader to repair something that is fine.
    ///
    /// Root ignores permission bits, so this cannot be written to pass under
    /// every uid. It announces the skip instead of passing vacuously — a green
    /// test that exercised nothing is worse than an absent one.
    #[test]
    fn an_object_that_stats_but_will_not_open_is_a_hole_not_corruption() {
        use std::os::unix::fs::PermissionsExt;

        let d = repo();
        let hash = write_commit(d.path(), "e", "unopenable", None, true);
        set_branch(d.path(), "main", &hash);
        let obj = d
            .path()
            .join(GFS_DIR)
            .join(OBJECTS_DIR)
            .join(&hash[..2])
            .join(&hash[2..]);

        fs::set_permissions(&obj, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&obj).is_ok() {
            eprintln!("SKIP: running as a uid that ignores mode bits (root); nothing exercised");
            return;
        }
        assert!(
            fs::metadata(&obj).is_ok(),
            "precondition: stat still succeeds"
        );

        let r = check(d.path(), Duration::ZERO).unwrap();
        let _ = fs::set_permissions(&obj, fs::Permissions::from_mode(0o644));

        assert!(
            r.dangling.is_empty(),
            "an unreadable object is not a commit referencing something missing: {:?}",
            r.dangling
        );
        assert!(
            r.unrecognised.is_empty(),
            "nor is it an entry we read and could not name: {:?}",
            r.unrecognised
        );
        assert!(
            !r.reachability_complete,
            "the walk did not reach everything"
        );
        assert!(r.unreachable.is_empty(), "and so may propose nothing");
    }

    /// An absent HEAD is corruption, not a repository that happens to have no
    /// HEAD: `init` always writes one. It is also the only root whose loss empties
    /// the root set silently -- a detached HEAD is the sole root its commit has, so
    /// reading the absence as "nothing is detached" offers that live commit, its
    /// file list and its snapshot for collection, and reports the walk as complete
    /// while doing it.
    ///
    /// This is the one place the "NotFound is a real answer" rule does not hold.
    /// It holds for `refs/deleted`, which does not exist until a branch is
    /// soft-deleted; it never holds for HEAD.
    #[test]
    fn an_absent_head_is_a_hole_not_an_empty_answer() {
        let d = repo();
        let detached = write_commit(d.path(), "d", "reachable only via HEAD", None, true);
        fs::write(d.path().join(GFS_DIR).join("HEAD"), &detached).unwrap();

        let before = check(d.path(), Duration::ZERO).unwrap();
        assert_eq!(
            before.unreachable.len(),
            0,
            "precondition: nothing is garbage"
        );

        fs::remove_file(d.path().join(GFS_DIR).join("HEAD")).unwrap();
        let after = check(d.path(), Duration::ZERO).unwrap();

        assert!(
            !after.reachability_complete,
            "the walk did not start from every root, and must not claim it did"
        );
        assert!(
            after.unreachable.is_empty(),
            "nothing may be offered for collection when a root was never read: {:?}",
            after.unreachable
        );
        assert_eq!(
            after.exit_code(),
            3,
            "could-not-complete, not clean and not garbage"
        );
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
        // But emphatically not clean, and this assertion is the inverse of what
        // it used to be. A run that could not see the snapshots has checked half
        // the graph; calling that consistent is how the same repository reported
        // "inconsistent" with the cluster up and a green tick with it down.
        assert!(
            !r.is_clean(),
            "an unverified half is not a clean bill: {r:?}"
        );
        assert_eq!(
            r.exit_code(),
            3,
            "did-not-run is not the same as found-garbage"
        );
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

    /// The race the double read exists for, made deterministic.
    ///
    /// A `branch --restore` renames `refs/deleted/<ms>/x` to `refs/heads/x`. If
    /// that lands after fsck has read `refs/heads` and before it reads
    /// `refs/deleted`, the branch is in neither snapshot and its whole history
    /// loses its only root -- while the walk still reports itself complete. The
    /// seam places the rename exactly there instead of racing for it from
    /// outside, which a randomised search could not do in 150 attempts.
    #[test]
    fn a_restore_landing_between_the_ref_reads_does_not_unroot_its_history() {
        let d = repo();
        let main_tip = write_commit(d.path(), "aa", "on main", None, true);
        set_branch(d.path(), "main", &main_tip);
        let doomed_tip = write_commit(d.path(), "bb", "on doomed", None, true);

        // A tombstone, as `branch -d` leaves one. `doomed` is NOT in refs/heads.
        let stamp = d
            .path()
            .join(GFS_DIR)
            .join(REFS_DIR)
            .join(DELETED_REFS_DIR)
            .join("1791242262279");
        fs::create_dir_all(&stamp).unwrap();
        fs::write(stamp.join("doomed"), &doomed_tip).unwrap();

        // The restore, fired from inside the window: deleted -> heads.
        let from = stamp.join("doomed");
        let to = d
            .path()
            .join(GFS_DIR)
            .join(REFS_DIR)
            .join(HEADS_DIR)
            .join("doomed");
        let hook: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(move || {
            let _ = fs::rename(&from, &to);
        });
        *BETWEEN_REF_READS.lock().unwrap() = Some((d.path().to_path_buf(), hook));

        let report = check(d.path(), Duration::ZERO).unwrap();

        // Clear before asserting, so a failure cannot leak the hook into the
        // rest of the suite.
        *BETWEEN_REF_READS.lock().unwrap() = None;

        // The rename really happened, or the test proved nothing.
        assert!(
            d.path()
                .join(GFS_DIR)
                .join(REFS_DIR)
                .join(HEADS_DIR)
                .join("doomed")
                .exists(),
            "precondition: the seam must have performed the restore"
        );
        assert!(
            !report
                .unreachable
                .iter()
                .any(|u| doomed_tip.starts_with(&u.hash[..8.min(u.hash.len())])),
            "a branch restored mid-walk was offered for collection: {:?}",
            report.unreachable
        );
    }

    /// The second branch read must not turn one finding into two. The first pass
    /// owns reporting; this guards against the tempting "just report in both",
    /// which was confirmed to fail this test rather than assumed to.
    #[test]
    fn reading_the_branch_store_twice_reports_each_finding_once() {
        let d = repo();
        let live = write_commit(d.path(), "aa", "on main", None, true);
        set_branch(d.path(), "main", &live);
        // A ref whose tip is not a hash: one finding, however many times the
        // store is walked.
        set_branch(d.path(), "broken", "not-a-hash");

        let report = check(d.path(), Duration::ZERO).unwrap();
        let against_broken = report
            .dangling
            .iter()
            .filter(|dg| dg.from_commit.contains("broken"))
            .count();
        assert_eq!(
            against_broken, 1,
            "expected exactly one finding for the broken ref, got {:?}",
            report.dangling
        );
    }

    /// And it must not invent roots: a tip seen twice is still one root, so the
    /// reachable set is unchanged by the extra walk.
    #[test]
    fn reading_the_branch_store_twice_does_not_change_what_is_reachable() {
        let d = repo();
        let a = write_commit(d.path(), "aa", "on main", None, true);
        let b = write_commit(d.path(), "bb", "on side", None, true);
        set_branch(d.path(), "main", &a);
        set_branch(d.path(), "side", &b);
        let orphan = write_commit(d.path(), "cc", "unreferenced", None, true);

        let report = check(d.path(), Duration::ZERO).unwrap();
        let collectable: Vec<&str> = report
            .unreachable
            .iter()
            .map(|u| u.hash.as_str())
            .filter(|h| orphan.starts_with(&h[..8.min(h.len())]))
            .collect();
        assert!(
            !collectable.is_empty(),
            "the orphan must still be reported: {:?}",
            report.unreachable
        );
        // Both live tips stay out of the collectable set.
        for live in [&a, &b] {
            assert!(
                !report
                    .unreachable
                    .iter()
                    .any(|u| live.starts_with(&u.hash[..8.min(u.hash.len())])),
                "a live tip was offered for collection"
            );
        }
    }

    /// An absent branch store is a hole, not a repository with no branches.
    /// `init` always writes `refs/heads/main`, so this is a partial copy -- and
    /// it is exactly when every object looks unreferenced.
    #[test]
    fn an_absent_branch_store_suppresses_the_collectable_list() {
        let d = repo();
        let live = write_commit(d.path(), "aa", "on main", None, true);
        set_branch(d.path(), "main", &live);
        let orphan = write_commit(d.path(), "bb", "genuinely unreachable", None, true);

        // With the store intact the orphan is reported, which is what makes the
        // assertion below mean something rather than passing on an empty set.
        let before = check(d.path(), Duration::ZERO).unwrap();
        assert!(
            before
                .unreachable
                .iter()
                .any(|u| u.hash.starts_with(&orphan[..8])),
            "precondition: the orphan must be collectable while the store is intact"
        );

        // Detach HEAD onto the live commit first. With HEAD still attached to
        // `main`, moving the store aside is caught by the attached-HEAD guard
        // instead, and this test would pass with the absent-store guard removed
        // -- which per-site calibration showed it did.
        fs::write(d.path().join(GFS_DIR).join(HEAD_FILE), &live).unwrap();

        let heads = d.path().join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR);
        let aside = d.path().join("heads-aside");
        fs::rename(&heads, &aside).unwrap();

        let after = check(d.path(), Duration::ZERO).unwrap();
        assert!(
            after.unreachable.is_empty(),
            "a walk with no roots must name nothing, got {:?}",
            after.unreachable
        );
        assert!(
            !after.reachability_complete,
            "the walk must declare itself incomplete"
        );
    }

    /// An attached HEAD naming a branch that is not there lost its only root
    /// while the walk still read as complete.
    #[test]
    fn an_attached_head_whose_branch_is_gone_is_a_hole() {
        let d = repo();
        let live = write_commit(d.path(), "aa", "on main", None, true);
        set_branch(d.path(), "main", &live);
        fs::write(
            d.path().join(GFS_DIR).join(HEAD_FILE),
            format!("ref: {REFS_DIR}/{HEADS_DIR}/gone"),
        )
        .unwrap();

        let report = check(d.path(), Duration::ZERO).unwrap();
        assert!(
            !report.reachability_complete,
            "HEAD names a branch that was never listed, so the walk has a hole"
        );
        assert!(report.unreachable.is_empty(), "and it must name nothing");
    }

    /// `roots` reports a symlinked `refs/heads` entry; the soft-deleted path used
    /// to skip the identical input, unrooting a branch `--restore` still honours.
    #[test]
    fn a_symlinked_tombstone_is_reported_not_skipped() {
        let d = repo();
        let live = write_commit(d.path(), "aa", "on main", None, true);
        set_branch(d.path(), "main", &live);
        let doomed = write_commit(d.path(), "bb", "soft deleted", None, true);

        let dir = d
            .path()
            .join(GFS_DIR)
            .join(REFS_DIR)
            .join(DELETED_REFS_DIR)
            .join("1791242262279");
        fs::create_dir_all(&dir).unwrap();
        let holder = d.path().join("holder");
        fs::write(&holder, &doomed).unwrap();
        std::os::unix::fs::symlink(&holder, dir.join("doomed")).unwrap();

        let report = check(d.path(), Duration::ZERO).unwrap();
        assert!(
            !report.reachability_complete,
            "a tombstone we refused to follow is a hole, not an absence"
        );
        assert!(
            report.unreachable.is_empty(),
            "and nothing may be offered for collection, got {:?}",
            report.unreachable
        );
    }

    /// A tombstone can hold `NO_COMMIT`, and that is not a missing object.
    #[test]
    fn a_branch_deleted_before_its_first_commit_is_not_dangling() {
        let d = repo();
        let live = write_commit(d.path(), "aa", "on main", None, true);
        set_branch(d.path(), "main", &live);

        let entry = d
            .path()
            .join(GFS_DIR)
            .join(REFS_DIR)
            .join(DELETED_REFS_DIR)
            .join("1791237274625")
            .join("never-committed");
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        fs::write(&entry, NO_COMMIT).unwrap();

        let report = check(d.path(), Duration::ZERO).unwrap();
        assert!(
            report.is_clean(),
            "a tombstone with nothing behind it is not a reference to something missing"
        );
        assert!(
            report.dangling.is_empty(),
            "expected no dangling, got {:?}",
            report.dangling
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

    /// The whole-repository failure: one truncated ref and every object looks
    /// collectable. `: > .gfs/refs/heads/main` is what a crash mid-write leaves,
    /// and it used to be treated as the `"0"` sentinel — silently removing the
    /// only root, so the walk started from nowhere and reached nothing.
    #[test]
    fn an_empty_ref_is_corruption_not_a_reason_to_collect_everything() {
        let d = repo();
        let a = write_commit(d.path(), "aa", "first", None, true);
        let b = write_commit(d.path(), "bb", "second", Some(&a), true);
        set_branch(d.path(), "main", &b);

        // Sound to begin with.
        assert!(check(d.path(), Duration::ZERO).unwrap().is_clean());

        // The crash.
        fs::write(
            d.path()
                .join(GFS_DIR)
                .join(REFS_DIR)
                .join(HEADS_DIR)
                .join("main"),
            "",
        )
        .unwrap();

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert_eq!(
            r.exit_code(),
            2,
            "a truncated ref is corruption, and exit 2 is what stops `--plan`: {r:?}"
        );
        assert!(
            r.dangling.iter().any(|x| x.from_commit.contains("main")),
            "the report must name the broken ref: {:?}",
            r.dangling
        );
        assert!(
            !r.unreachable.iter().any(|u| u.hash == a || u.hash == b),
            "commits behind a broken ref must not be offered up for collection: {:?}",
            r.unreachable
        );
    }

    /// The same asymmetry one level down. A recovery record that cannot be read
    /// used to be dropped, which quietly stopped protecting a branch the user
    /// was told they could restore.
    #[test]
    fn an_unreadable_deleted_ref_is_reported_rather_than_dropped() {
        let d = repo();
        let live = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &live);

        let entry = d
            .path()
            .join(GFS_DIR)
            .join(REFS_DIR)
            .join(DELETED_REFS_DIR)
            .join("1788452232388")
            .join("feature");
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        fs::write(&entry, "not-a-hash").unwrap();

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert_eq!(r.exit_code(), 2, "unreadable recovery record: {r:?}");
        assert!(
            r.dangling.iter().any(|x| x.from_commit.contains("feature")),
            "must say which record is broken: {:?}",
            r.dangling
        );
    }

    /// Grace exists to cover an operation still in flight, and a snapshot is
    /// made by copying a data directory — which carries the *source's* mtime
    /// over. On a repository that has been running a while that mtime is hours
    /// old, so a snapshot taken a second ago was born outside its own grace
    /// window and got reported as collectable while it was still being written.
    #[cfg(unix)]
    #[test]
    fn a_snapshot_that_inherited_an_old_mtime_is_still_within_grace() {
        let d = repo();
        let live = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &live);
        // Unreferenced, so grace is the only thing that can protect it.
        let stranded = write_commit(d.path(), "bb", "in flight", None, true);

        // What a copy does: mtime back-dated, ctime untouched.
        let snap = hash_of(&format!("5{}", "bb"));
        for target in [
            d.path()
                .join(GFS_DIR)
                .join(OBJECTS_DIR)
                .join(&stranded[..2])
                .join(&stranded[2..]),
            d.path()
                .join(GFS_DIR)
                .join(SNAPSHOTS_DIR)
                .join(&snap[..2])
                .join(&snap[2..]),
        ] {
            let ok = std::process::Command::new("touch")
                .args(["-m", "-t", "202001010000"])
                .arg(&target)
                .status()
                .expect("touch")
                .success();
            assert!(ok, "could not back-date {}", target.display());
        }

        let r = check(d.path(), Duration::from_secs(3600)).unwrap();
        assert!(
            !r.unreachable.iter().any(|u| u.hash == stranded),
            "a just-created entry carrying a copied mtime is still in flight: {:?}",
            r.unreachable
        );
        assert!(r.protected_by_grace > 0, "{r:?}");

        // And the clock still works: with no window, it is collectable again.
        let none = check(d.path(), Duration::ZERO).unwrap();
        assert!(
            none.unreachable.iter().any(|u| u.hash == stranded),
            "zero grace must protect nothing: {:?}",
            none.unreachable
        );
    }

    /// A JSON consumer cannot read a doc comment, so the caveat that these are
    /// unsafe to remove has to be a field. It was previously only in prose.
    #[test]
    fn a_reported_working_copy_is_marked_unsafe_to_remove() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);
        let dir = d
            .path()
            .join(GFS_DIR)
            .join("workspaces")
            .join("deleted-branch")
            .join("0")
            .join("data");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("payload"), vec![0u8; 4096]).unwrap();

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert!(!r.reclaimable_workspaces.is_empty());
        assert!(
            r.reclaimable_workspaces.iter().all(|w| !w.safe_to_remove),
            "checkout preserves an existing workspace, so none of these are safe"
        );
        let json = serde_json::to_string(&r.reclaimable_workspaces[0]).unwrap();
        assert!(json.contains("\"safe_to_remove\":false"), "got {json}");
    }

    /// The cluster namespace is shared. Handing fsck a snapshot belonging to
    /// another deployment makes it garbage by definition — no local commit
    /// names it — so the scoping that keeps it out of the map is the only thing
    /// standing between a routine check and a plan to delete a live branch tip.
    #[test]
    fn a_pending_snapshot_is_present_but_never_collectable() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, false);
        set_branch(d.path(), "main", &h);
        let snap = hash_of(&format!("5{}", "aa"));

        // Mid-commit: the VolumeSnapshot exists, the backend is still writing it.
        let mut map = HashMap::new();
        map.insert(
            snap.clone(),
            SnapshotFacts {
                ready: false,
                created: Some(SystemTime::now()),
                bytes: None,
            },
        );
        let r = check_with(d.path(), Duration::ZERO, &SnapshotSource::Known(&map)).unwrap();
        assert!(
            r.dangling.is_empty(),
            "a snapshot being written is present, not missing: {:?}",
            r.dangling
        );
        assert!(
            !r.unreachable.iter().any(|u| u.hash == snap),
            "and it is in flight, so it is not garbage at any age: {:?}",
            r.unreachable
        );

        // Unreferenced *and* unfinished stays protected even with no grace.
        let orphan = hash_of("9f");
        let mut map2 = HashMap::new();
        map2.insert(
            orphan.clone(),
            SnapshotFacts {
                ready: false,
                created: None,
                bytes: None,
            },
        );
        let r2 = check_with(d.path(), Duration::ZERO, &SnapshotSource::Known(&map2)).unwrap();
        assert!(
            !r2.unreachable.iter().any(|u| u.hash == orphan),
            "{:?}",
            r2.unreachable
        );
        assert!(r2.protected_by_grace > 0);
    }

    /// `--grace` read no clock at all on this backend, so it did nothing: a
    /// snapshot created seconds ago was offered for collection on every run.
    #[test]
    fn grace_protects_a_freshly_created_backend_snapshot() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, false);
        set_branch(d.path(), "main", &h);

        let fresh = hash_of("7a");
        let ancient = hash_of("7b");
        let mut map = HashMap::new();
        map.insert(
            fresh.clone(),
            SnapshotFacts {
                ready: true,
                created: Some(SystemTime::now()),
                bytes: Some(1024),
            },
        );
        map.insert(
            ancient.clone(),
            SnapshotFacts {
                ready: true,
                created: Some(SystemTime::UNIX_EPOCH),
                bytes: Some(2048),
            },
        );

        let held = check_with(
            d.path(),
            Duration::from_secs(3600),
            &SnapshotSource::Known(&map),
        )
        .unwrap();
        let names: Vec<&str> = held.unreachable.iter().map(|u| u.hash.as_str()).collect();
        assert!(
            !names.contains(&fresh.as_str()),
            "seconds old, inside the window: {names:?}"
        );
        assert!(
            names.contains(&ancient.as_str()),
            "and the window is not simply protecting everything: {names:?}"
        );

        // Zero grace still means zero.
        let none = check_with(d.path(), Duration::ZERO, &SnapshotSource::Known(&map)).unwrap();
        assert_eq!(none.unreachable.len(), 2, "{:?}", none.unreachable);
    }

    /// An unmeasured size is not a zero one. Where the CLI can reach the cluster
    /// but no ZFS pool answers, every size comes back absent — and the report
    /// used to print "0 B would be freed", which reads as a measurement.
    #[test]
    fn an_unmeasured_snapshot_is_not_reported_as_costing_nothing() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, false);
        set_branch(d.path(), "main", &h);

        let orphan = hash_of("7c");
        let mut map = HashMap::new();
        map.insert(
            orphan,
            SnapshotFacts {
                ready: true,
                created: Some(SystemTime::UNIX_EPOCH),
                bytes: None,
            },
        );
        let r = check_with(d.path(), Duration::ZERO, &SnapshotSource::Known(&map)).unwrap();
        assert_eq!(r.unreachable.len(), 1);
        assert_eq!(r.exclusive_bytes, Some(0));
        assert!(
            r.exclusive_is_partial,
            "the zero has to be marked as incomplete, or it reads as a measurement: {r:?}"
        );
    }

    /// Dropped rather than reported: a stray file in the object store hit a
    /// `continue` and was never mentioned, while the model's own docs cite
    /// `git count-objects -v` reporting exactly this as `garbage`.
    #[test]
    fn a_stray_file_in_the_object_store_is_reported_not_swallowed() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);
        assert!(check(d.path(), Duration::ZERO).unwrap().is_clean());

        let objects = d.path().join(GFS_DIR).join(OBJECTS_DIR);
        fs::write(objects.join("README.txt"), "how did this get here").unwrap();
        fs::create_dir_all(objects.join("not-a-shard")).unwrap();

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert_eq!(r.exit_code(), 2, "{r:?}");
        let named: Vec<&str> = r.unrecognised.iter().map(|u| u.hash.as_str()).collect();
        assert!(
            named.iter().any(|n| n.contains("README.txt")),
            "must name the file: {named:?}"
        );
        assert!(
            named.iter().any(|n| n.contains("not-a-shard")),
            "and the directory: {named:?}"
        );
    }

    /// ...but a Finder artefact is not corruption. Reporting it would make a
    /// routine check refuse to write a plan on a repository where nothing is
    /// wrong, on any machine someone has opened the folder on.
    #[test]
    fn os_droppings_are_not_mistaken_for_corruption() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);
        let objects = d.path().join(GFS_DIR).join(OBJECTS_DIR);
        fs::write(objects.join(".DS_Store"), "\x00\x01").unwrap();
        fs::write(objects.join(&h[..2]).join(".DS_Store"), "\x00\x01").unwrap();

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert!(r.is_clean(), "{r:?}");
    }

    /// A ref is a file. A symlink in its place sends the walk outside the
    /// repository to decide what is live inside it.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_ref_is_refused_rather_than_followed() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);

        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("elsewhere");
        fs::write(&target, &h).unwrap();
        let link = d
            .path()
            .join(GFS_DIR)
            .join(REFS_DIR)
            .join(HEADS_DIR)
            .join("sneaky");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert!(
            r.dangling.iter().any(|x| x.from_commit.contains("sneaky")),
            "the link is the finding: {:?}",
            r.dangling
        );
        assert_eq!(r.exit_code(), 2, "{r:?}");
    }

    /// `Path::exists` is false for a permission error as well as for absence,
    /// so an intact object inside an unopenable directory was reported as
    /// missing. Repositories on the k8s hosts are root-owned, so this told any
    /// non-root operator that a healthy repository was corrupt.
    #[cfg(unix)]
    #[test]
    fn an_object_we_cannot_open_is_not_reported_as_missing() {
        use std::os::unix::fs::PermissionsExt;

        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);
        let snap = hash_of(&format!("5{}", "aa"));
        let shard = d.path().join(GFS_DIR).join(SNAPSHOTS_DIR).join(&snap[..2]);
        fs::set_permissions(&shard, fs::Permissions::from_mode(0o000)).unwrap();

        // A mode of 000 does not stop root, so ask rather than assume: if the
        // directory is still readable there is no unreadable case to test.
        if fs::read_dir(&shard).is_ok() {
            fs::set_permissions(&shard, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let r = check(d.path(), Duration::ZERO).unwrap();
        // Restore before asserting, or the temp dir cannot be cleaned up.
        fs::set_permissions(&shard, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            r.dangling.is_empty(),
            "intact data behind a closed door is not missing data: {:?}",
            r.dangling
        );
        assert!(
            !r.unreadable.is_empty(),
            "but it is worth saying out loud: {r:?}"
        );
        assert_eq!(
            r.exit_code(),
            3,
            "and the check did not complete, so it claims neither clean nor corrupt: {r:?}"
        );
        assert!(
            r.unreachable.is_empty(),
            "a walk with a hole in it must not propose deletions: {:?}",
            r.unreachable
        );
    }

    /// Write a commit that references `files_ref`, so the object can be
    /// tampered with afterwards.
    fn write_commit_with_files(repo: &Path, seed: &str, files_ref: &str) -> String {
        let hash = hash_of(seed);
        let commit = serde_json::json!({
            "hash": hash,
            "message": "has a file list",
            "timestamp": "2026-01-01T00:00:00Z",
            "parents": Vec::<String>::new(),
            "snapshot_hash": hash_of(&format!("5{seed}")),
            "files_ref": files_ref,
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
        let snap = hash_of(&format!("5{seed}"));
        let dir = repo
            .join(GFS_DIR)
            .join(SNAPSHOTS_DIR)
            .join(&snap[..2])
            .join(&snap[2..]);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("data.txt"), "x").unwrap();
        hash
    }

    /// The store scan only ever looked at objects nothing referenced, so the one
    /// object that must not be lost was the one nobody parsed. Replacing a live
    /// file list with arbitrary bytes used to report "✓ consistent", exit 0.
    #[test]
    fn a_live_object_that_is_not_what_it_claims_to_be_is_reported() {
        let d = repo();
        let files = hash_of("f1");
        let objects = d.path().join(GFS_DIR).join(OBJECTS_DIR);
        fs::create_dir_all(objects.join(&files[..2])).unwrap();
        fs::write(
            objects.join(&files[..2]).join(&files[2..]),
            b"not a file list",
        )
        .unwrap();

        let h = write_commit_with_files(d.path(), "aa", &files);
        set_branch(d.path(), "main", &h);

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert_eq!(r.exit_code(), 2, "{r:?}");
        assert!(
            r.unrecognised
                .iter()
                .any(|u| u.hash == files && u.reason.contains("referenced as file list")),
            "must say what it was supposed to be: {:?}",
            r.unrecognised
        );
    }

    /// A snapshot whose directory is empty *while its own file list records
    /// files* has lost its contents. The file list is what makes this
    /// distinguishable: a commit of an empty data directory also has an empty
    /// snapshot, and flagging that made a freshly initialised repository report
    /// corruption.
    #[test]
    fn an_emptied_snapshot_is_caught_but_a_legitimately_empty_one_is_not() {
        use crate::model::commit::FileEntry;

        let d = repo();
        let files = repo_layout::write_files_object(
            d.path(),
            &[FileEntry {
                relative_path: "data.txt".into(),
                file_size: 1,
                owner: None,
                group: None,
                permissions: None,
                file_attributes: None,
            }],
        )
        .unwrap();
        let h = write_commit_with_files(d.path(), "aa", &files);
        set_branch(d.path(), "main", &h);
        assert!(check(d.path(), Duration::ZERO).unwrap().is_clean());

        let snap = hash_of(&format!("5{}", "aa"));
        let dir = d
            .path()
            .join(GFS_DIR)
            .join(SNAPSHOTS_DIR)
            .join(&snap[..2])
            .join(&snap[2..]);
        fs::remove_file(dir.join("data.txt")).unwrap();

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert_eq!(r.exit_code(), 2, "{r:?}");
        assert!(
            r.dangling
                .iter()
                .any(|x| x.missing.contains("1 file(s) recorded")),
            "{:?}",
            r.dangling
        );

        // And the sound case: no files recorded, nothing in the directory.
        let d2 = repo();
        let empty_list = repo_layout::write_files_object(d2.path(), &[]).unwrap();
        let h2 = write_commit_with_files(d2.path(), "aa", &empty_list);
        set_branch(d2.path(), "main", &h2);
        let snap2 = hash_of(&format!("5{}", "aa"));
        let dir2 = d2
            .path()
            .join(GFS_DIR)
            .join(SNAPSHOTS_DIR)
            .join(&snap2[..2])
            .join(&snap2[2..]);
        fs::remove_file(dir2.join("data.txt")).unwrap();
        let r2 = check(d2.path(), Duration::ZERO).unwrap();
        assert!(
            r2.is_clean(),
            "an empty snapshot of an empty data directory is sound: {r2:?}"
        );
    }

    /// The F1 failure again, one level in. A commit that will not parse hides
    /// every ancestor behind it, so the walk reaches none of them and the report
    /// offers up the history -- including the snapshot you would recover from.
    #[test]
    fn a_commit_that_will_not_parse_suppresses_the_collectable_list() {
        let d = repo();
        let root = write_commit(d.path(), "aa", "oldest", None, true);
        let mid = write_commit(d.path(), "bb", "middle", Some(&root), true);
        let tip = write_commit(d.path(), "cc", "newest", Some(&mid), true);
        set_branch(d.path(), "main", &tip);
        assert!(check(d.path(), Duration::ZERO).unwrap().is_clean());

        // Corrupt the middle commit: root and its snapshot are now unwalkable.
        let objects = d.path().join(GFS_DIR).join(OBJECTS_DIR);
        fs::write(objects.join(&mid[..2]).join(&mid[2..]), b"{ not json").unwrap();

        let r = check(d.path(), Duration::ZERO).unwrap();
        assert_eq!(r.exit_code(), 2, "{r:?}");
        assert!(
            r.unreachable.is_empty(),
            "the ancestors are hidden, not garbage \u{2014} listing them names the data you \
             would recover from: {:?}",
            r.unreachable
        );
        assert!(!r.reachability_complete);
    }

    /// Make `path` unopenable, run `f`, restore. Returns `None` when the mode
    /// had no effect, which is the case as root.
    #[cfg(unix)]
    fn while_unreadable<T>(path: &Path, f: impl FnOnce() -> T) -> Option<T> {
        use std::os::unix::fs::PermissionsExt;
        let original = fs::metadata(path).unwrap().permissions();
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(path).is_ok() {
            fs::set_permissions(path, original).unwrap();
            return None;
        }
        let out = f();
        fs::set_permissions(path, original).unwrap();
        Some(out)
    }

    /// An object store that cannot be listed is not an empty one. This used to
    /// report "checked 0 commits" and then "✓ consistent, exit 0" — the check
    /// having examined nothing at all.
    #[cfg(unix)]
    #[test]
    fn an_unlistable_object_store_is_not_an_empty_one() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);

        let objects = d.path().join(GFS_DIR).join(OBJECTS_DIR);
        let Some(r) = while_unreadable(&objects, || check(d.path(), Duration::ZERO).unwrap())
        else {
            return;
        };

        assert!(!r.is_clean(), "nothing was examined: {r:?}");
        assert_eq!(r.exit_code(), 3, "{r:?}");
        assert!(!r.unreadable.is_empty(), "{r:?}");
        assert!(r.unreachable.is_empty(), "{:?}", r.unreachable);
    }

    /// The file naming the live workspace. Unreadable used to become an empty
    /// path, matching nothing, so the checked-out workspace — the most valuable
    /// directory in the repository — was reported as unneeded.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_workspace_pointer_does_not_condemn_the_live_workspace() {
        use std::os::unix::fs::PermissionsExt;

        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);
        let ws = d.path().join(GFS_DIR).join(WORKSPACES_DIR);
        let live = ws.join("main").join("0").join("data");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("payload"), vec![0u8; 4096]).unwrap();
        let pointer = d.path().join(GFS_DIR).join(WORKSPACE_FILE);
        fs::write(&pointer, live.to_string_lossy().as_ref()).unwrap();

        assert!(check(d.path(), Duration::ZERO).unwrap().is_clean());

        let original = fs::metadata(&pointer).unwrap().permissions();
        fs::set_permissions(&pointer, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_to_string(&pointer).is_ok() {
            fs::set_permissions(&pointer, original).unwrap();
            return;
        }
        let r = check(d.path(), Duration::ZERO).unwrap();
        fs::set_permissions(&pointer, original).unwrap();

        assert!(
            r.reclaimable_workspaces.is_empty(),
            "an unreadable pointer says nothing about which workspace is live: {:?}",
            r.reclaimable_workspaces
        );
        assert_eq!(r.exit_code(), 3, "{r:?}");
    }

    /// Guessing `Filesystem` for a repository whose runtime cannot be read walks
    /// a directory a Kubernetes repository never creates, finds nothing, and
    /// invents corruption out of an unreadable config file.
    #[test]
    fn an_unreadable_runtime_config_verifies_nothing_rather_than_guessing() {
        let d = repo();
        fs::write(d.path().join(GFS_DIR).join("config.toml"), "= not toml =").unwrap();
        assert!(matches!(
            default_snapshot_source(d.path()),
            SnapshotSource::Unavailable
        ));

        // A repository with no runtime section is still a local one.
        fs::write(
            d.path().join(GFS_DIR).join("config.toml"),
            "version = \"1\"\n",
        )
        .unwrap();
        assert!(matches!(
            default_snapshot_source(d.path()),
            SnapshotSource::Filesystem
        ));
    }

    /// Branch tips are the roots of every reachability walk, so "no branches"
    /// and "could not read the branches" differ by the entire repository.
    /// `Path::exists` is false when a *parent* cannot be opened, so an
    /// unreadable `refs/` made the walk start from nowhere and reach nothing —
    /// with every object then looking collectable, at exit 1, where `--plan`
    /// would still write.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_refs_parent_is_an_error_not_an_empty_branch_list() {
        let d = repo();
        let h = write_commit(d.path(), "aa", "live", None, true);
        set_branch(d.path(), "main", &h);
        assert!(check(d.path(), Duration::ZERO).unwrap().is_clean());

        let refs = d.path().join(GFS_DIR).join(REFS_DIR);
        let Some(result) = while_unreadable(&refs, || check(d.path(), Duration::ZERO)) else {
            return;
        };
        match result {
            Err(_) => {}
            Ok(r) => panic!(
                "an unreadable refs directory must not read as a repository with no \
                 branches: {r:?}"
            ),
        }
    }
}
