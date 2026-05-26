---
name: RFC 008 - Remote clone
title: Lazy clone of a remote database (copy-on-read overlay)
status: Accepted
---

## Summary

Clone a remote PostgreSQL database into GFS **instantly**, without dumping and
importing the data. Only the schema is mirrored up front (small, even for
multi-TB databases). Reads are served live from the remote until rows are copied
locally; **writes always go to GFS**, so the clone **diverges** from the remote.
Git's `clone` semantics applied to a database.

Mechanism: an **overlay** built entirely inside the local GFS PostgreSQL. Each
cloned table becomes an updatable **view** that unions a local store with the
remote (via `postgres_fdw`), where **local always wins**. Correctness is
guaranteed by construction; no proxy and no changes on the remote.

This replaces "dump + import" for the common case of working against a large
remote database where only a subset of the data is read, or where you want to
experiment with writes against production-shaped data.

## Motivation

`gfs import` transfers the entire dataset before any work can start. For
multi-TB databases this is prohibitively slow and wasteful when you only touch a
fraction of the data (dev branches, analytics on recent rows, debugging a few
records). Lazy clone makes "fork a production-sized database" a sub-second
operation: the remote is read on demand, writes stay local and diverge.

## Goals

- **Instant clone**: schema only up front, no data moved.
- **Read-through**: any read returns correct data (local if present, else
  remote), with predicate pushdown so a selective read fetches only matching
  rows from the remote.
- **Diverging writes**: INSERT/UPDATE/DELETE land locally (copy-on-write); the
  clone forks from the remote.
- **Nothing on the remote**: read-only (`SELECT`) access only, with no slots, no
  triggers, no extensions installed remotely.
- **No application change** for plain CRUD: apps connect to the GFS connection
  string and use the cloned tables normally.

## Non-goals (v1)

- Propagating remote changes **into** the local store (no CDC / no re-sync).
  This is not "ignoring the remote": untouched rows are read **live** and reflect
  the current remote; only rows written or warmed locally are frozen (local wins).
  Consequence: if the remote changes, live and frozen rows can be from different
  points in time.
- Cross-table point-in-time consistency (as-of-first-touch; see Design decisions).
- A transparent read **cache** for direct connections (would need a wire-protocol
  proxy, out of scope; reads are live-federation, which is correct and, thanks
  to pushdown, cheap for selective queries).
- MySQL / ClickHouse (PostgreSQL only for now).

## Design decisions

| Decision | Choice | Rationale |
|---|---|---|
| Mechanism | **Overlay view** (`UNION ALL`, local wins) + `INSTEAD OF` triggers | Correct by construction (disjoint row sets); pure SQL, no proxy, nothing on the remote. |
| Consistency | **As-of-first-touch** | Unhydrated rows reflect the remote when read; no snapshot/slot held open → zero load/privilege on the remote. No cross-table PIT guarantee. |
| Remote access | **Read-only strict** | Only `SELECT`; all state lives in GFS. |
| Row identity | **PK, else shortest unique index** (single or composite) | Needed for dedup, tombstones, and `ON CONFLICT`. Tables with no unique key are skipped. |
| Local engine version | **Match the remote major** (probed), or `--database-version`/`--image` | A clone of a v16 database should run v16. |
| Read caching | **Explicit warming primitive only** | Transparent caching needs a proxy (out of scope); the overlay is already correct + pushes down. |

## Architecture

### Instant clone (`gfs clone --from <pg-url>`)

1. Provision a **local GFS PostgreSQL** whose major version matches the remote
   (probed via a one-off `SHOW server_version_num`), unless `--database-version`
   or `--image` is given.
2. Run a **bootstrap** (one `psql` script, via the sidecar pattern) against the
   local database that:
   - creates the `postgres_fdw` server + a `USER MAPPING FOR PUBLIC`;
   - for each mirrored remote schema `S`, imports its tables into a shadow schema
     `gfs_remote_S` (per-table, skipping any table whose type can't resolve
     locally; see Extensions);
   - for each table with a usable key, builds the overlay objects below.

No table data is copied; the clone is ready immediately.

### Database design: objects created

For a remote table `orders` in schema `S` (key column `id`), the bootstrap
creates, **all inside the local GFS database**:

