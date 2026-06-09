import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";

import Fastify from "fastify";
import fastifyStatic from "@fastify/static";

import { source, connFor, pickDb, SOURCE_URL, CLONE_URL } from "./db.js";
import { cloneReady, cloneTimeMs } from "./db.js";
import { QUERIES, PARAMETRIC, paramsOf, runQuery, provisionClone } from "./bench.js";

const here = dirname(fileURLToPath(import.meta.url));
const webDist = join(here, "../../web/dist");
const PORT = Number(process.env.SERVER_PORT ?? 8789);

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

let cloning = false;
app.get("/api/mode", async () => ({ sourceUrl: SOURCE_URL, cloneUrl: CLONE_URL }));
app.get("/api/clone", async () => ({ cloned: cloneReady(), ms: cloneTimeMs(), cloning }));
app.post("/api/clone", async (_req, reply) => {
  if (cloning) return reply.code(409).send({ error: "clone already in progress" });
  cloning = true;
  try {
    const ms = await provisionClone(log);
    return { ok: true, ms };
  } catch (e) {
    log(`clone: FAILED — ${String(e)}`);
    throw e;
  } finally {
    cloning = false;
  }
});

app.get("/api/queries", async () =>
  Object.entries(QUERIES).map(([id, q]) => ({ id, label: q.label, path: q.path, hint: q.hint, parametric: PARAMETRIC.has(id) })));

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
      DELETE FROM gfs.cached; DELETE FROM gfs.cached_predicate; DELETE FROM gfs.tombstone;
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
