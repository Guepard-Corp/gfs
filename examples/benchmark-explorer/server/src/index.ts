import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { existsSync } from "node:fs";
import { readFile, rm, mkdir } from "node:fs/promises";
import { spawn } from "node:child_process";

import Fastify from "fastify";
import fastifyStatic from "@fastify/static";

import { source, connFor, pickDb, attachClone, cloneReady, cloneTimeMs, SOURCE_URL, CLONE_URL, type DbName } from "./db.js";

const here = dirname(fileURLToPath(import.meta.url));
const webDist = join(here, "../../web/dist");
const PORT = Number(process.env.SERVER_PORT ?? 8789);

const GFS_BIN = process.env.GFS_BIN ?? "gfs";
const CLONE_DIR = process.env.CLONE_DIR ?? join(here, "../../clone-repo");
const CLONE_PORT = process.env.CLONE_PORT ?? "55621";
const REMOTE_HOST = process.env.REMOTE_HOST ?? "host.docker.internal";
const SOURCE_PORT = process.env.SOURCE_PORT ?? "55620";
const SOURCE_DB = process.env.SOURCE_DB ?? "tpch";
const SOURCE_USER = process.env.SOURCE_USER ?? "app";
const SOURCE_PASS = process.env.SOURCE_PASS ?? "pw";
const DB_VERSION = process.env.DB_VERSION ?? "16";
const CLONE_IMAGE = process.env.CLONE_IMAGE ?? "gfs-postgres:16";

const app = Fastify({ logger: false });
function log(msg: string): void {
  console.log(`${new Date().toISOString().slice(11, 19)} ${msg}`);
}
app.addHook("onResponse", async (req, reply) => {
  if (req.url.startsWith("/api")) log(`${req.method} ${req.url} → ${reply.statusCode} ${reply.elapsedTime.toFixed(0)}ms`);
});
app.setErrorHandler((err: Error & { statusCode?: number }, req, reply) => {
  const code = err.statusCode ?? 500;
  if (code >= 500) log(`ERROR ${req.method} ${req.url}: ${err.message}`);
  reply.code(code).send({ error: err.message });
});

type Params = { lo: number; hi: number; val: number; from: string; to: string };
type Q = { label: string; path: "P1" | "P2" | "P3" | "P5"; hint: string; sql: (p: Params) => string };

