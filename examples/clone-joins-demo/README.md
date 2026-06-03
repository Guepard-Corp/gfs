# GFS clone demo — transitive FK warming makes JOINs local

A self-contained end-to-end test of **transitive foreign-key warming** (RFC 008,
"level 0"): the feature that lets a lazy copy-on-read clone serve **joins**
entirely locally, with no app change and no custom extension.

> run `gfs clone` on a star-schema source → a cold join federates both sides →
> warm the queried child range → the same join becomes 100% local (zero
> `Foreign Scan`), because warming followed the foreign keys to the parents.

## Why joins are the hard case

The overlay clone serves a table as a view: `local UNION ALL foreign − anti-join`.
That view is correct, but it **defeats `postgres_fdw`'s join pushdown** — so a
cold join federates *each* side (a `Foreign Scan` per table) and joins them
locally. Warming the child table alone doesn't help the parent: a join gives the
planner no direct predicate on the parent key, so the range-exclusion CHECK can
never prune the parent's foreign scan.

**The fix:** when `gfs_sync.warm_query_chunks` warms a table, it reads that
table's outgoing foreign keys (`pg_constraint`, already present because
`gfs clone` replays the source DDL) and warms exactly the referenced parent
rows — in topological order, with FK triggers suspended — then promotes small
parent (dimension) tables to `whole_table` so their foreign branch is dropped
from the overlay view. The join is then a local index join: zero remote contact.

## What the script proves

`run.sh` seeds `customers <- orders -> products` (FKs), clones it, and asserts:

| Step | Assertion |
|------|-----------|
| 1 | the **cold** join federates (`Foreign Scan` present) |
| 2 | warming the child range hydrates **both** parent tables transitively |
| 3 | the **warm** join has **zero** `Foreign Scan` (fully local) |
| 4 | the clone's join result is identical to the source |
| 5 | the source stayed read-only (copy-on-read) |

## Prerequisites

- Docker Desktop (the clone reaches the source via `host.docker.internal`)
- A built `gfs` binary: `cargo build -p gfs-cli` from the repo root
  (defaults to `target/debug/gfs`; override with `GFS_BIN`).

## Run

```bash
cd examples/clone-joins-demo
bash run.sh
```

Exit code is 0 on all-pass. The script starts and removes its own containers
(`gfs-joins-source`, `gfs-joins-clone`) and the `joins-clone/` repo on exit.

Override defaults with env vars: `SOURCE_PORT`, `CLONE_PORT`, `GFS_BIN`,
`REMOTE_HOST`.

## Notes

- This is the **level-0** feature: pure in-DB SQL, working on a standard
  `postgres:16` clone — **no `gfs_clone` extension required**. Correctness never
  depends on warming; warming only removes remote round-trips.
- For purely *cold* analytical joins (no warmed child to seed the FK walk),
  `postgres_fdw` already pushes the join to the remote when routed at the foreign
  tables directly — see `docs/rfcs/008-remote-clone/poc-fdw-join-pushdown`.
- To exercise the transparent-warming **extension** hook (so a plain `SELECT`
  triggers warming with no proxy), see `crates/extensions/gfs_clone`
  (`cargo pgrx run pg16`).

## Cleanup

The script cleans up on exit. If interrupted hard, remove leftovers manually:

```bash
docker rm -f gfs-joins-source gfs-joins-clone
rm -rf joins-clone
```
