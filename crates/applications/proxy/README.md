# guepard-proxy-v2

A thin PostgreSQL wire-protocol proxy that fronts a configured backend (or
**auto-discovered** GFS clones), observes queries for **telemetry**, and triggers
**in-DB cache warming** for GFS lazy clones.

Inspired by `guepard-proxy` (v1) but deliberately stripped down:

| v1 | v2 |
|---|---|
| SNI-based routing + Nomad discovery + `/api/travel` | **single `--backend`**, or **Docker label auto-discovery** (`--discover`) — works local & cloud |
| TLS-required (for SNI) | TLS optional (plaintext refuses `SSLRequest` with `N` for now) |
| byte pass-through only | **message-aware sniffing** (Query/Parse) for telemetry + warming |
| — | **Prometheus `/metrics`** |

Lifted in spirit from v1: the PG v3 protocol handling (`src/pg.rs`, a trimmed
`pq_proto`) and the bidirectional copy pattern (`src/proxy.rs`).

## What it does

- Accepts client connections; **terminates TLS** when `--tls-cert/--tls-key` are
  given (`sslmode=require`), else refuses SSL with `N` so the client retries
  plaintext. Plaintext clients work whether or not TLS is enabled.
- Connects to `--backend`, forwards traffic **verbatim** in both directions.
  With `--backend-tls` the proxy↔backend link is encrypted (PostgreSQL
  `SSLRequest` + TLS; `--backend-tls-insecure` to skip cert verification). It
  also rewrites the backend's SASL auth to drop `SCRAM-SHA-256-PLUS`, since
  channel binding can't cross a TLS-terminating proxy.
- **Sniffs** the client→server direction (never alters it) to classify queries
  (`read`/`write`/`ddl`/`txn`/`other`) — simple `Query`, extended `Parse`, and
  `Bind` (parameter values).
- **Warms the cache** (`--warm`): observed simple-`Query` reads **and**
  parameterized `Parse`+`Bind` reads (text params substituted) trigger an async
  `SELECT gfs_sync.warm_query_chunks($1)` on a low-priv side connection, deduped
  by query text (TTL) and reconnecting on failure. That in-DB function expands
  the query's key span to chunk boundaries and **hydrates** the chunk(s). A
  periodic **refresher** (`--refresh-interval`) then calls
  `gfs_sync.refresh_exclusions()` to apply hydrated ranges as **remote-scan
  elision** — so the read-blocking `ALTER` is batched on a timer, not run per
  read. A one-time capability probe makes these a **no-op with zero overhead**
  on backends that aren't GFS clones.
- **Cache-coverage telemetry** (`--cache-metrics`): periodically scrapes the
  in-DB `gfs_sync` catalog and exposes coverage gauges. No-op on non-clone
  backends. (The role needs SELECT on `gfs_sync`; a superuser locally.)
- Exposes Prometheus metrics.

The proxy only ever talks to the **database** — never to GFS. Its contract with
the clone is the in-DB `gfs_sync.*` functions.

## Run

```bash
cargo run -- --backend 127.0.0.1:5432 --listen 0.0.0.0:6432 --metrics 127.0.0.1:9099 --warm
# then point a client at the proxy:
psql "host=127.0.0.1 port=6432 user=postgres password=postgres sslmode=prefer"
curl -s http://127.0.0.1:9099/metrics | grep '^proxy_'
```

Config via flags or env (`PROXY_BACKEND`, `PROXY_LISTEN`, `PROXY_METRICS`,
`PROXY_LOG`, `PROXY_WARM`).

### Auto-discovery (`--discover`)

**Omit `--backend`** (or pass `--discover` explicitly) and the proxy **finds GFS
clones itself** via the Docker API, fronting each on its own listener — no
per-clone config:

```bash
cargo run -- --warm --cache-metrics --metrics 127.0.0.1:9099   # no --backend → discovery
# list what it found and where it put each clone:
curl -s http://127.0.0.1:9099/clones
# {"clones":[{"container":"gfs-postgres-...","backend":"127.0.0.1:55470",
#             "listen_port":55600,"remote":"host.docker.internal:55452"}]}
psql "host=127.0.0.1 port=55600 user=postgres password=postgres sslmode=disable"
```

It lists running containers labelled `gfs.managed=true` + `gfs.role=clone` +
`gfs.provider=postgres` (set by `gfs clone`), reads each one's published
`5432/tcp` port, and binds a listener at `--listen-base` (default `55500`) and
upward — one per clone. A periodic reconcile (`--discover-interval`, default 3s)
picks up clones created or destroyed at runtime, freeing the port when a clone
disappears. The live clone→port map is served at `GET /clones`, alongside
`/metrics` on the same `--metrics` address.

Discovery is read-only on Docker — the proxy still only ever *connects* to the
databases; it never creates or mutates a container. It is gated behind the
`discovery` cargo feature (on by default); `--no-default-features` builds a lean,
single-`--backend` proxy with no Docker dependency.

### TLS

```bash
cargo run -- --backend 127.0.0.1:5432 --tls-cert cert.pem --tls-key key.pem
psql "host=127.0.0.1 port=6432 user=postgres password=postgres sslmode=require"
```

## Metrics

`proxy_connections_active` (gauge), `proxy_connections_total`,
`proxy_queries_total{kind}`, `proxy_bytes_total{direction}`,
`proxy_warm_observed_total`, `proxy_warm_calls_total{outcome}`,
`proxy_messages_skipped_large_total`, and (with `--cache-metrics`)
`proxy_cache_ranges`, `proxy_cache_tables`, `proxy_overlay_tables` (gauges).

## Status & roadmap

Done: client **TLS termination** + **backend TLS** (with SCRAM-PLUS rewrite),
query sniffing (simple `Query`, extended `Parse`/`Bind`), Prometheus metrics, the
**warmer** (side connection, capability probe, dedup, reconnect, chunk-driven
elision via `warm_query_chunks`, parameterized-query warming), the periodic
**refresher** (decoupled CHECK rebuild), **cache-coverage telemetry**
(`--cache-metrics`), and **Docker auto-discovery** of clone containers
(`--discover`, `/clones`).

Next:
- **Extended-protocol depth**: warm parameterized `Parse` reads using `Bind`
  values (today only literal-predicate simple queries are warmed).
- **Multi-statement classification**: a simple-query message with several
  statements is currently labelled by its leading keyword only.

The *network elision* (not re-reading cached rows from source) lives **in-DB**
(CHECK-constraint on the foreign table + `constraint_exclusion`, validated PoC in
the `gfs` repo). This proxy provides the **automatic trigger + observability**.