const QUERIES: Record<string, Q> = {
  range: {
    label: "Range scan", path: "P1",
    hint: "WHERE l_orderkey BETWEEN lo AND hi → fetch the missing key RANGE into the local table, then serve local (re-run = elision, no source).",
    sql: (p) => `SELECT l_orderkey,l_partkey,l_quantity,l_shipdate FROM lineitem
      WHERE l_orderkey BETWEEN ${p.lo} AND ${p.hi} ORDER BY l_orderkey LIMIT 200`,
  },
  selective: {
    label: "Selective filter", path: "P2",
    hint: "WHERE l_quantity = v on the too-big lineitem → 1st touch federates, 2nd fetches ONLY the matching slice (capped, self-validating), 3rd serves local.",
    sql: (p) => `SELECT l_orderkey,l_linenumber,l_extendedprice FROM lineitem
      WHERE l_quantity = ${p.val} ORDER BY l_orderkey LIMIT 200`,
  },
  q1: {
    label: "Q1 · aggregate", path: "P3",
    hint: "Single-table aggregate over the not-ownable lineitem → federated (computed at the source, nothing materialized).",
    sql: () => `SELECT l_returnflag,l_linestatus,sum(l_quantity)::bigint sum_qty,count(*)::bigint cnt
      FROM lineitem WHERE l_shipdate <= date '1998-09-01'
      GROUP BY l_returnflag,l_linestatus ORDER BY l_returnflag,l_linestatus`,
  },
  q3: {
    label: "Q3 · 3-table join", path: "P3",
    hint: "customer ⋈ orders ⋈ lineitem → the whole join+aggregate is pushed to the source via postgres_fdw.",
    sql: () => `SELECT l_orderkey,o_orderdate,round(sum(l_extendedprice*(1-l_discount))::numeric,2) revenue
      FROM customer,orders,lineitem
      WHERE c_mktsegment='BUILDING' AND c_custkey=o_custkey AND l_orderkey=o_orderkey
        AND o_orderdate<date '1995-03-15' AND l_shipdate>date '1995-03-15'
      GROUP BY l_orderkey,o_orderdate ORDER BY revenue DESC,l_orderkey LIMIT 20`,
  },
  q5: {
    label: "Q5 · 6-table join", path: "P3",
    hint: "region ⋈ nation ⋈ customer ⋈ orders ⋈ lineitem ⋈ supplier → join pushed down (needs use_remote_estimate; without it this fetches 60M rows row-by-row).",
    sql: () => `SELECT n_name,round(sum(l_extendedprice*(1-l_discount))::numeric,2) revenue
      FROM customer,orders,lineitem,supplier,nation,region
      WHERE c_custkey=o_custkey AND l_orderkey=o_orderkey AND l_suppkey=s_suppkey
        AND c_nationkey=s_nationkey AND s_nationkey=n_nationkey AND n_regionkey=r_regionkey
        AND r_name='ASIA' AND o_orderdate>=date '1994-01-01' AND o_orderdate<date '1995-01-01'
      GROUP BY n_name ORDER BY revenue DESC`,
  },
  q10: {
    label: "Q10 · 4-table join", path: "P3",
    hint: "customer ⋈ orders ⋈ lineitem ⋈ nation → join pushed to the source.",
    sql: () => `SELECT c_custkey,n_name,round(sum(l_extendedprice*(1-l_discount))::numeric,2) revenue
      FROM customer,orders,lineitem,nation
      WHERE c_custkey=o_custkey AND l_orderkey=o_orderkey AND o_orderdate>=date '1993-10-01'
        AND o_orderdate<date '1994-01-01' AND l_returnflag='R' AND c_nationkey=n_nationkey
      GROUP BY c_custkey,n_name ORDER BY revenue DESC,c_custkey LIMIT 20`,
  },
  q2: {
    label: "Q2 · part/supplier join", path: "P3",
    hint: "part ⋈ partsupp ⋈ supplier ⋈ nation ⋈ region → join pushed to the source (exercises part/partsupp).",
    sql: () => `SELECT p_partkey,p_mfgr,min(ps_supplycost)::numeric(15,2) min_cost
      FROM part,partsupp,supplier,nation,region
      WHERE p_partkey=ps_partkey AND ps_suppkey=s_suppkey AND s_nationkey=n_nationkey
        AND n_regionkey=r_regionkey AND r_name='EUROPE' AND p_size=15
      GROUP BY p_partkey,p_mfgr ORDER BY min_cost,p_partkey LIMIT 20`,
  },
  temporal: {
    label: "Temporal window", path: "P5",
    hint: "WHERE o_orderdate BETWEEN from AND to on the DATE-keyed orders → fetch the TIME range, then local. Narrow the window INSIDE it → local (elision). A too-wide window federates (capped).",
    sql: (p) => `SELECT o_orderkey,o_orderdate,o_totalprice FROM orders
      WHERE o_orderdate BETWEEN date '${p.from}' AND date '${p.to}' ORDER BY o_orderkey LIMIT 200`,
  },
};

const DATE_RE = /^\d{4}-\d{2}-\d{2}$/;
function paramsOf(query: Record<string, string | undefined>): Params {
  const int = (v: string | undefined, d: number) => (Number.isFinite(Math.trunc(Number(v))) && v !== undefined ? Math.trunc(Number(v)) : d);
  const dt = (v: string | undefined, d: string) => (v && DATE_RE.test(v) ? v : d);
  const lo = Math.max(1, int(query.lo, 1_000_000));
  return {
    lo, hi: Math.max(lo, int(query.hi, lo + 500)), val: int(query.val, 25),
    from: dt(query.from, "1994-01-01"), to: dt(query.to, "1994-03-31"),
  };
}

function capture(cmd: string, args: string[]): Promise<string> {
  return new Promise((resolve, reject) => {
    const p = spawn(cmd, args);
    let out = "", err = "";
    p.stdout.on("data", (d) => (out += d));
    p.stderr.on("data", (d) => (err += d));
    p.on("error", reject);
    p.on("close", (code) => (code === 0 ? resolve(out.trim()) : reject(new Error(err.trim() || `${cmd} exited ${code}`))));
  });
}
const run = (cmd: string, args: string[]) => capture(cmd, args).then(() => undefined);

async function prepClone(): Promise<void> {
  const ids = await capture("docker", ["ps", "-q", "--filter", `publish=${CLONE_PORT}`]).catch(() => "");
  if (ids) await run("docker", ["rm", "-f", ...ids.split("\n")]).catch(() => {});
  await rm(CLONE_DIR, { recursive: true, force: true });
  await mkdir(CLONE_DIR, { recursive: true });
}

