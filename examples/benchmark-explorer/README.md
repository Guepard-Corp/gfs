# GFS Benchmark Explorer

**The** benchmark for the GFS lazy clone (copy-on-read of an external PostgreSQL) —
one set of scenarios over a real multi-table workload (**TPC-H**), driven two ways:

- **UI** (`scripts/run.sh`) — a side-by-side playground: run each query on the
  **SOURCE** and on a lazy **CLONE**, watch the router pick a path, tune the cost
  weights live.
- **Headless** (`scripts/bench.sh`) — the same scenarios, no browser: asserts
  `clone == source` + the expected route per shot, proves convergence, writes
  objective metrics with a regression guard. CI-friendly, exit 0/1.

```
SOURCE (Postgres 16, TPC-H)  ──clone──▶  CLONE (gfs copy-on-read, planner hook)
   every query runs on both, side by side, with a route badge + router state
```

Both front-ends import the same definitions (`server/src/bench.ts`): the scenario
SQL, the clone provisioning, the pinned cost weights, and the route detection are
defined **once**. The UI wires them to HTTP (`server/src/index.ts`); the headless
runner wires them to a CLI (`server/src/headless.ts`).

## What you see

| Path | Query | On the clone |
|------|-------|--------------|
| **P1** range | `lineitem WHERE l_orderkey BETWEEN …` | **fetched** (the key range) on 1st read, **local** after (elision) |
| **P2** selective | `lineitem WHERE l_quantity = v` | **federated** → **partial** (only the matching *slice*, capped) → **local** |
| **P3** join | TPC-H Q1 / Q3 / Q5 / Q10 | **federated** — the whole join/aggregate is pushed to the source (`postgres_fdw`) |
| **P5** temporal | `orders WHERE o_orderdate BETWEEN …` | **fetched** (the time range) then **local** (elision); a too-wide window federates (capped) |
| **convergence** | a join, then `gfs.warm` its tables | **federated** cold → **local** after warming (zero `Foreign Scan`) — the lazy clone converging to a self-sufficient copy |

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

### Headless (CI)

Same source, same scenarios, no browser — asserts correctness + route per shot and
writes `benchmark.results.tsv` (append-only, compared to the previous run):

```bash
SF=1 ./scripts/bench.sh                 # fast proof (~1 GB), exit 0 on all-pass
LABEL=my-run SF=1 ./scripts/bench.sh    # label the results row
```

The verdict is `N passed, M failed`; the objective block reports
`source_ops` / `rows_pulled` / `local_pct` and flags IMPROVED / REGRESSED vs the
last persisted run — that's the regression guard.

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
