# RFC 009: Integrity checking and garbage collection

GFS never reclaims anything. A deleted branch, an interrupted commit and a
detached-HEAD commit all leave their snapshot trees and commit objects on disk
permanently, and nothing in the product can find them again or free them.

This RFC specifies `gfs fsck` (read-only, ships first) and `gfs gc`
(reclaims, ships behind it), and records why the safety design is what it is.

Background research, with primary sources and eleven post-mortems from git,
Dolt, lakeFS, Nessie, Iceberg and Delta Lake, is held outside this repository
and is not required to follow the decisions below; every claim it supports is
restated here with its source.

## The problem, measured

Three branches created, committed to, and deleted:

```
                   before delete   after delete
snapshot trees           4              4
commit objects           8              8
size                 16.1 MB        16.1 MB
reachable commits        —              1
```

Nothing is freed. Three of four snapshot trees are unreachable from any ref and
no tool can name them, let alone remove them. The payload here was 4 MB; a real
database makes it gigabytes.

Four sources of permanent garbage exist today:

- deleted branches — the ref goes, the snapshot and objects stay;
- commits interrupted between taking the snapshot and writing the object;
- commits made at a detached HEAD, unreachable the moment they are created;
- amended-away commits, once `commit --amend` exists.

Snapshots are named `hash_snapshot(source_path, timestamp)` rather than by
content, so two commits of byte-identical data produce two independent trees.
GFS never dedups, which makes its garbage problem larger than that of the
systems it is compared to below, not smaller.

There is no tool in this repository that reclaims any of it. A standalone
reclaim script exists on an unmerged branch and is deliberately *not*
reachability-based — its own docstring says *"Every commit is scanned, not just
the ones reachable from a ref, so a snapshot belonging to a commit on a deleted
branch is NOT reclaimed."* It covers only the crash window between taking a
snapshot and writing its commit object.

## The hazard

`commit` creates `.gfs/snapshots/<2>/<62>/` and only afterwards writes the
commit object that references it. In that window the snapshot is live data that
nothing points at, and a naive collector deletes it.

### Why a lock is not sufficient

The rule, drawn from comparing every system in the research:

> A lock substitutes for a grace period **exactly when** you can enumerate every
> writer and force each one to a quiescent point. Otherwise you need a grace
> period — and the grace period is not a weaker substitute for the lock, it
> defends a different failure.

Dolt demonstrates both halves in one codebase. Its in-process GC uses a
cooperative safepoint and no grace period, because every writer is a session
inside one server process. Its `file://` backup pruner uses a manifest lock
**and** a ten-minute quiescence window, because there the writers are processes
it cannot see. From `go/store/nbs/prune_grace.go`:

> *"We cannot ask 'is this file unreferenced?' — a writer which renames a table
> file into place and has not yet committed its manifest is still referencing
> the file, but we have no visibility into it. Instead we ask 'has anything in
> this directory been touched recently?'"*

**GFS is the second case.** Its writers are separate short-lived CLI processes,
possibly of different versions, possibly killed by a container runtime
mid-commit. They cannot be enumerated. So the grace period is mandatory, and the
lock is taken to *bound the damage* of a wrong quiescence judgement, never as
the safety argument.

The cautionary tale is GitLab Gitaly MR 4410: they called `git prune` directly,
assuming the two-week default that only `git gc` applies, and *"accidentally
started to delete all unreachable objects, even if they had just been created."*
That is precisely this window.

## Decisions

### D1 — `gfs fsck` ships before `gfs gc`, and `gc` refuses to run when `fsck` fails

