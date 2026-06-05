# GFS Benchmark Explorer

A side-by-side playground for the GFS lazy-clone **cost router**, on a real
multi-table benchmark (**TPC-H**). Run range / selective / join queries on the
**SOURCE** and on a lazy **CLONE**, and watch the router pick a path:

```
SOURCE (Postgres 16, TPC-H)  ──clone──▶  CLONE (gfs copy-on-read, planner hook)
   every query runs on both, side by side, with a route badge + router state
```

Where [`clone-explorer-pro`](../clone-explorer-pro) is a product demo, this one
opens the hood: it shows **which decision the cost router makes per query and why**,
exposes the router's catalog, and lets you **tune the cost weights live**.

## What you see

| Path | Query | On the clone |
|------|-------|--------------|
| **P1** range | `lineitem WHERE l_orderkey BETWEEN …` | **fetched** (the key range) on 1st read, **local** after (elision) |
| **P2** selective | `lineitem WHERE l_quantity = v` | **federated** → **fetched** (only the matching *slice*, capped) → **local** |
| **P3** join | TPC-H Q1 / Q3 / Q5 / Q10 | **federated** — the whole join/aggregate is pushed to the source (`postgres_fdw`) |

- **Route badge** per pane: `fetched` · `partial` · `federated` · `local` (or
  `source`), derived honestly from the extension's copy-on-read counters
  (`gfs.clones`) before/after each query.
- **Router state** panel — per-table `whole_cached` / `partial_rows` / cached
  ranges / cached predicates / rows fetched / federate calls (`gfs.clones`).
- **Cost weights** editor — edit `gfs.cost` (net, source, ceiling, partial_max_frac,
  …) and re-run to watch routing change. Lower the `ceiling` → more tables become
  too-big-to-own and federate.
- **`plan`** — `EXPLAIN` the clone's plan: a single `Foreign Scan` over the joined
  relations proves the join was **pushed down** (needs `use_remote_estimate`).
- **`reset clone`** — clear the hydration state and replay the paths from cold.

## Run it

Needs **docker**, **pnpm**, **duckdb** (generates TPC-H — no `dbgen` build), a built
`gfs` binary, and the `gfs-postgres:16` image.

```bash
cd ../.. && cargo build -p gfs-cli && cd -            # if target/debug/gfs is missing
SF=1 ./scripts/run.sh
# open http://localhost:8789  →  click "Clone the source"
```

The TPC-H **source is persistent** (a named Docker volume per scale factor): the
slow generate+load happens **once**; reruns reuse it and only rebuild the lazy
clone. Scale up with `SF=10` (~16 GB) or `SF=50` (~100 GB) — each SF gets its own
volume.

```bash
SF=10 ./scripts/run.sh                  # bigger source, reused on later runs
REBUILD_SOURCE=1 SF=1 ./scripts/run.sh  # force regenerate
DROP_SOURCE=1 SF=1 ./scripts/run.sh     # delete the source container + volume
```

## How the route is decided

The clone's tables are **real** (no foreign scan in the app's plan), so copy-on-read
happens *inside* the planner hook — invisible to `EXPLAIN`. The server surfaces it
the honest way: it snapshots the extension's cumulative counters
(`rows_fetched`, `federate_calls`, complete predicates) **before and after** each
query. A query that added a complete predicate → `partial`; raised `rows_fetched` →
`fetched`; raised `federate_calls` only → `federated`; neither → `local`.

## Notes baked in

- After cloning, the server pins router weights and sets the `ceiling` to
  `0.1 × lineitem`'s whole-own cost (so the big facts stay not-ownable and exercise
  P2/P3) and does **not** calibrate (calibrate would make them ownable and collapse
  the partial path).
- Federation pushes joins to the source because the clone's foreign server is
  created with `use_remote_estimate` (see `clone_bootstrap.sql`); without it a
  6-table join over millions of rows is fetched row-by-row instead.

## Tear down

```bash
DROP_SOURCE=1 SF=1 ./scripts/run.sh
rm -rf clone-repo
```
