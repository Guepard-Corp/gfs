# Orchestrating a live commit alongside the freeze

**Question.** Can engine-assisted live backup sit beside the container freeze as
primary/fallback, and how is it orchestrated?

**Answer: yes, and it needs no change to the `Compute` port.** It slots into a
ladder that already exists, and the correct place for it is narrower than it
first appears — it should *replace* the unsound best-effort path, not the
freeze.

## What is already there

`take_snapshot` in `commit_repo_usecase.rs` already runs a three-rung ladder
when it tries to freeze:

| Condition | Behaviour today |
| --------- | --------------- |
| `pause()` succeeds | freeze, snapshot, unpause (Docker) |
| `PauseUnsupported` **and** `db_live_during_snapshot` | proceed **live** — the runtime's storage takes an atomic point-in-time snapshot (Kubernetes + ZFS CSI `VolumeSnapshot`), so `CHECKPOINT` + atomic snapshot is crash-consistent without freezing |
| `PauseUnsupported` and not atomic | **refuse**, unless `GFS_ALLOW_UNFROZEN_SNAPSHOT=1`, which proceeds "best-effort" and may need manual WAL replay on restore |

So Kubernetes already commits a live database, and `db_live_during_snapshot`
is both the schema-overlap flag and the live-commit marker.

The gap is the third rung. On a non-atomic store that cannot freeze — rootless
Podman on cgroup v1, LXC without freezer — the choice today is refuse or do
something unsound. The spike in `postgres-live-commit-spike.md` showed that
"unsound" is also *undetectable*: a naive live copy started, answered queries
and passed `bt_index_parent_check`.

## Where the new strategy belongs

Four rungs, tried in order:

1. **Atomic storage, live.** `db_live_during_snapshot` + an atomic adapter
   (`storage-kubernetes`, `storage-btrfs`). Unchanged. Cheapest and already
   shipped.
2. **Engine-assisted live backup.** NEW. `pg_backup_start`/`pg_backup_stop`
   around a non-atomic copy. Correct without freezing.
3. **Freeze.** Unchanged, and still the default for Docker until rung 2 is
   proven in CI.
4. **Unfrozen best-effort.** Reachable only when the provider offers no rung 2.

Rung 2 earns its place by **eliminating rung 4** for Postgres, not by displacing
the freeze. Making it preferred over rung 3 is a separate, later decision: it
buys zero-pause commits, which is an availability win, not a correctness one.

## Orchestration without a port change

The recipe needs `pg_backup_start` and `pg_backup_stop` **in one session** that
stays open across the snapshot. `Compute::exec` is one-shot
(`-> Result<ExecOutput>`), so it cannot hold a session — but it does not need
to. Hold the session *inside* a single exec and let it wait on a sentinel:

```text
exec arm                                   storage arm
--------                                   -----------
psql <<SQL
  SELECT pg_backup_start('gfs', true);
  \! wait for  <workspace>/.gfs/tmp/snap-done
                                           StoragePort::snapshot(...)
                                           touch <workspace>/.gfs/tmp/snap-done
  SELECT labelfile FROM pg_backup_stop();
SQL
-> ExecOutput carries the label
```

Both arms run under `tokio::join!`, which `commit_repo_usecase` already uses for
the schema and snapshot arms. The sentinel lives in the workspace, which is
already shared between gfs and the container on the Docker path.

Failure handling falls out of Postgres' own semantics: a non-exclusive backup
**aborts when its session dies**. So if the snapshot fails, gfs kills the exec
and the backup unwinds itself — no stuck backup state, no cleanup path to get
wrong. That is a genuine argument for the session-based shape over a
begin/end pair of independent calls.

## The snapshot must stay immutable

`StoragePort` makes a snapshot read-only, and the recipe produces two artefacts
*after* the copy: the `backup_label` returned by `pg_backup_stop`, and the WAL
segments written during the window. They cannot be written into the snapshot.

Put them in gfs's own object store, keyed by the commit:

```text
.gfs/objects/<snapshot_hash>.recovery/
    backup_label
    pg_wal/000000010000000000000002
    ...
```

Checkout already copies a snapshot into the workspace and already repairs
ownership afterwards (the `.needs-repair` marker). Applying a recovery sidecar
is one more step on that same path, and it keeps the snapshot immutable, which
every other consumer — clone, mount, the Kubernetes adapter — depends on.

## What the provider must supply

Rung 2 is per-engine, so it belongs behind the provider, like
`prepare_for_snapshot`. The smallest shape that works:

```rust
/// How to take a consistent copy of this engine's files while it keeps serving.
/// `None` — no engine-assisted live backup; the caller falls back to freezing.
fn live_backup(&self) -> Option<LiveBackupSpec>;

struct LiveBackupSpec {
    /// Runs in one session; waits on `sentinel_path` between begin and end.
    script: String,
    /// Paths to collect into the recovery sidecar after the window closes.
    recovery_paths: Vec<String>,
    /// Settings that must already hold, verified before the window opens.
    requires: Vec<(String, String)>,   // e.g. wal_keep_size >= '1GB'
}
```

`requires` is what makes this safe rather than hopeful. The spike failed twice
before it worked, and the second failure was WAL recycling — the segments the
backup needed were overwritten before they could be collected. Verifying
`wal_keep_size` (or a slot, or archiving) **before** opening the window turns
that from a silent restore-time failure into a refusal at commit time, which
falls back to the freeze.

Per engine:

| Provider | Rung 2 primitive | `requires` |
| -------- | ---------------- | ---------- |
| postgres | `pg_backup_start` / `pg_backup_stop` | WAL retention across the window |
| mysql | `LOCK INSTANCE FOR BACKUP` (8.0) | InnoDB redo recovery on restore |
| clickhouse | `ALTER TABLE … FREEZE` (parts are immutable) | none expected |
| sqlite | `LocalEngine` already holds the write lock | n/a |

Only postgres is measured. The other three rows are from knowledge, not
experiment, and each needs its own spike before anyone relies on it.

## Order of work

1. Set WAL retention at provision time, in the Postgres definition — not per
   commit, which would be a persistent config change made on a hot path.
2. Add `live_backup()` returning `None` everywhere. No behaviour change.
3. Implement it for postgres; put rung 2 between the atomic and freeze rungs,
   gated off by default.
4. Prove it in CI with the negative control from the spike: a continuously
   written database, and an assertion that the restore reaches
   `consistent recovery state reached` — plus the naive-copy case, to show the
   test can tell the two apart. A test that only snapshots an idle database
   proves nothing here.
5. Make rung 2 preferred over the freeze only after (4) is green on repeat runs.
6. Then remove `GFS_ALLOW_UNFROZEN_SNAPSHOT` for postgres, since rung 2 covers
   the case it exists for.

## Assumption recorded

This design keeps the freeze as the default and treats live backup as the
fallback that makes the unsound rung unnecessary. The opposite framing — live
backup primary, freeze as fallback — is the same machinery with the rung order
swapped, and becomes the better default once step 4 has run enough times to
trust. Nothing here forecloses it.
