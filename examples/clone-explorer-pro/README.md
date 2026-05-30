# Clone Explorer **Pro**

A richer, side-by-side playground for the GFS lazy clone (copy-on-read) feature.
Where [`clone-explorer`](../clone-explorer) shows a single table, **Pro** exercises a
realistic multi-table schema with joins, fuzzy text search, temporal filters, an
aggregate dashboard, and copy-on-write writes — so you can watch *exactly* which
queries the lazy clone can serve locally and which still federate to the source.

```
SOURCE (Postgres 16, seeded large)  ───clone──▶  CLONE (GFS lazy clone)
        every query runs on both, side by side, with a "served from" badge
```

## What it demonstrates

| Tab | Query shape | On the clone |
|-----|-------------|--------------|
| **Products (range)** | `WHERE id BETWEEN lo AND hi` (key predicate, inlined bounds) | `remote` until warmed, then **`local` (elided)** — the key-range `CHECK` + `constraint_exclusion` prunes the foreign scan |
| **Fuzzy search** | `name ILIKE '%term%'` (non-key, parameterized) | `remote` while partial; **`local` once the whole table is cached** (auto-promotion to `whole_table`) |
| **Reviews search** | trigram search on a text body | same as fuzzy |
| **By category** | `products ⋈ categories` | `remote` — the anti-join in the overlay view blocks predicate push-down |
| **Customer orders** | 3-table `customers ⋈ orders ⋈ order_items` | `remote` |
| **Recent orders** | temporal filter `placed_at > now() - N days` | `remote` |
| **Dashboard** | revenue/category aggregate over joins | `remote` (federates) |
| *(writes)* | place order / write review | copy-on-write: the clone diverges, **the source is untouched** |

The honest takeaway it's built to show: **a key-predicate scan can be fully elided**
(range `CHECK`, or `whole_table` once everything is cached), but **joins, aggregates
and non-key filters over a partially-cached table still federate** — that's inherent
to the overlay (`_local UNION ALL foreign WHERE NOT EXISTS …`), not a bug.

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
# from this directory (needs: docker, pnpm, and a built gfs binary)
cd ../.. && cargo build -p gfs-cli && cd -      # if target/debug/gfs is missing
./scripts/run.sh
# open http://localhost:8788  →  click "Clone the source"
```

The source is seeded large by default (1M products, 300k orders, …). Override via env:

```bash
SEED_PRODUCTS=20000 SEED_ORDERS=5000 SEED_REVIEWS=4000 SEED_EVENTS=0 ./scripts/run.sh
```

### Proxy mode (auto-warming)

```bash
./scripts/run.sh --proxy
```

Fronts the clone with the [`guepard-proxy-v2`](../../crates/applications/proxy) binary,
built and run on the host (`cargo build` each run) in **auto-discovery** mode: with no
`--backend`, it finds the clone via Docker labels (`gfs.role=clone`) and fronts it on
`localhost:55454` (map at `curl localhost:9091/clones`). It warms cached ranges in the background and periodically applies
the exclusion, so pages flip from `remote` → `local` on their own as you browse — no
manual "warm" button needed. A ⚡ badge appears in the header.

## How "served from" is decided

Each scenario runs the query **and** an `EXPLAIN (FORMAT JSON)` of it. If the plan
still contains a `Foreign Scan`, the row was (at least partly) read from the source →
`remote`. If the foreign branch was pruned (range `CHECK` refuted, or the view rewritten
to `whole_table`), the plan is local-only → `local`. On the source itself it's `source`.

## Warming, by hand vs. by proxy

`gfs_sync.warm_range(schema, table, lo, hi)` only **hydrates** rows into the local
store — the `AccessExclusive` `CHECK` rebuild is *decoupled* (so frequent hydration
never blocks readers). Something has to call `gfs_sync.refresh_exclusions()` to actually
build the range `CHECK` / promote to `whole_table`:

- **direct mode** — the `/api/warm` endpoint calls `refresh_exclusions()` right after
  `warm_range()`, so the manual "warm this page" / "warm whole products" buttons elide
  immediately.
- **proxy mode** — the proxy's background refresher does it on an interval.

## Tear down

```bash
docker compose down -v
rm -rf clone-repo
```
