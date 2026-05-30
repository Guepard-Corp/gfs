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
const PORT = Number(process.env.SERVER_PORT ?? 8788);

const GFS_BIN = process.env.GFS_BIN ?? "gfs";
const CLONE_DIR = process.env.CLONE_DIR ?? join(here, "../../clone-repo");
const CLONE_PORT = process.env.CLONE_PORT ?? "55453";
const REMOTE_HOST = process.env.REMOTE_HOST ?? "host.docker.internal";
const SOURCE_PORT = process.env.SOURCE_PORT ?? "55452";
const SOURCE_DB = process.env.SOURCE_DB ?? "appdb";
const SOURCE_USER = process.env.SOURCE_USER ?? "app";
const SOURCE_PASS = process.env.SOURCE_PASS ?? "app";
const DB_VERSION = process.env.DB_VERSION ?? "16";
const PROXY_MODE = (process.env.PROXY_MODE ?? "") !== "";

const app = Fastify({ logger: false });

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
// Clone control
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
const run = (cmd: string, args: string[]) => capture(cmd, args).then(() => undefined);

async function prepClone(): Promise<void> {
  const ids = await capture("docker", ["ps", "-q", "--filter", `publish=${CLONE_PORT}`]).catch(() => "");
  if (ids) await run("docker", ["rm", "-f", ...ids.split("\n")]).catch(() => {});
  await rm(CLONE_DIR, { recursive: true, force: true });
  await mkdir(CLONE_DIR, { recursive: true });
}

let cloning = false;
app.get("/api/mode", async () => ({ proxy: PROXY_MODE }));
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
// Scenario runner: execute a query, time it, and detect where it was served.
// EXPLAIN (no ANALYZE) is cheap and only used to label remote vs local.
// ---------------------------------------------------------------------------

type Served = "source" | "remote" | "local";

async function scenario(
  db: DbName,
  text: string,
  params: unknown[] = [],
): Promise<{ rows: unknown[]; ms: number; servedFrom: Served }> {
  const sql = connFor(db);
  const t0 = performance.now();
  const rows = (await sql.unsafe(text, params as never[])) as unknown[];
  const ms = Number((performance.now() - t0).toFixed(1));
  let servedFrom: Served = "source";
  if (db === "clone") {
    const plan = await sql.unsafe(`EXPLAIN (FORMAT JSON) ${text}`, params as never[]);
    servedFrom = JSON.stringify(plan).includes("Foreign Scan") ? "remote" : "local";
  }
  return { rows, ms, servedFrom };
}

const q = (req: { query: Record<string, string | undefined> }) => pickDb(req.query.db);
const int = (v: string | undefined, def: number) => {
  const n = Math.trunc(Number(v));
  return Number.isFinite(n) ? n : def;
};

// ---------------------------------------------------------------------------
// Meta + stats
// ---------------------------------------------------------------------------

app.get("/api/meta", async () => {
  const [r] = await source`
    SELECT (SELECT max(id) FROM products)::bigint AS max_product,
           (SELECT max(n) FROM customers)::bigint AS customers,
           (SELECT max(id) FROM orders)::bigint   AS orders`;
  const cats = await source`SELECT name FROM categories ORDER BY id`;
  return { maxProduct: Number(r.max_product), customers: Number(r.customers), orders: Number(r.orders), categories: cats.map((c) => c.name) };
});

