# GFS Clone Explorer

A small web app that puts a **source** PostgreSQL and its **GFS clone**
side by side, so you can watch what a lazy copy-on-read clone actually does.

```
┌───────────────────────────┐   ┌───────────────────────────┐
│ SOURCE (upstream)         │   │ CLONE (GFS)               │
│ products: 500,000         │   │ products: 500,000         │
│ [source · 1.2 ms]         │   │ [remote read · 14 ms]     │  ← reads through
│ id  name      price       │   │ id  name      price       │
│ ...                       │   │ ...                       │
└───────────────────────────┘   └───────────────────────────┘
                                   ↧ Warm this page
                                 [local (elided) · 0.4 ms]    ← now served locally
```

The clone is created with `gfs clone` and copies **nothing** up front: it reads
through to the source on demand. The app makes the three core behaviours visible:

| Action | What you see | What it proves |
|--------|--------------|----------------|
| Page through both panels | clone badge says **remote read**, source says **source** | reads federate to the remote on demand (no bulk copy) |
| **Warm this page** (clone) | badge flips to **local (elided)**, latency drops, `cached ranges` grows | `gfs_sync.warm_range` hydrates the id range and the planner prunes the foreign scan |
| **Order** / **edit price** on the clone | the clone diverges, `local rows` grows, the source is **unchanged** | copy-on-write: the clone owns its writes, the source stays read-only |
| **edit price** on the source | a **cold** clone row reflects it; a **warmed** row does not | warmed ranges are frozen; cold rows are still live read-through |

`products` has a dense `bigint` key (required for range elision) and `orders`
has a `STORED` generated column (`total_cents`), so placing an order on the clone
also exercises the overlay's generated-column write path.

## Prerequisites

- Docker Desktop (the clone reaches the source via `host.docker.internal`)
- Node 20+ and [pnpm](https://pnpm.io) 9+
- A built `gfs` binary: `cargo build -p gfs-cli` from the repo root
  (defaults to `target/debug/gfs`; override with `GFS_BIN`).

## Run

```bash
cd examples/clone-explorer
cp .env.example .env        # optional: tweak SEED_ROWS / ports
pnpm demo                   # = bash scripts/run.sh
# open http://localhost:8787
```

Seed size is configurable (`SEED_ROWS`, default 500k). Push it to a few million
to make the dump-vs-lazy difference obvious — the clone still starts instantly.

## Proxy mode (auto-warming)

```bash
pnpm demo -- --proxy        # or: bash scripts/run.sh --proxy
```

This builds and starts the **guepard proxy** binary
(`crates/applications/proxy/`, `cargo build` each run so source changes are picked up) in **auto-discovery** mode (no `--backend`): it watches Docker for the clone the
UI creates (labels `gfs.role=clone`/`gfs.provider=postgres`) and fronts it on
`localhost:55444` — the live clone→port map is at `curl localhost:9090/clones`. Now you don't
click anything: as you **page through the clone**, the proxy observes your reads,
calls `gfs_sync.warm_query_chunks` to hydrate the touched chunk, and a periodic
refresher applies the exclusion — so pages flip from **remote read** to
**local (elided)** on their own. The manual "Warm this page" button is replaced
by an **⚡ auto-warm** badge.

This is the showcase of the project's intent: a normal app, connected to the
clone through the proxy, transparently stops re-reading from the source as it
warms — no app changes, no manual warming. The proxy is a thin sidecar that only
talks to the database (it calls the in-DB `gfs_sync.*` functions); correctness is
always guaranteed by the overlay, independent of the proxy.

Proxy Prometheus metrics: `curl localhost:9090/metrics | grep '^proxy_'`
(queries seen, warm calls, `proxy_cache_ranges` / `proxy_overlay_tables`).

> The proxy terminates client TLS only if configured; here the app↔proxy link is
> plaintext and proxy↔clone is local. Backend TLS, client TLS, and parameterized
> (`Parse`/`Bind`) warming are all supported — see `crates/applications/proxy/README.md`.

## Architecture

```
docker-compose.yml          source PostgreSQL 16 (plain upstream, no GFS objects)
sql/                        extensions + schema + seed
server/                     Fastify: two connections (source | clone), EXPLAIN-based
                            "served from" detection, warm / price / order endpoints
web/                        Vite + React: two <Panel db=…> side by side
scripts/run.sh              seed → gfs clone → build web → serve
```

The server keeps two connections and routes each request by `?db=source|clone`.
For the clone it runs `EXPLAIN (FORMAT JSON)` on the page query: a remaining
`Foreign Scan` means the remote was contacted, otherwise the range is cached and
elided. Correctness never depends on elision — an unconstrained `count(*)` over
the clone stays exact (the overlay's anti-join dedups remote vs local).

## Dev (hot reload)

```bash
pnpm --filter clone-explorer-server start    # terminal 1 (after the source + clone are up)
pnpm --filter clone-explorer-web dev         # terminal 2, proxies /api -> :8787
```

## Tear down

```bash
docker compose down -v
rm -rf clone-repo
```
