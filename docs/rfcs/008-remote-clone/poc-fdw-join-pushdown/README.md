# PoC: postgres_fdw already pushes joins down — the overlay is what blocks it

**Question.** For cold analytical joins (no warmed child to seed transitive FK
warming, see ../poc-join-fk), do we need a custom pgrx FDW implementing
`GetForeignJoinPaths` to push the join to the remote?

**Answer: no.** `postgres_fdw` already pushes a whole join to the remote when
both sides are foreign tables of the same server (native since PG 9.6). The only
thing defeating it today is our overlay *view*. `run.sh` (single container,
postgres_fdw looping back to the same db) — **4/4 PASS**.

## Findings

- **A — native join pushdown.** A join between two `gfs_remote.*` foreign tables
  plans as a *single* `Foreign Scan` with
  `Relations: (gfs_remote.orders o) INNER JOIN (gfs_remote.customers c)` and
  `Remote SQL: SELECT … FROM (src.orders r1 INNER JOIN src.customers r2 ON …)`.
  The join executes remotely, in one round-trip.
- **B — correctness.** The pushed result equals the local computation.
- **C — the overlay defeats it.** The same join through the overlay views
  (`local UNION ALL foreign − anti-join`) plans as **2 Foreign Scans** joined
  locally. The overlay, not postgres_fdw, is the obstacle.

## Implication: re-scope the `gfs_clone` extension

The extension does **not** need a custom FDW. Its job becomes a set of
lightweight hooks:

1. **Transparent warming on SELECT** — a scan/planner hook that triggers
   `gfs_sync.warm_query_chunks` (incl. transitive FK warming) on a read, so the
   warming the proxy does today happens in-DB on a direct connection (the proxy
   is no longer *required* for warming).
2. **Cold-join routing** — when a join is cold (no warmed rows), route it to the
   `gfs_remote_*` foreign tables so postgres_fdw's native pushdown kicks in,
   instead of going through the overlay view that blocks it.

This is far simpler and lower-risk than `GetForeignJoinPaths` in `pg_sys`
unsafe. Correctness still never depends on the extension (graceful degradation:
absent extension ⇒ today's overlay behaviour).

## Run

```bash
bash run.sh   # ~20-40s; one container; cleans up on exit.
```