`fsck` only reads, so it cannot lose data, and it needs no change to the
`Storage` port (see D6). It also *is* the mark phase, so it is not throwaway
work. Every comparable system has one — `git fsck --unreachable`, `dolt fsck`,
lakeFS's `NaiveCommittedAddressLister` fail-safe — and running a collector
against an already-inconsistent repository is how a recoverable incident becomes
an unopenable database (Dolt #11070).

It reports both directions:

- **unreachable** — objects and snapshot trees no walk from a root touched;
- **dangling** — a commit referencing a snapshot that is missing. GFS can
  already produce this, and a checkout that finds its snapshot gone must refuse
  rather than hand back an empty database.

### D2 — roots

Every branch ref, HEAD (attached or detached), and every soft-deleted ref still
inside its retention window. Any snapshot carrying a live pin (D3) or an mtime
newer than the grace cutoff (D4) is also a root.

Following Dolt, whose roots are its dataset map, and explicitly **not** treating
any log or journal as a root.

### D3 — an explicit pin, written before the snapshot

Copied from git's `.keep`, which exists for exactly this window: *"to prevent a
simultaneous `git repack` process from deleting the newly constructed pack and
index before refs can be updated."*

`.gfs/snapshots/<2>/<62>.keep` is written **before** the snapshot directory and
removed after the ref update. It records pid, hostname, start time and the
operation, so an orphaned pin can be attributed. GC treats a live pin as a root
unconditionally.

A pin is reaped only when its pid is dead **and** its mtime is older than the
grace period — never on pid-death alone, because a crashed commit's snapshot is
exactly the data a human may want back.

### D4 — grace period: derived, not chosen. Default 24 hours.

The rule is Delta Lake's: larger than the longest possible duration of a job.
The longest legitimate GFS operation is a full snapshot of a multi-GB data
directory on a slow volume, plus a paused-instance timeout, plus schema
extraction — hours, not minutes. lakeFS reached the same 24h after 6h proved too
short for large uploads (#10099, #10180).

**Do not copy git's two weeks.** Git is protecting loose objects that cost 4 KB;
GFS would be protecting whole database snapshots that cost gigabytes and never
dedup.

A floor is enforced in argument parsing, as Dolt does with
`BackupPruneMinGracePeriod`. `--expire=now` is reachable only behind a flag
whose name says what it disables, following Delta's
`retentionDurationCheck.enabled`.

Two refinements are required because GFS runs on NFS-backed volumes under
Kubernetes, both from Dolt's `pruneDirWithGrace`: take the cutoff from **a probe
file the filesystem itself just stamped** rather than the local clock, and let
**one recent mtime veto the whole pass** rather than deciding tree by tree.

An unreferenced snapshot newer than the cutoff is **promoted to a root**, per
git's `reachable.c` — not merely skipped. And per `builtin/prune.c`, re-`stat`
immediately before deleting and skip if the mtime moved.

### D5 — two phases, with the plan persisted before the first unlink

`.gfs/gc/<mark-id>/plan.json` is written **before** anything is deleted, and
`summary.json` after. That single artefact provides dry-run, audit,
crash-diagnosis and resumability.

This fixes an ordering bug rather than copying one: lakeFS writes its reports in
a `finally` *after* the sweep, which is why a killed run cannot be resumed
(`FailedRunException("Provided mark ($markID) is of a failed run")`).

Anything created after the mark began is out of scope by construction — Nessie's
`--max-file-modification` defaults to the mark epoch, and GFS does the same.

### D6 — delete by renaming into trash, purge on a later run

A doomed snapshot is `rename(2)`d into `.gfs/trash/<mark-id>/` — one atomic,
instantly reversible metadata operation — and purged by a subsequent run. This
is GitHub's "limbo repositories", shipped as `git repack --expire-to=<dir>`.

It also solves a GFS-specific problem: snapshot trees are made read-only
(`chmod -R u+rX,u-w,go-rwx`), so a recursive delete must restore write
permission first and can fail partway, leaving a half-stripped tree. A rename
cannot.

**This requires extending the `Storage` port, which today has `snapshot`,
`clone`, `mount`, `unmount`, `status`, `quota` and `finalize_snapshot` — and no
delete operation at all.** A collector cannot be written without adding one, and
it cannot bypass the port: under Kubernetes a snapshot is a VolumeSnapshot
object removed through an API call, not a directory to unlink. This is the main
reason `fsck` lands first.

Reporting follows Dolt's `PruneStats`: files deleted, bytes reclaimed, and a
list of human-readable skip reasons — *a skip is a normal outcome, not an
error*.

### D7 — reject cooperative safepoints, generational GC, incremental state, and copying collection

- **Cooperative safepoints** (Dolt's `VisitGCRoots`) presuppose enumerable
  writers, which GFS does not have. Their characteristic failure is that the
  participant list rots: in Dolt #10602 the binlog producer held live references,
  was never registered as a roots provider, and GC deleted chunks under it. GFS
  already has more such components — running database containers holding open
  descriptors into a workspace, storage adapters, sidecars.
- **Generational GC** buys nothing when the object graph is a few thousand
  directory entries; a full walk is a `readdir`.
- **Incremental reachability state** was removed from lakeFS in PR #6634 after
  three correctness bugs. A mature project could not keep it correct.
- **Copying collection** needs 100% headroom without dedup. Dolt #10463 shows
  the failure: an interrupted copying GC left 38 GB of temporary files against a
  30 GB database. Rename-into-trash gives the same atomicity for free.

### D8 — `--auto`, and make "did it run?" observable

`gc.auto`-style amortisation so nobody has to remember, plus a recorded
last-successful-GC. Dolt #10944 is the reason: an inverted load-average formula
meant auto-GC silently never fired for five releases, and a GC that never runs
is indistinguishable from a GC with nothing to do. Dolt #10463 is the other
reason: an operator could not answer "is a GC running right now?".

### D9 — working copies are a fifth source of garbage, and usually the largest

**Added after the decisions above; nothing in D1–D8 changes.** The list of four
sources in "The problem, measured" is incomplete. It counts only things in the
object graph, and the largest waste in a repository is not in the graph at all.

`.gfs/workspaces/` holds one full copy of the data directory per branch, plus
one per detached checkout. **Nothing in the product has ever removed one.**
Measured on a small repository after three detached checkouts:

```
.gfs total            1.9 MB
of that, workspaces   1.1 MB      58%
fsck reclaimable      0 bytes
fsck verdict          consistent
```

On a real database each of those is gigabytes rather than kilobytes.

**They are not objects, and must not be reported as unreachable ones.** They are
reported separately, with their own byte total, so neither number misleads.

**Correction (2026-09-04): an earlier draft of this section called a workspace a
cache and said removing one costs time, not data. That is false on today's
`main`.** `GfsRepository::checkout` populates from a snapshot *only when the
workspace does not exist* — the comment there says "preserve live DB state in
branch workspace". So a workspace is authoritative, and deleting one destroys
state checkout would have kept. The cache framing borrowed behaviour from PR #76
("Always restore from the snapshot"), which is not merged.

Consequences: this list is **report-only** until checkout always restores, and a
collector must not act on it until GFS can tell a pristine workspace from a
written-on one. That ordering matches every comparable system surveyed — git, jj,
Perforce and EdenFS all decline to delete a working copy's files, and three of
the four refuse or warn on uncommitted work.

Liveness, in order:

1. the directory named by `.gfs/WORKSPACE` is live, whatever else is true;
2. `workspaces/<branch>/` is live while a ref of that name exists — including
   when it only *contains* live nested branches, since `team/` is a directory
   holding `team/alpha`;
3. `workspaces/detached/<prefix>/` is live while a reachable commit begins with
   that prefix. The path carries a 12-character prefix, not a full hash, so this
   is a prefix match against the reachable set rather than a lookup.

The grace period of D4 applies here too: a workspace being written right now is
not garbage.

Two consequences worth stating. A workspace can be stale while its branch's
*commits* remain perfectly reachable — deleting a branch strands the working
copy but not the history — so the two findings are independent and a repository
can have either without the other. And on the Kubernetes runtime, where
snapshots are `VolumeSnapshot` objects rather than directories, workspaces are
still ordinary directories, so this check runs there even when the snapshot
check is suppressed.

Prior art is thin: none of git, Dolt, lakeFS or Nessie has this problem, because
none of them keeps a full materialised copy of the data per ref. That is a
consequence of GFS snapshotting a live database directory rather than storing
content-addressed chunks, and it means this decision is ours to get right rather
than to borrow.

## Sequencing

1. `gfs fsck` — read-only, no port change.
2. `Storage::delete` — across the file, APFS, btrfs and Kubernetes adapters.
3. `gfs gc` — mark, plan, rename into trash, purge.

The shared repository lock (`RepoLock`, extending `.gfs/commit.lock` beyond
`commit`) is a prerequisite for step 3 and is tracked separately.

## Out of scope

Content-addressing snapshots. It would give GFS dedup and make identical data
free, and it is the strategic fix for the underlying cost — but it is a change to
the snapshot identity model, not to collection, and it does not remove the need
for any decision above.

## Amendments

Appended rather than edited in place: the original text stays as written, and
these record where `main` has moved since. All measured on the implementation
branch, 2026-10-06.

### A1 — the measured problem statement no longer reproduces

"The problem, measured" rests on three branches created, committed to and
deleted leaving their objects unreachable. On current `main` that construction
leaves **nothing** collectable, for two reasons that both arrived after this RFC
was written:

- `branch -d` is recoverable. It moves the ref into `refs/deleted/` rather than
  unlinking it, and fsck roots every tombstone, so the commits stay live. They
  become collectable only once expiry removes the entry — and expiry runs solely
  from inside `branch -d`, so it is lazy (tracked separately).
- `branch -d` also removes the branch's working copy, so no reclaimable
  workspace is left either.

The hazard the RFC describes is real; the reproduction in it is stale. A fresh
measurement is needed before the cost figures are quoted again.

### A2 — two further claims that `main` has falsified

"Commits made at a detached HEAD, unreachable the moment they are created":
`main` refuses to commit on a detached HEAD and leaves no orphan snapshot.

"A standalone reclaim script exists on an unmerged branch":
`scripts/gfs-reclaim-orphan-snapshots.py` is in `main`.

### A3 — D4's floor and gate are implemented, with a named flag

D4 asked for a floor enforced in argument parsing and for `--expire=now` to be
reachable "only behind a flag whose name says what it disables". Implemented as
`MIN_GRACE` of one hour and `--disable-grace-period-check`, rejected before the
repository is read. Measured before the floor existed: at `--grace 0`, 19 of 19
runs during a concurrent commit persisted a plan naming live in-flight data, and
32 of 32 in an independent audit; at the default window, 0 of 28.

D4's two NFS refinements — taking the cutoff from a probe file the filesystem
stamped, and letting one recent mtime veto the whole pass — are **not**
implemented.

### A4 — D5's mark epoch is now what the plan records

The persisted `cutoff` was stamped at write time, dating the plan after the walk;
a collector honouring it would have failed to exclude anything created during the
mark. It now carries the epoch the walk used, to millisecond precision. Measured
after: the recorded epoch sits ~9ms after launch and stays flat as the walk
lengthens, where before it tracked walk duration (52ms and 90ms).

### A5 — the lock D5 and D6 depend on does not exist on the writers

`RepoLock` is listed as a prerequisite for step 3. Note that `branch -d` and
`branch --restore` take **no** lock — they are bare `fs::rename` — so `gc` cannot
simply acquire it: those two commands have to start taking it first, which is a
user-visible change since they currently never block.

Consequently fsck narrows its own read race (reading the branch store twice) but
cannot close the gap between mark and sweep, which is unbounded. `gc` must
re-validate the marked set under the lock immediately before each unlink — D4
already requires exactly that for mtime, and reachability needs the same.

### A6 — D9's rule 1 was never load-bearing

"`.gfs/WORKSPACE` is live, whatever else is true" was implemented as a raw string
prefix test that matched nothing for the relative path `gfs init .` records, and
mismatched a canonical recorded value when `--path` reached the repository
through a symlink. Both are fixed. But reverting the fix changes no observable
behaviour: a branch workspace is already protected by its live branch ref and a
detached one by the reachable-commit prefix, so rule 1 is belt-and-braces behind
rules 2 and 3 rather than the guarantee the RFC presents it as.
