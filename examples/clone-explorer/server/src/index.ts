import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { existsSync } from "node:fs";
import { readFile, rm, mkdir } from "node:fs/promises";
import { spawn } from "node:child_process";

import Fastify from "fastify";
import fastifyStatic from "@fastify/static";

import { source, connFor, pickDb, attachClone, cloneReady, cloneTimeMs, type DbName } from "./db.js";

const here = dirname(fileURLToPath(import.meta.url));
const webDist = join(here, "../../web/dist");
const PORT = Number(process.env.SERVER_PORT ?? 8787);

// Clone configuration: how the server runs `gfs clone` on demand.
const GFS_BIN = process.env.GFS_BIN ?? "gfs";
const CLONE_DIR = process.env.CLONE_DIR ?? join(here, "../../clone-repo");
const CLONE_PORT = process.env.CLONE_PORT ?? "55443";
const REMOTE_HOST = process.env.REMOTE_HOST ?? "host.docker.internal";
const SOURCE_PORT = process.env.SOURCE_PORT ?? "55442";
const SOURCE_DB = process.env.SOURCE_DB ?? "appdb";
const SOURCE_USER = process.env.SOURCE_USER ?? "app";
const SOURCE_PASS = process.env.SOURCE_PASS ?? "app";
const DB_VERSION = process.env.DB_VERSION ?? "16";

// Proxy mode: the clone is reached through the guepard proxy (CLONE_URL points at
// it), which auto-warms reads. The UI adapts (hides the manual "warm" button).
const PROXY_MODE = (process.env.PROXY_MODE ?? "") !== "";

const app = Fastify({ logger: false });

// Plain, readable console logging (one line per API request + domain events).
function log(msg: string): void {
  console.log(`${new Date().toISOString().slice(11, 19)} ${msg}`);
}

app.addHook("onResponse", async (req, reply) => {
  if (req.url.startsWith("/api")) {
    log(`${req.method} ${req.url} → ${reply.statusCode} ${reply.elapsedTime.toFixed(0)}ms`);
  }
});

app.setErrorHandler((err: Error & { statusCode?: number }, req, reply) => {
  const code = err.statusCode ?? 500;
  if (code >= 500) log(`ERROR ${req.method} ${req.url}: ${err.message}`);
  reply.code(code).send({ error: err.message });
});

// ---------------------------------------------------------------------------
// Subprocess helpers
// ---------------------------------------------------------------------------

function capture(cmd: string, args: string[]): Promise<string> {
  return new Promise((resolve, reject) => {
    const p = spawn(cmd, args);
    let out = "";
    let err = "";
    p.stdout.on("data", (d) => (out += d));
    p.stderr.on("data", (d) => (err += d));
    p.on("error", reject);
    p.on("close", (code) => (code === 0 ? resolve(out.trim()) : reject(new Error(err.trim() || `${cmd} exited ${code}`))));
  });
}

function run(cmd: string, args: string[]): Promise<void> {
  return capture(cmd, args).then(() => undefined);
}

// Free the clone port and wipe the repo dir so a clone always starts fresh.
async function prepClone(): Promise<void> {
  const ids = await capture("docker", ["ps", "-q", "--filter", `publish=${CLONE_PORT}`]).catch(() => "");
  if (ids) await run("docker", ["rm", "-f", ...ids.split("\n")]).catch(() => {});
  await rm(CLONE_DIR, { recursive: true, force: true });
  await mkdir(CLONE_DIR, { recursive: true });
}

// ---------------------------------------------------------------------------
// Clone control (driven from the UI)
// ---------------------------------------------------------------------------

let cloning = false;

app.get("/api/clone", async () => ({ cloned: cloneReady(), ms: cloneTimeMs(), cloning }));

