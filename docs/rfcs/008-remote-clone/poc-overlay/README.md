# RFC 008 PoC: overlay (the validated mechanic)

Proves the overlay (copy-on-read) of [RFC 008](../../008-remote-clone.md) in
plain SQL on two throwaway Postgres containers.

> Context: an earlier prototype used foreign tables as range partitions. It was
> **incorrect**: a foreign partition mapping the whole remote table does not
> restrict to its key range (PostgreSQL trusts partitions and never pushes the
> bound; `postgres_fdw` only pushes explicit query predicates), so unqualified /
> non-key scans over-count (N× the rows). The overlay below replaces it.

## The mechanic

Per table `T` (PK `id`):
- `gfs_remote.T`: foreign table = whole remote (read-only).
- `T_local`: local heap store (hydrated + written rows), PK on `id`.
- `T_deleted`: delete tombstones (PKs hidden from the remote side).
- `T`: a **view** where local wins, remote rows only if neither local nor tombstoned.

```sql
CREATE VIEW T AS
  SELECT * FROM T_local
  UNION ALL
  SELECT r.* FROM gfs_remote.T r
   WHERE NOT EXISTS (SELECT 1 FROM T_local   l WHERE l.id = r.id)
     AND NOT EXISTS (SELECT 1 FROM T_deleted t WHERE t.id = r.id);
```

Writes go through `INSTEAD OF` triggers (copy-on-write): INSERT/UPDATE upsert into
`T_local`; DELETE removes from `T_local` and adds a tombstone.

Correctness is guaranteed by construction (disjoint row sets), independent of
hydration. **Hydration becomes a pure optimisation**: `INSERT INTO T_local
SELECT … FROM gfs_remote.T WHERE <range>` warms the cache and enables divergence.

## What it proves

`./run.sh` (or `bash run.sh`) checks, on the cases that broke the partition PoC:

- global `count(*)` = remote (30000, not 90000);
- non-key predicate count is exact (no N× duplication);
- selective reads push the predicate to the remote (verified via `EXPLAIN
  VERBOSE`: `Remote SQL: … WHERE ((id = 12345))`), so no full-table scan;
- hydration keeps counts correct;
- UPDATE copy-on-write: rows land local, remote untouched;
- DELETE tombstone: row hidden in the view, remote untouched;
- INSERT new key: visible locally, remote unaffected;
- divergence: the remote stays unchanged throughout.

Requires Docker with the Compose plugin. Two throwaway Postgres 17 containers;
uses `01-remote-seed.sql` for the remote seed.

## Trade-offs

- `NOT EXISTS` anti-join on the remote side (cheap with the PK indexes on
  `T_local`/`T_deleted`; `T_local` is small early on).
- Writes carry per-row `INSTEAD OF` trigger overhead.
- `T` is a view, not a table (introspection tools see a view).
- Unhydrated rows reflect the **live** remote until hydrated (consistent with the
  as-of-first-touch model).
