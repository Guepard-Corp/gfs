# Can a WAL watermark replace a storage snapshot in `commit`?

**Question.** Can `LocalEngine::prepare_for_snapshot` return a guard whose meaning
is "watermark recorded at frame N" rather than "writers excluded while files
copy", without changing what `commit` promises callers?

**Answer: no, not as a replacement — but yes as the consistency mechanism, and
that is the more valuable version.** The watermark cannot be the artifact a
commit records. It can be how the artifact is taken consistently, and doing it
that way removes the database pause that `commit` requires today.

Read from source on 2026-10-05: `crates/domain/src/ports/storage.rs`,
`ports/database_provider.rs`, `ports/compute.rs`, `model/commit.rs`,
`usecases/repository/commit_repo_usecase.rs`, `checkout_repo_usecase.rs` in the
gfs tree, and `core/connection.rs`, `core/cdc.rs`, `sync/engine/src/*` in
`tursodatabase/turso`.

## What `commit` promises today

Five things, each load-bearing somewhere downstream:

1. **A crash-consistent image.** `commit_repo_usecase` pauses the container
   before snapshotting and *refuses by default* otherwise, because "a file-level
   snapshot of an unfrozen database is not crash-consistent — pages and WAL can
   be captured mid-write". Proceeding is an explicit opt-in.
2. **A materialised, addressable artifact.** `Commit.snapshot_hash` is
   `hash_snapshot(volume_id, timestamp)` and names a directory via
   `ensure_snapshot_path`. Checkout copies files *out of* it into the workspace.
3. **Engine independence.** `StoragePort::snapshot(&VolumeId, SnapshotOptions)`
   knows nothing about databases. The same call serves postgres, mysql and
   clickhouse.
4. **Mountable and cloneable.** `StoragePort::clone(.., from_snapshot)` plus
   `mount()`; the Kubernetes adapter maps these onto PVCs and VolumeSnapshots.
5. **Survival independent of the engine.** A snapshot is a separate copy. It
   outlives the database file, the engine version, and the process.

## What a watermark satisfies, and what it does not

A watermark satisfies (1), and arguably better than a freeze does: it is a
logical read-point rather than a hope that the files were quiet.

It fails (2) through (5). It is an integer offset into one SQLite file's WAL. It
is not a path, cannot be mounted, cannot be handed to `StoragePort::clone`, and
`Snapshot::size_bytes` has no meaning for it. Checkout, which copies files out of
a snapshot directory, would have nothing to copy.

**The decisive failure is (5) combined with WAL checkpointing.** Checkpointing
reclaims frames. A watermark into a checkpointed WAL is a dangling pointer, so
every historical commit would require pinning the WAL from its frame forward —
the WAL then grows without bound, and any checkpoint past a watermark silently
destroys that commit's recoverability. That converts a durability *guarantee*
into an operational *discipline*, which is the wrong direction for the one thing
this product sells.

## The variant that works

Keep the artifact, change the consistency mechanism:

- `prepare_for_snapshot` returns a guard meaning **"a consistent read-point is
  pinned at frame N"**, not "writers are excluded".
- While that guard is held, the storage layer materialises pages *as of that
  watermark* — `turso_core::Connection::try_wal_watermark_read_page`, with
  `sync/engine/src/sparse_io.rs` as the precedent for faulting pages — into the
  same snapshot directory `ensure_snapshot_path` already produces.
- `snapshot_hash`, checkout, clone, mount, `Snapshot` and the Kubernetes
  adapter are all untouched. Promises (2) through (5) hold unchanged.

**What changes is that writers are never excluded.** Commit stops needing to
pause the database. Today it pauses, and refuses outright when it cannot freeze
(rootless Podman on cgroup v1, LXC without freezer). A watermark-pinned read
makes a consistent commit of a *live* database possible, which is a product
improvement rather than an internal refactor.

This also dissolves a layering objection raised earlier. The engine supplies
*consistency*; `StoragePort` still supplies the *artifact*. It is a per-provider
consistency strategy under the existing contract, not a second `Repository`
implementation competing for ownership of `commit`.

## Caveats, stated plainly

- **One provider.** All of this is SQLite-file-shaped. Postgres, MySQL and
  ClickHouse keep the pause-and-snapshot path. This adds a strategy; it removes
  nothing.
- **`db_live_during_snapshot` is not this seam.** It means `pause()` does not
  freeze the database so read-only schema extraction may *overlap* the snapshot
  — a latency optimisation (max instead of sum), not a consistency mechanism.
  Easy to misread as support for live snapshots.
- **Turso is beta.** MVCC keeps row versions in memory; `wal_checkpoint(TRUNCATE)`
  blocks readers; passive checkpointing is behind an experimental flag.
- **Not a drop-in.** `db-providers/src/sqlite.rs` is ~3,250 lines on `rusqlite`.
  Turso has its own API and its own `IO` trait, so this is a new adapter that
  must re-earn that file's hard-won behaviour (symlink handling, empty-snapshot
  guards, lock timeouts).
- **`SparseLinuxIo` is Linux-only** — fine for k3s, not for macOS development.
- **CDC is the wrong mechanism for this job** and should not be load-bearing:
  it is per-connection via `PRAGMA capture_data_changes_conn`, and gfs does not
  own the connection (the writer is the user's own application, as
  `LocalEngine`'s own documentation says). It also excludes views and triggers
  (`core/translate/view.rs` passes `None`), and has no retention story. Useful
  for `gfs log`/diff enrichment, where its gaps degrade a feature rather than
  corrupt a commit.

## The spike worth running

One question, and it is narrower than "integrate Turso":

> Can the pages of a Turso database be materialised, as of a pinned WAL
> watermark, into a directory that satisfies the existing `Snapshot` contract,
> while a writer is concurrently committing?

If yes: a live-database commit path, with every downstream contract intact.
If no: a provider swap for the SQLite path, and little else.

Verification must include the negative case — write continuously during the
materialisation and prove the output matches the state at frame N, not a later
one. A test that only snapshots an idle database proves nothing here.