app.post("/api/clone", async (_req, reply) => {
  if (cloning) return reply.code(409).send({ error: "clone already in progress" });
  cloning = true;
  try {
    await prepClone();
    const from = `postgres://${SOURCE_USER}:${SOURCE_PASS}@${REMOTE_HOST}:${SOURCE_PORT}/${SOURCE_DB}`;
    log(`clone: provisioning from ${REMOTE_HOST}:${SOURCE_PORT}/${SOURCE_DB} on port ${CLONE_PORT}…`);
    const t0 = Date.now();
    await run(GFS_BIN, ["clone", "--from", from, "--database-version", DB_VERSION, "--port", CLONE_PORT, CLONE_DIR]);
    const ms = Date.now() - t0;
    await attachClone(ms);
    log(`clone: ready in ${ms}ms`);
    return { ok: true, ms };
  } catch (e) {
    log(`clone: FAILED — ${String(e)}`);
    throw e;
  } finally {
    cloning = false;
  }
});

// ---------------------------------------------------------------------------
// Where-served detection (clone only)
// ---------------------------------------------------------------------------

// The SAME EXPLAIN runs for both databases so the per-request work (and thus the
// client round-trip "load") is symmetric; only the interpretation differs. Range
// bounds are inlined as constants (not bind params): constraint_exclusion can
// only refute the foreign table's CHECK against constant predicates, so a
// generic/parameterised plan would never be pruned.
async function servedFrom(
  db: DbName,
  q: string,
  lo: number,
  hi: number,
): Promise<"source" | "remote" | "local"> {
  const sql = connFor(db);
  const plan = q
    ? await sql`EXPLAIN (FORMAT JSON) SELECT id FROM products WHERE name ILIKE ${"%" + q + "%"} ORDER BY id LIMIT 50`
    : await sql.unsafe(`EXPLAIN (FORMAT JSON) SELECT id FROM products WHERE id BETWEEN ${lo} AND ${hi} ORDER BY id`);
  if (db === "source") return "source";
  return JSON.stringify(plan).includes("Foreign Scan") ? "remote" : "local";
}

// ---------------------------------------------------------------------------
// Data API
// ---------------------------------------------------------------------------

app.get("/api/mode", async () => ({ proxy: PROXY_MODE }));

app.get("/api/meta", async () => {
  const [{ max }] = await source`SELECT coalesce(max(id), 0)::bigint AS max FROM products`;
  return { maxId: Number(max) };
});

// Cheap, frequently-polled stats. Deliberately excludes count(*) over products:
// on the clone that count federates a full scan of the remote (the overlay's
// NOT EXISTS anti-join blocks aggregate push-down), which costs seconds. The
// product total is fetched once via /api/products-count instead.
app.get<{ Querystring: { db?: string } }>("/api/stats", async (req) => {
  const db = pickDb(req.query.db);
  const sql = connFor(db);
  const [{ orders }] = await sql`SELECT count(*)::bigint AS orders FROM orders`;
  // Whole-database on-disk size: the source carries all the data, the clone only
  // what it owns locally (warmed or written) — the rest stays on the remote.
  const [{ bytes }] = await sql`SELECT pg_database_size(current_database())::bigint AS bytes`;

  let localProducts: number | null = null;
  let cachedRanges: number | null = null;
  if (db === "clone") {
    // Local store = the faithful table (public.products); schema-qualified to
    // count locally-owned rows, not the federating overlay (gfs_ovl__public.products).
    const lp = await sql`SELECT count(*)::bigint AS n FROM public.products`.catch(() => [{ n: null }]);
    localProducts = lp[0].n == null ? null : Number(lp[0].n);
    const cr = await sql`SELECT count(*)::bigint AS n FROM gfs_sync.cached_range WHERE table_name = 'products'`.catch(
      () => [{ n: null }],
    );
    cachedRanges = cr[0].n == null ? null : Number(cr[0].n);
  }
  return { db, orders: Number(orders), sizeBytes: Number(bytes), localProducts, cachedRanges };
});

// Logical row total. On the clone this federates to the remote (see /api/stats),
// so it is fetched on demand and cached, not polled.
app.get<{ Querystring: { db?: string } }>("/api/products-count", async (req) => {
  const db = pickDb(req.query.db);
  const [{ n }] = await connFor(db)`SELECT count(*)::bigint AS n FROM products`;
  return { count: Number(n) };
});

