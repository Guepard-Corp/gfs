# Clone Explorer **Pro**

A richer, side-by-side playground for the GFS lazy clone (**copy-on-read**) feature.
Where [`clone-explorer`](../clone-explorer) shows a single table, **Pro** exercises a
realistic multi-table schema with joins, fuzzy text search, temporal filters, an
aggregate dashboard, and writes — so you can watch the clone **fetch data from the
source on first read and serve it locally thereafter**.

```
SOURCE (Postgres 16, seeded large)  ───clone──▶  CLONE (gfs copy-on-read TAM)
        every query runs on both, side by side, with a "served from" badge
```

The clone's tables are **real tables** (`relkind='r'`, with the source's indexes)
— not overlay views. A **planner hook** in the `gfs` extension routes each query
two ways (the source is only reachable over SQL, so this is a logical clone, not a
page-level one):

- **`fetched` → `local` (hydrate)** — a query that bounds a table's **range key**
  (`id BETWEEN`, `id > …`) fetches the missing key **range** into the real local
  table, records it, and runs local. Re-asking a covered range hits **no source**
  (elision); adjacent ranges coalesce.
- **`federated`** — a query with no range-key bound on a not-yet-owned table
  (fuzzy text, non-key join, aggregate) is **pushed whole to the source**:
  `postgres_fdw` computes the join/aggregate remotely and returns the result.
  **Nothing is materialized locally** — so a dashboard over a multi-TB source
  doesn't pull millions of rows.
- **`local` (owned)** — once a table is fully materialized (`gfs.warm`, or as
  ranges accumulate), it is served locally even for federate-class queries — **no
  source contact**. The clone converges to a self-sufficient copy.

## What it demonstrates

| Tab | Query shape | On the clone |
|-----|-------------|--------------|
| **Products (range)** | `WHERE id BETWEEN lo AND hi` | `fetched` (the range) on first read, then **`local`** (range elided) |
| **Fuzzy search** | `name ILIKE '%term%'` | `federated` — pushed to the source's trigram index (0 rows materialized) |
| **Reviews search** | trigram search on a text body | same as fuzzy |
| **By category** | `products ⋈ categories` (by name) | `federated` — join pushed to the source |
| **Customer orders** | 3-table `customers ⋈ orders ⋈ order_items` | `federated` — join pushed to the source |
| **Recent orders** | temporal filter `placed_at > now() - N days` | `federated` — pushed to the source |
| **Dashboard** | revenue/category aggregate over joins | `federated` — aggregate computed at the source, ~12 rows back |
| **warm whole products** | `gfs.warm('products')` | materialize + **own** a table → its queries then serve `local` |
| *(writes)* | place order / write review | the clone diverges, **the source is untouched** |

The takeaway: the clone fetches **ranges** of what you touch by key, **pushes**
analytical work (joins/aggregates/text) to the source instead of dragging the
data over, and **converges** to a self-sufficient local copy as ranges fill in or
you `warm` tables. Foreign keys do **not** trip when a child is fetched before its
parent: the clone
drops FK constraints at bootstrap (the source already enforced them; the clone is
a working copy, like a replica), and the parent fetches on its own first touch.

## Schema

- `categories` — `smallint` PK
- `customers` — `uuid` PK + `bigint` surrogate `n` (for human-friendly addressing)
- `products` — `bigint` PK, GIN trigram index on `name`
- `orders` — `bigint` PK, FK to customers, `placed_at` timestamptz
- `order_items` — **composite PK** `(order_id, line)`, generated `total_cents`
- `reviews` — `bigint` PK, GIN trigram index on `body`
- `events` — `bigint` PK, time-series

## Run it

```bash
# needs: docker, pnpm, a built gfs binary, and the gfs Postgres image.
cd ../.. && cargo build -p gfs-cli && cd -            # if target/debug/gfs is missing
./scripts/run.sh
# open http://localhost:8788  →  click "Clone the source"
```

The clone **requires** the `gfs` extension — `clone_bootstrap.sql` runs
`CREATE EXTENSION gfs` with no overlay fallback. `run.sh` builds the
`gfs-postgres:16` image (from [`crates/extensions/gfs`](../../crates/extensions/gfs))
on first run if it is missing — a heavy multi-stage build (rustup + cargo-pgrx +
a release build, ~10-20 min the first time). Override with `GFS_IMAGE=…`.

The source is seeded large by default (1M products, 300k orders, …). Override via env:

```bash
SEED_PRODUCTS=20000 SEED_ORDERS=5000 SEED_REVIEWS=4000 SEED_EVENTS=0 ./scripts/run.sh
```

## How "served from" is decided

The clone's tables are real (no foreign scan in the plan), so copy-on-read happens
*inside* the storage layer — invisible to `EXPLAIN`. The explorer surfaces it the
honest way: it snapshots the extension's cumulative `rows_fetched`
(`gfs.clone_stats`) **before and after** each query. If it went up, the query
fetched rows from the source → `fetched`; otherwise it was served locally →
`local`. On the source itself it's `source`. The bar also shows total **rows
fetched (copy-on-read)** for the clone.

## Warming

Reads warm on their own (copy-on-read). To materialize a whole table in one shot,
`gfs.warm('public.<t>')` runs a full seq scan that fetches + writes through every
row (the **warm whole products** button). The old overlay needed an external
warmer/proxy + range `CHECK` elision; the TAM does it in-DB on read, so the proxy
is no longer needed for warming.

## Tear down

```bash
docker compose down -v
rm -rf clone-repo
```