app.get<{ Querystring: { db?: string } }>("/api/stats", async (req) => {
  const db = q(req);
  const sql = connFor(db);
  const [{ bytes }] = await sql`SELECT pg_database_size(current_database())::bigint AS bytes`;
  const out: Record<string, unknown> = { db, sizeBytes: Number(bytes) };
  if (db === "clone") {
    // The local store IS the faithful table (public.<t>); schema-qualified so we
    // count locally-owned rows, not the overlay (gfs_ovl__public.<t>) which would
    // federate the full remote set.
    const local = async (t: string) =>
      sql
        .unsafe(`SELECT count(*)::bigint AS n FROM public.${t}`)
        .then((r) => Number((r as unknown as { n: string }[])[0].n))
        .catch(() => null);
    out.localProducts = await local("products");
    out.localOrders = await local("orders");
    out.localReviews = await local("reviews");
    const cr = await sql`SELECT count(*)::bigint AS n FROM gfs_sync.cached_range`.catch(() => [{ n: null }]);
    out.cachedRanges = cr[0].n == null ? null : Number(cr[0].n);
    const fc = await sql`SELECT count(*)::bigint AS n FROM gfs_sync.fully_cached`.catch(() => [{ n: null }]);
    out.fullyCached = fc[0].n == null ? null : Number(fc[0].n);
  }
  return out;
});

app.post<{ Body: { table: string; lo: number; hi: number } }>("/api/warm", async (req) => {
  const { table, lo, hi } = req.body;
  const safe = /^[a-z_]+$/.test(table) ? table : "products";
  const clone = connFor("clone");
  const [{ n }] = await clone`
    SELECT gfs_sync.warm_range('public', ${safe}, ${String(Math.trunc(lo))}, ${String(Math.trunc(hi))}) AS n`;
  // warm_range only hydrates (the AccessExclusive CHECK rebuild is decoupled).
  // In direct mode there is no proxy refresher, so apply the exclusion now —
  // this builds the key-range CHECK and promotes to whole_table if fully covered.
  await clone`SELECT gfs_sync.refresh_exclusions()`;
  log(`warm: ${safe} [${lo},${hi}] → ${Number(n)} rows`);
  return { hydrated: Number(n) };
});

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

// 1. Paginate products by id range (KEY predicate → elided when warmed). Bounds
//    inlined as constants so constraint_exclusion can prune.
app.get<{ Querystring: { db?: string; lo?: string; hi?: string } }>("/api/products", async (req) => {
  const lo = Math.max(1, int(req.query.lo, 1));
  const hi = Math.max(lo, int(req.query.hi, lo + 49));
  return scenario(q(req),
    `SELECT id, name, category_id, price_cents, in_stock FROM products
     WHERE id BETWEEN ${lo} AND ${hi} ORDER BY id`);
});

// 2. Fuzzy product search (NON-KEY → federates until the table is whole-cached).
app.get<{ Querystring: { db?: string; term?: string } }>("/api/search", async (req) => {
  const term = (req.query.term ?? "").slice(0, 64);
  return scenario(q(req),
    `SELECT id, name, category_id, price_cents FROM products WHERE name ILIKE $1 ORDER BY id LIMIT 50`,
    [`%${term}%`]);
});

// 3. Fuzzy review search (text body, non-key).
app.get<{ Querystring: { db?: string; term?: string } }>("/api/reviews", async (req) => {
  const term = (req.query.term ?? "").slice(0, 64);
  return scenario(q(req),
    `SELECT id, product_id, rating, body FROM reviews WHERE body ILIKE $1 ORDER BY id LIMIT 50`,
    [`%${term}%`]);
});

// 4. Filter by category — JOIN products ⋈ categories (two overlays).
app.get<{ Querystring: { db?: string; cat?: string } }>("/api/category", async (req) => {
  const cat = (req.query.cat ?? "games").slice(0, 32);
  return scenario(q(req),
    `SELECT p.id, p.name, p.price_cents FROM products p
       JOIN categories c ON c.id = p.category_id
      WHERE c.name = $1 ORDER BY p.id LIMIT 50`,
    [cat]);
});

// 5. Customer order history — 3-table JOIN (orders ⋈ order_items ⋈ products).
app.get<{ Querystring: { db?: string; n?: string } }>("/api/customer-orders", async (req) => {
  const n = Math.max(1, int(req.query.n, 1));
  return scenario(q(req),
    `SELECT o.id AS order_id, o.placed_at, oi.line, p.name, oi.qty, oi.total_cents
       FROM customers cu
       JOIN orders o       ON o.customer_id = cu.id
       JOIN order_items oi ON oi.order_id = o.id
       JOIN products p     ON p.id = oi.product_id
      WHERE cu.n = $1 ORDER BY o.id, oi.line LIMIT 100`,
    [n]);
});