| Object | Kind | Purpose |
|---|---|---|
| `gfs_remote_S.orders` | foreign table (`postgres_fdw`) | Live, read-only window onto the remote table. |
| `S.orders_local` | table (heap) | Local store: rows that were written **or** warmed. PK on the key column(s). |
| `S.orders_deleted` | table (heap) | Tombstones: keys deleted locally (so the remote row is hidden). |
| `S.orders` | **view** | What the application sees (same name as the remote table). |
| `S.orders_<col>_gfsseq` | sequence | Per auto-increment column; default on the view + local store, started past the remote max. |
| `gfs_sync.<S>_orders_{ins,upd,del}()` | functions | `INSTEAD OF` trigger bodies (copy-on-write). |

Plus two shared objects:

- `gfs_sync.table_meta(schema_name, table_name, key_cols)`: registry of overlaid
  tables and their conflict key.
- `gfs_sync.warm_for_query(sql)`: optional cache-warming primitive (below).

The overlay view:

```sql
CREATE VIEW S.orders AS
  SELECT * FROM S.orders_local
  UNION ALL
  SELECT r.* FROM gfs_remote_S.orders r
   WHERE NOT EXISTS (SELECT 1 FROM S.orders_local   l WHERE l.id = r.id)
     AND NOT EXISTS (SELECT 1 FROM S.orders_deleted d WHERE d.id = r.id);
```

A row is served from the local store if present, else as a tombstone-filtered
remote row; the sets are disjoint, so **every row appears exactly once**.
Aggregates (`count(*)`, `sum`) are therefore correct with no delta math.

Schemas introduced in the GFS-local database:
- `gfs_remote_<S>`: one shadow schema per mirrored remote schema, holding the
  foreign tables (keeps remote names collision-free across schemas).
- `gfs_sync`: the catalog (`table_meta`), the warming function, and the trigger
  functions.