app.get<{ Querystring: { db?: string; page?: string; size?: string; q?: string } }>(
  "/api/products",
  async (req) => {
    const db = pickDb(req.query.db);
    const sql = connFor(db);
    const size = Math.min(Math.max(Number(req.query.size ?? 50), 1), 200);
    const page = Math.max(Number(req.query.page ?? 0), 0);
    const q = (req.query.q ?? "").trim();
    const lo = Math.trunc(page * size + 1);
    const hi = Math.trunc(lo + size - 1);

    const t0 = performance.now();
    // Constant range bounds so a warmed page is actually elided (see servedFrom).
    const rows = q
      ? await sql`SELECT id, name, category, price_cents FROM products WHERE name ILIKE ${"%" + q + "%"} ORDER BY id LIMIT ${size}`
      : await sql.unsafe(`SELECT id, name, category, price_cents FROM products WHERE id BETWEEN ${lo} AND ${hi} ORDER BY id`);
    const ms = Number((performance.now() - t0).toFixed(1));

    return { rows, page, size, lo, hi, q, ms, servedFrom: await servedFrom(db, q, lo, hi) };
  },
);

app.post<{ Body: { lo: number; hi: number } }>("/api/warm", async (req) => {
  const { lo, hi } = req.body;
  const clone = connFor("clone");
  const [{ n }] = await clone`SELECT gfs_sync.warm_range('public', 'products', ${String(lo)}, ${String(hi)}) AS n`;
  // warm_range only hydrates rows; the AccessExclusive CHECK rebuild is decoupled.
  // In direct mode there is no proxy refresher, so apply the exclusion now so the
  // warmed range is actually elided (builds the key-range CHECK / promotes to whole_table).
  await clone`SELECT gfs_sync.refresh_exclusions()`;
  log(`warm: products [${lo},${hi}] → hydrated ${Number(n)} rows`);
  return { hydrated: Number(n) };
});

app.post<{ Querystring: { db?: string }; Body: { id: number; priceCents: number } }>(
  "/api/price",
  async (req, reply) => {
    const db = pickDb(req.query.db);
    const { id, priceCents } = req.body;
    const res = await connFor(db)`UPDATE products SET price_cents = ${priceCents} WHERE id = ${id}`;
    if (res.count === 0) return reply.code(404).send({ error: "no such product" });
    log(`price: db=${db} product=${id} → ${priceCents} cents`);
    return { ok: true };
  },
);

app.post<{ Querystring: { db?: string }; Body: { productId: number; qty: number } }>(
  "/api/orders",
  async (req, reply) => {
    const db = pickDb(req.query.db);
    const sql = connFor(db);
    const { productId, qty } = req.body;
    const prod = await sql`SELECT price_cents FROM products WHERE id = ${productId}`;
    if (prod.length === 0) return reply.code(404).send({ error: "no such product" });
    // RETURNING on the clone's overlay view yields NEW (no generated value), so
    // re-read the row: the store recomputed total_cents (STORED generated column).
    const [{ id }] = await sql`
      INSERT INTO orders (product_id, qty, unit_cents, origin)
      VALUES (${productId}, ${qty}, ${prod[0].price_cents}, ${db})
      RETURNING id`;
    const [order] = await sql`
      SELECT id, product_id, qty, unit_cents, total_cents FROM orders WHERE id = ${id}`;
    log(`order: db=${db} product=${productId} qty=${qty} → #${order.id} total=${order.total_cents}`);
    return { order };
  },
);

// ---------------------------------------------------------------------------
// Static SPA
// ---------------------------------------------------------------------------

if (existsSync(webDist)) {
  await app.register(fastifyStatic, { root: webDist });
  app.setNotFoundHandler(async (req, reply) => {
    if (req.method === "GET" && !req.url.startsWith("/api")) {
      return reply.type("text/html").send(await readFile(join(webDist, "index.html")));
    }
    return reply.code(404).send({ error: "not found" });
  });
} else {
  app.get("/", async (_req, reply) =>
    reply.type("text/html").send(`<h1>clone-explorer</h1><p>Run <code>npm run build:web</code> first (or use <code>npm run demo</code>).</p>`),
  );
}

await app.listen({ port: PORT, host: "0.0.0.0" });
log(`clone-explorer listening on http://localhost:${PORT}`);