async function pinWeights(): Promise<void> {
  const clone = connFor("clone");
  await clone`UPDATE gfs.cost SET net=1, source=20, negligible=100000, horizon=0, partial_max_frac=0.05`;
  await clone.unsafe(
    `UPDATE gfs.cost SET ceiling = GREATEST(
       (SELECT (x.net*s.row_bytes*s.source_rows)::bigint FROM gfs.clone_source s, gfs.cost x
          WHERE s.relid='lineitem'::regclass) / 10, 1000000)`,
  ).catch(() => {});
}

async function setupTemporal(): Promise<void> {
  const clone = connFor("clone");
  const rows = await clone`SELECT source_ref FROM gfs.clone_source WHERE relid='orders'::regclass`.catch(() => []);
  if (!rows[0]) return;
  const sref = String(rows[0].source_ref).replace(/'/g, "''");
  await clone.unsafe(`SELECT gfs.unregister_clone('orders'::regclass)`).catch(() => {});
  await clone.unsafe(`SELECT gfs.register_clone('orders'::regclass, '${sref}', 'o_orderdate')`).catch(() => {});
}

let cloning = false;
app.get("/api/mode", async () => ({ sourceUrl: SOURCE_URL, cloneUrl: CLONE_URL }));
app.get("/api/clone", async () => ({ cloned: cloneReady(), ms: cloneTimeMs(), cloning }));
app.post("/api/clone", async (_req, reply) => {
  if (cloning) return reply.code(409).send({ error: "clone already in progress" });
  cloning = true;
  try {
    await prepClone();
    const from = `postgres://${SOURCE_USER}:${SOURCE_PASS}@${REMOTE_HOST}:${SOURCE_PORT}/${SOURCE_DB}`;
    log(`clone: provisioning from ${REMOTE_HOST}:${SOURCE_PORT}/${SOURCE_DB} on port ${CLONE_PORT}…`);
    const t0 = Date.now();
    await run(GFS_BIN, ["clone", "--from", from, "--image", CLONE_IMAGE, "--database-version", DB_VERSION, "--port", CLONE_PORT, CLONE_DIR]);
    const ms = Date.now() - t0;
    await attachClone(ms);
    await pinWeights();
    await setupTemporal();
    log(`clone: ready in ${ms}ms (weights pinned, not calibrated; orders keyed on o_orderdate for P5)`);
    return { ok: true, ms };
  } catch (e) {
    log(`clone: FAILED — ${String(e)}`);
    throw e;
  } finally {
    cloning = false;
  }
});

type Served = "source" | "fetched" | "partial" | "federated" | "local";
type Sql = ReturnType<typeof connFor>;

async function counters(sql: Sql): Promise<{ fetched: number; federated: number; preds: number }> {
  const r = await sql`SELECT COALESCE(sum(rows_fetched),0)::bigint AS f,
                             COALESCE(sum(federate_calls),0)::bigint AS d,
                             (SELECT count(*) FROM gfs.cached_predicate WHERE complete)::bigint AS p
                        FROM gfs.clones`.catch(() => [{ f: 0, d: 0, p: 0 }] as { f: number | string; d: number | string; p: number | string }[]);
  return { fetched: Number(r[0].f), federated: Number(r[0].d), preds: Number(r[0].p) };
}

async function runQuery(db: DbName, text: string): Promise<{ rows: unknown[]; ms: number; servedFrom: Served; rowCount: number }> {
  const sql = connFor(db);
  const before = db === "clone" ? await counters(sql) : { fetched: 0, federated: 0, preds: 0 };
  const t0 = performance.now();
  const rows = (await sql.unsafe(text)) as unknown[];
  const ms = Number((performance.now() - t0).toFixed(1));
  let servedFrom: Served = "source";
  if (db === "clone") {
    const a = await counters(sql);
    servedFrom =
      a.federated > before.federated ? "federated"
      : a.preds > before.preds ? "partial"
      : a.fetched > before.fetched ? "fetched"
      : "local";
  }
  return { rows: rows.slice(0, 200), ms, servedFrom, rowCount: rows.length };
}

app.get("/api/queries", async () =>
  Object.entries(QUERIES).map(([id, q]) => ({ id, label: q.label, path: q.path, hint: q.hint, parametric: id === "range" || id === "selective" || id === "temporal" })));

app.get("/api/meta", async () => {
  const tbls = await source`
    SELECT relname AS name, reltuples::bigint AS rows
      FROM pg_class WHERE relkind='r' AND relnamespace='public'::regnamespace AND reltuples >= 0
     ORDER BY reltuples DESC`;
  const [{ bytes }] = await source`SELECT pg_database_size(current_database())::bigint AS bytes`;
  const tables = tbls.map((t) => ({ name: t.name as string, rows: Number(t.rows) }));
  const lineitem = tables.find((t) => t.name === "lineitem")?.rows ?? 0;
  return {
    tables,
    sourceRows: tables.reduce((a, t) => a + t.rows, 0),
    sizeBytes: Number(bytes),
    sf: lineitem > 0 ? Math.max(1, Math.round(lineitem / 6_001_215)) : 0,
  };
});

app.get<{ Querystring: { db?: string; id?: string; lo?: string; hi?: string; val?: string; from?: string; to?: string } }>("/api/run", async (req, reply) => {
  const q = QUERIES[req.query.id ?? ""];
  if (!q) return reply.code(400).send({ error: "unknown query id" });
  const out = await runQuery(pickDb(req.query.db), q.sql(paramsOf(req.query)));
  return { ...out, path: q.path };
});

app.get<{ Querystring: { id?: string; lo?: string; hi?: string; val?: string; from?: string; to?: string } }>("/api/explain", async (req, reply) => {
  const q = QUERIES[req.query.id ?? ""];
  if (!q) return reply.code(400).send({ error: "unknown query id" });
  const clone = connFor("clone");
  const rows = (await clone.unsafe(`EXPLAIN (COSTS off) ${q.sql(paramsOf(req.query))}`)) as { "QUERY PLAN": string }[];
  return { plan: rows.map((r) => r["QUERY PLAN"]).join("\n") };
});

app.get("/api/router", async () => {
  const clone = connFor("clone");
  const rows = await clone`
    SELECT clone, chunk_kind, whole_cached, no_partial, partial_rows::bigint AS partial_rows, access_count::bigint AS access_count,
           rows_fetched::bigint AS rows_fetched, federate_calls::bigint AS federate_calls,
           cached_ranges::bigint AS cached_ranges, cached_preds::bigint AS cached_preds
      FROM gfs.clones ORDER BY clone`;
  return rows.map((r) => ({
    table: r.clone, chunkKind: r.chunk_kind, wholeCached: r.whole_cached, noPartial: r.no_partial,
    partialRows: Number(r.partial_rows), access: Number(r.access_count),
    rowsFetched: Number(r.rows_fetched), federateCalls: Number(r.federate_calls),
    cachedRanges: Number(r.cached_ranges), cachedPreds: Number(r.cached_preds),
  }));
});

const COST_COLS = ["net", "source", "negligible", "ceiling", "horizon", "prod_load", "partial_max_frac", "promote_frac", "max_partial_preds"];
app.get("/api/cost", async () => {
  const clone = connFor("clone");
  const [r] = await clone.unsafe(`SELECT ${COST_COLS.join(",")} FROM gfs.cost`);
  return r;
});
app.post<{ Body: Record<string, number> }>("/api/cost", async (req, reply) => {
  const clone = connFor("clone");
  const sets: string[] = [];
  for (const [k, v] of Object.entries(req.body ?? {})) {
    if (COST_COLS.includes(k) && Number.isFinite(Number(v))) sets.push(`${k}=${Number(v)}`);
  }
  if (sets.length === 0) return reply.code(400).send({ error: "no valid cost columns" });
  await clone.unsafe(`UPDATE gfs.cost SET ${sets.join(",")}`);
  log(`cost: set ${sets.join(", ")}`);
  return { ok: true };
});

app.post("/api/reset", async () => {
  const clone = connFor("clone");
  await clone.unsafe(`DO $$
    DECLARE t text;
    BEGIN
      FOR t IN SELECT clone FROM gfs.clones LOOP EXECUTE 'TRUNCATE '||t; END LOOP;
      DELETE FROM gfs.cached; DELETE FROM gfs.cached_predicate;
      UPDATE gfs.clone_source SET whole_cached=false, access_count=0, partial_rows=0, no_partial=false;
      UPDATE gfs.clone_stats SET rows_fetched=0, fetch_calls=0, federate_calls=0;
    END $$;`);
  log("reset: clone hydration state cleared");
  return { ok: true };
});

if (existsSync(webDist)) {
  await app.register(fastifyStatic, { root: webDist });
  app.setNotFoundHandler(async (req, reply) => {
    if (req.method === "GET" && !req.url.startsWith("/api")) return reply.type("text/html").send(await readFile(join(webDist, "index.html")));
    return reply.code(404).send({ error: "not found" });
  });
} else {
  app.get("/", async (_req, reply) =>
    reply.type("text/html").send(`<h1>benchmark-explorer</h1><p>Run <code>pnpm run build:web</code> first (or <code>pnpm demo</code>).</p>`));
}

await app.listen({ port: PORT, host: "0.0.0.0" });
log(`benchmark-explorer listening on http://localhost:${PORT}`);
