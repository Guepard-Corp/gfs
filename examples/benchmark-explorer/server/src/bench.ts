// The benchmark — defined ONCE, driven two ways: the HTTP server (UI, index.ts,
// via scripts/run.sh) and the headless runner (CLI, headless.ts, via
// scripts/bench.sh) both import this module, so they exercise the exact same
// scenarios, clone provisioning, and route detection. Nothing benchmark-specific
// lives in the front-ends.

import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { rm, mkdir } from "node:fs/promises";
import { spawn } from "node:child_process";

import { connFor, attachClone, type DbName } from "./db.js";

const here = dirname(fileURLToPath(import.meta.url));

// --- clone provisioning config (env-overridable; defaults match scripts/run.sh) ---
export const cfg = {
  GFS_BIN: process.env.GFS_BIN ?? "gfs",
  CLONE_DIR: process.env.CLONE_DIR ?? join(here, "../../clone-repo"),
  CLONE_PORT: process.env.CLONE_PORT ?? "55621",
  REMOTE_HOST: process.env.REMOTE_HOST ?? "host.docker.internal",
  SOURCE_PORT: process.env.SOURCE_PORT ?? "55620",
  SOURCE_DB: process.env.SOURCE_DB ?? "tpch",
  SOURCE_USER: process.env.SOURCE_USER ?? "app",
  SOURCE_PASS: process.env.SOURCE_PASS ?? "pw",
  DB_VERSION: process.env.DB_VERSION ?? "16",
  CLONE_IMAGE: process.env.CLONE_IMAGE ?? "gfs-postgres:16",
};

// =====================================================================
// Scenarios — the single source of truth for both the UI and headless.
// =====================================================================
export type Params = { lo: number; hi: number; val: number; from: string; to: string };
export type Q = { label: string; path: "P1" | "P2" | "P3" | "P5"; hint: string; sql: (p: Params) => string };

export const QUERIES: Record<string, Q> = {
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
  join_local: {
    label: "Join → local (convergence)", path: "P3",
    hint: "orders ⋈ customer ⋈ nation on a bounded o_orderkey range. Cold → federates; after warming the small dimensions (customer/nation) it serves entirely local (zero Foreign Scan) — the lazy clone converging to a self-sufficient copy (the transitive-FK-warming case, planner-hook model).",
    sql: (p) => `SELECT o_orderkey,c_name,n_name FROM orders,customer,nation
      WHERE c_custkey=o_custkey AND c_nationkey=n_nationkey
        AND o_orderkey BETWEEN ${p.lo} AND ${p.hi} ORDER BY o_orderkey LIMIT 200`,
  },
};

export const PARAMETRIC = new Set(["range", "selective", "temporal", "join_local"]);

const DATE_RE = /^\d{4}-\d{2}-\d{2}$/;
export function paramsOf(query: Record<string, string | undefined>): Params {
  const int = (v: string | undefined, d: number) => (Number.isFinite(Math.trunc(Number(v))) && v !== undefined ? Math.trunc(Number(v)) : d);
  const dt = (v: string | undefined, d: string) => (v && DATE_RE.test(v) ? v : d);
  const lo = Math.max(1, int(query.lo, 1_000_000));
  return {
    lo, hi: Math.max(lo, int(query.hi, lo + 500)), val: int(query.val, 25),
    from: dt(query.from, "1994-01-01"), to: dt(query.to, "1994-03-31"),
  };
}

// =====================================================================
// Process helpers + clone provisioning (shared by /api/clone and headless).
// =====================================================================
export function capture(cmd: string, args: string[]): Promise<string> {
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
  const ids = await capture("docker", ["ps", "-q", "--filter", `publish=${cfg.CLONE_PORT}`]).catch(() => "");
  if (ids) await run("docker", ["rm", "-f", ...ids.split("\n")]).catch(() => {});
  await rm(cfg.CLONE_DIR, { recursive: true, force: true });
  await mkdir(cfg.CLONE_DIR, { recursive: true });
}

// Pin fixed cost weights (do NOT calibrate — that rewrites `net` to measured
// seconds/byte and collapses the P2 partial niche) and size the whole-own ceiling
// to 1/10 of lineitem's whole-own cost so the big facts stay not-ownable.
export async function pinWeights(): Promise<void> {
  const clone = connFor("clone");
  await clone`UPDATE gfs.cost SET net=1, source=20, negligible=100000, horizon=0, partial_max_frac=0.05`;
  await clone.unsafe(
    `UPDATE gfs.cost SET ceiling = GREATEST(
       (SELECT (x.net*s.row_bytes*s.source_rows)::bigint FROM gfs.clone_source s, gfs.cost x
          WHERE s.relid='lineitem'::regclass) / 10, 1000000)`,
  ).catch(() => {});
}

// Re-key `orders` on its DATE column so the temporal window (P5) exercises the
// time-range hydrate path.
export async function setupTemporal(): Promise<void> {
  const clone = connFor("clone");
  const rows = await clone`SELECT source_ref FROM gfs.clone_source WHERE relid='orders'::regclass`.catch(() => []);
  if (!rows[0]) return;
  const sref = String(rows[0].source_ref).replace(/'/g, "''");
  await clone.unsafe(`SELECT gfs.unregister_clone('orders'::regclass)`).catch(() => {});
  await clone.unsafe(`SELECT gfs.register_clone('orders'::regclass, '${sref}', 'o_orderdate')`).catch(() => {});
}

// Provision a fresh lazy clone of the source and make it the active clone
// connection. Returns the provisioning time (ms). Shared verbatim by the UI's
// POST /api/clone and the headless runner.
export async function provisionClone(logFn: (m: string) => void = () => {}): Promise<number> {
  await prepClone();
  const from = `postgres://${cfg.SOURCE_USER}:${cfg.SOURCE_PASS}@${cfg.REMOTE_HOST}:${cfg.SOURCE_PORT}/${cfg.SOURCE_DB}`;
  logFn(`clone: provisioning from ${cfg.REMOTE_HOST}:${cfg.SOURCE_PORT}/${cfg.SOURCE_DB} on port ${cfg.CLONE_PORT}…`);
  const t0 = Date.now();
  await run(cfg.GFS_BIN, ["clone", "--from", from, "--image", cfg.CLONE_IMAGE, "--database-version", cfg.DB_VERSION, "--port", cfg.CLONE_PORT, cfg.CLONE_DIR]);
  const ms = Date.now() - t0;
  await attachClone(ms);
  await pinWeights();
  await setupTemporal();
  logFn(`clone: ready in ${ms}ms (weights pinned, not calibrated; orders keyed on o_orderdate for P5)`);
  return ms;
}

// =====================================================================
// Query execution + route detection (the "served from" badge).
// =====================================================================
export type Served = "source" | "fetched" | "partial" | "federated" | "local";
type Sql = ReturnType<typeof connFor>;

export async function counters(sql: Sql): Promise<{ fetched: number; federated: number; preds: number }> {
  const r = await sql`SELECT COALESCE(sum(rows_fetched),0)::bigint AS f,
                             COALESCE(sum(federate_calls),0)::bigint AS d,
                             (SELECT count(*) FROM gfs.cached_predicate WHERE complete)::bigint AS p
                        FROM gfs.clones`.catch(() => [{ f: 0, d: 0, p: 0 }] as { f: number | string; d: number | string; p: number | string }[]);
  return { fetched: Number(r[0].f), federated: Number(r[0].d), preds: Number(r[0].p) };
}

export async function runQuery(db: DbName, text: string): Promise<{ rows: unknown[]; ms: number; servedFrom: Served; rowCount: number }> {
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