- the real schemas `<S>`: hold the overlay views, local stores, tombstones, and
  sequences (preserving the remote's schema layout).

### Diagram

```
        application  (any SQL client, connects to the GFS connection string)
              │  SELECT / INSERT / UPDATE / DELETE   (no code change)
              ▼
 ┌──────────────────────────────── GFS-local PostgreSQL (the clone) ───────────┐
 │                                                                              │
 │  schema S                                                                    │
 │  ┌────────────────────────── VIEW  S.orders ──────────────────────────┐     │
 │  │  SELECT * FROM S.orders_local                                       │     │
 │  │  UNION ALL                                                          │     │
 │  │  SELECT r.* FROM gfs_remote_S.orders r                              │     │
 │  │     WHERE NOT EXISTS (orders_local l WHERE l.id=r.id)               │     │
 │  │       AND NOT EXISTS (orders_deleted d WHERE d.id=r.id)             │     │
 │  └─────────▲───────────────────────────────────────────┬─────────────┘     │
 │   INSTEAD OF│ins/upd/del (copy-on-write)                │ reads               │
 │   ┌─────────┴─────────┐  ┌──────────────────┐           │                     │
 │   │ TABLE orders_local│  │ TABLE            │           │                     │
 │   │ (written+warmed)  │  │ orders_deleted   │           │                     │
 │   └───────────────────┘  └──────────────────┘           │                     │
 │                                                          ▼                     │
 │  schema gfs_remote_S   FOREIGN TABLE gfs_remote_S.orders ──┐ postgres_fdw      │
 │  schema gfs_sync       table_meta · warm_for_query() · triggers                │
 └────────────────────────────────────────────────────────────┼─────────────────┘
                                                                │ SELECT only
                                                                ▼
                                       remote PostgreSQL  (source, untouched, read-only)
```

### Read path

A read of the view returns local rows plus remote rows not shadowed/tombstoned.
`postgres_fdw` **pushes selective predicates** to the remote (verified via
`EXPLAIN VERBOSE`: `Remote SQL: … WHERE ((id = 5))`), so a selective query
fetches only the matching rows, not the whole remote table. The
`NOT EXISTS` anti-joins use the PK indexes on `orders_local`/`orders_deleted`.

Reads are **live-federation**: an unhydrated row reflects the remote at read
time. There is no transparent read cache for direct connections (that would need
a proxy). Optional warming is available as a primitive:

```sql
SELECT gfs_sync.warm_for_query($$ SELECT * FROM orders WHERE id BETWEEN 1 AND 100 $$);
```

It `EXPLAIN (VERBOSE)`s the query, finds the foreign scans, and copies exactly
the rows each was about to serve into the local store (`ON CONFLICT DO NOTHING`).
This is an optimisation, never required for correctness; it is not wired onto the
read path (PostgreSQL has no `SELECT` trigger, and apps bypass `gfs query`).

### Network elision (`gfs_sync.warm_range`)

`warm_for_query` gives **ownership** (rows survive locally) but not network
savings: the overlay's `NOT EXISTS` anti-join references local tables, so it
can't be pushed to the remote — a repeated read re-contacts the remote and the
already-local rows are filtered out GFS-side.

To actually stop re-reading cached rows, GFS tracks fully-hydrated key ranges and
attaches a **CHECK constraint to the foreign table** declaring it holds only rows
*outside* those ranges. With `constraint_exclusion = on` (set on the clone DB),
the planner **refutes** a query predicate that falls inside a cached range and
**prunes the foreign scan entirely** — zero remote contact. The CHECK over-states
the data on purpose, but is used only for pruning; unconstrained scans still read
the foreign table and the anti-join keeps them correct.

```sql
SELECT gfs_sync.warm_range('public', 'orders', '1', '1000');
-- now: SELECT * FROM orders WHERE id = 42  → served locally, foreign scan pruned
--      SELECT * FROM orders WHERE id = 5000 → still federated to the remote
```

`warm_range` hydrates `[lo,hi]`, records it in `gfs_sync.cached_range`, and
rebuilds the exclusion CHECK (`gfs_sync.rebuild_exclusion`). It is `SECURITY
DEFINER` (a low-privilege proxy/cron can drive it) and works for **single-column,
orderable keys**; composite-key tables keep the correct overlay without elision.
Validated end-to-end in `docs/rfcs/008-remote-clone/poc-elision/` and the
`clone_warm_range_elides_remote_reads` e2e test. The *trigger* (when to warm)
stays external — explicit, a `pg_cron` job, or the proxy.

### Write path (copy-on-write)

The view is not auto-updatable (it has a `UNION`), so writes go through
`INSTEAD OF` triggers:

- **INSERT** → upsert into `orders_local`; clear any tombstone for the key.
- **UPDATE** → upsert the new row into `orders_local` (copy-up if it was a
  remote-only row); if the key changed, tombstone the old key.
- **DELETE** → remove from `orders_local` and add a tombstone.

After the first write the clone has diverged from the remote; GFS
commit/branch/snapshot semantics apply to the local stores like any other data.

### Keys (single & composite)

Each overlaid table needs a unique key. The bootstrap discovers it via the
remote catalog (dblink): **primary key first, else the shortest unique index**,
single-column or composite. The key drives the local store PK, the tombstone
table, the view's `NOT EXISTS` joins, and the triggers' `ON CONFLICT`. Tables
with no primary key and no unique index are **skipped** (no overlay view). Key
type is irrelevant (uuid, text, composite all work; the mechanism only relies on
equality and a unique index).

### Multiple schemas

`--from …?schema=a,b` mirrors specific schemas; omitted means **all non-system
schemas**, discovered at bootstrap. Each remote schema `S` is imported into its
own shadow `gfs_remote_S`, and its overlay objects live in a local schema `S`,
so same-named tables in different schemas don't collide.

### Source extensions

`IMPORT FOREIGN SCHEMA` needs the column types to exist locally. The bootstrap
first **mirrors the remote's extensions best-effort** (`CREATE EXTENSION IF NOT
EXISTS` each; contrib types like `citext`/`hstore` resolve), then imports
**per table**, skipping (with a warning) any table whose type is unavailable
(e.g. `pgvector`'s `vector` on the default image). To clone such tables with full
fidelity, provision a local image that bundles the extension:
`gfs clone … --image pgvector/pgvector:pg16`.

## App-visible limitations & remediations

Plain CRUD (SELECT/INSERT/UPDATE/DELETE) needs **no application change**. The
following differ from a real table because the relation is a view:

| Limitation | Status / remediation |
|---|---|
| **Auto-increment**: `IMPORT FOREIGN SCHEMA` drops identity/serial defaults → local inserts omitting the key would fail | **Fixed.** A local sequence per auto-increment column starts just past the remote max and is set as the default on **both the view and the local store** (the view default populates `NEW` before the `INSTEAD OF` trigger). Inserts work and never collide with existing remote keys. |
| **Server-side column defaults** (`DEFAULT now()`, `uuid_generate_v4()`, constants): `IMPORT FOREIGN SCHEMA` drops these too → an app omitting a `NOT NULL DEFAULT` column would insert NULL and fail | **Fixed.** Non-identity / non-`nextval` defaults are mirrored verbatim onto **both the view and the local store** (best-effort; a default that cannot resolve locally is skipped, not fatal). |
| **User-defined types** (ENUM, composite, domains) | **Fixed.** Discovered from the remote and recreated locally before import, in dependency order (ENUMs → DOMAINs → COMPOSITEs; composites use bounded multi-pass for composite-on-composite refs), same schema/name. ENUM label order, DOMAIN base type + DEFAULT + NOT NULL + CHECKs, and COMPOSITE attributes are preserved, so such tables clone fully and their constraints/ordering hold locally. |
| **`pg_stat_statements` (preload-only extensions)** | The extension is *created* on the clone but its view stays empty/unavailable: it needs `shared_preload_libraries`, which the source sets (via its `command`) but the GFS-provisioned engine does not. |
| **Per-role FDW access** | **Fixed.** `USER MAPPING FOR PUBLIC` → any local role reads through the overlay. |
| **DDL** (`ALTER TABLE`, `CREATE INDEX`, `TRUNCATE`) fails on a view | **Mitigated.** Each view carries a `COMMENT` pointing DDL/indexes at `<t>_local` or the source. Transparent DDL routing is deferred (a precise guard is not reliable at `ddl_command_start`). |
| **`SELECT … FOR UPDATE`** on a `UNION ALL` view | Documented; inherent. Optional future helper `gfs_sync.lock_row(t, key)`. |
| **Remote DB-side triggers / FK / CHECK** | Don't run on local writes; a data overlay, not a behavioural replica. Documented. |
| **Catalog introspection** sees `relkind='v'` | ORMs do CRUD fine; migration tools should target the source. Documented. |

Auto-increment caveat: because unhydrated reads are live, an extremely
write-heavy remote could surface a remote key beyond the local sequence start;
acceptable under the as-of-first-touch / read-only-remote assumption.

## Mapping to GFS

- `gfs clone --from <pg-url>` (alongside `gfs init`); `cmd_clone` parses the URL,
  provisions the local engine (reusing the init path), then runs the bootstrap.
- Domain use case `CloneRepoUseCase` (`detect_remote_version` + bootstrap run).
- PostgreSQL provider emits the bootstrap SQL (`clone_bootstrap_spec`).
- The bootstrap runs via the existing **sidecar pattern** (like `export`/`import`).
- All overlay objects are ordinary objects in the GFS PostgreSQL, so they are
  snapshotted and branched by the storage layer like any other data.

## CLI

```
gfs clone --from postgres://user:pass@host:5432/db [PATH]
          [--database-version 17]      # else: probed from the remote
          [--image pgvector/pgvector:pg16]   # bundle a source extension; pins version
          [--platform linux/amd64]     # run an image lacking a native-arch manifest
          [--port 5432]                # bind the local container to a host port
          [--from '…?schema=a,b']      # mirror specific schemas (default: all)
```

Quote the URL in single quotes if the password contains shell metacharacters
(e.g. a backtick).

## Roadmap / future work

- Transparent read caching + remote pruning (would require a wire-protocol
  proxy); currently out of scope.
- Auto image selection for known extensions (e.g. pgvector); degrade-to-`text`
  for extensions with no available image.
- MySQL / ClickHouse providers.
- Local-sequence and FK ergonomics; optional `FOR UPDATE` helper.

## Alternatives considered

- **Remote snapshot import (RDS/GCP)**: pull a provider snapshot and init from
  it. Fast for a full copy but transfers everything and depends on
  provider-specific tooling; no on-demand semantics.
- **Foreign tables as range partitions**: was prototyped and **rejected as
  incorrect**: PostgreSQL trusts partitions and never pushes the partition bound,
  and a foreign partition mapping the whole remote table does not restrict to its
  range, so unqualified / non-key scans over-count (N× the rows). The overlay
  replaces it.
- **Wire-protocol proxy**: would enable transparent read caching/pruning for
  direct connections, but is a large build and only adds a performance cache; the
  overlay already gives correct reads (with pushdown) and diverging writes.