// 6. Recent orders — temporal (non-key range) filter.
app.get<{ Querystring: { db?: string; days?: string } }>("/api/recent-orders", async (req) => {
  const days = Math.max(1, int(req.query.days, 7));
  return scenario(q(req),
    `SELECT id, customer_id, placed_at, status FROM orders
      WHERE placed_at > now() - ($1 || ' days')::interval ORDER BY placed_at DESC LIMIT 50`,
    [String(days)]);
});

// 7. Dashboard — aggregate JOIN (revenue per category). Anti-join blocks
//    push-down, so on the clone this federates full scans.
app.get<{ Querystring: { db?: string } }>("/api/dashboard", async (req) => {
  return scenario(q(req),
    `SELECT c.name AS category, count(*)::bigint AS items, sum(oi.total_cents)::bigint AS revenue_cents
       FROM order_items oi
       JOIN products p   ON p.id = oi.product_id
       JOIN categories c ON c.id = p.category_id
      GROUP BY c.name ORDER BY revenue_cents DESC`);
});

// ---------------------------------------------------------------------------
// Writes (copy-on-write divergence on the clone)
// ---------------------------------------------------------------------------

const randId = () => 9_000_000_000 + Math.floor(Math.random() * 1_000_000_000);

app.post<{ Querystring: { db?: string }; Body: { customerN: number; productId: number; qty: number } }>(
  "/api/order",
  async (req, reply) => {
    const db = q(req);
    const sql = connFor(db);
    const { customerN, productId, qty } = req.body;
    const cu = await sql`SELECT id FROM customers WHERE n = ${customerN}`;
    if (cu.length === 0) return reply.code(404).send({ error: "no such customer" });
    const prod = await sql`SELECT price_cents FROM products WHERE id = ${productId}`;
    if (prod.length === 0) return reply.code(404).send({ error: "no such product" });
    const oid = randId();
    await sql`INSERT INTO orders (id, customer_id, placed_at, status) VALUES (${oid}, ${cu[0].id}, now(), 'paid')`;
    await sql`INSERT INTO order_items (order_id, line, product_id, qty, unit_cents)
              VALUES (${oid}, 1, ${productId}, ${qty}, ${prod[0].price_cents})`;
    const [item] = await sql`SELECT total_cents FROM order_items WHERE order_id = ${oid} AND line = 1`;
    log(`order: db=${db} #${oid} product=${productId} qty=${qty} total=${item.total_cents}`);
    return { orderId: oid, totalCents: Number(item.total_cents) };
  },
);

app.post<{ Querystring: { db?: string }; Body: { productId: number; customerN: number; rating: number; body: string } }>(
  "/api/review",
  async (req, reply) => {
    const db = q(req);
    const sql = connFor(db);
    const { productId, customerN, rating, body } = req.body;
    const cu = await sql`SELECT id FROM customers WHERE n = ${customerN}`;
    if (cu.length === 0) return reply.code(404).send({ error: "no such customer" });
    const rid = randId();
    await sql`INSERT INTO reviews (id, product_id, customer_id, rating, body)
              VALUES (${rid}, ${productId}, ${cu[0].id}, ${rating}, ${body.slice(0, 200)})`;
    log(`review: db=${db} #${rid} product=${productId} rating=${rating}`);
    return { reviewId: rid };
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
    reply.type("text/html").send(`<h1>clone-explorer-pro</h1><p>Run <code>pnpm run build:web</code> first (or <code>pnpm demo</code>).</p>`),
  );
}

await app.listen({ port: PORT, host: "0.0.0.0" });
log(`clone-explorer-pro listening on http://localhost:${PORT}${PROXY_MODE ? " (proxy mode)" : ""}`);
