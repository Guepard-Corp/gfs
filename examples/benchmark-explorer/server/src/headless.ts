// Headless front-end of the SAME benchmark the UI runs (server/src/bench.ts):
// provisions a real lazy clone, replays the router scenarios on the clone AND the
// source, asserts clone == source + the expected route per shot, proves
// convergence (a federated join becomes fully local after warming), then writes
// objective metrics to benchmark.results.tsv with a regression compare. Exit 0 on
// all-pass, 1 otherwise. Driven by scripts/bench.sh; the UI (index.ts) is the
// other front-end of the very same definitions.

import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { readFile, writeFile, appendFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { createHash } from "node:crypto";

import { connFor } from "./db.js";
import { QUERIES, paramsOf, runQuery, provisionClone, type Params, type Served } from "./bench.js";

const here = dirname(fileURLToPath(import.meta.url));
const RESULTS = process.env.RESULTS_FILE ?? join(here, "../../benchmark.results.tsv");
const LABEL = process.env.LABEL ?? "headless";

const C = { red: "\x1b[31m", grn: "\x1b[32m", dim: "\x1b[2m", cyan: "\x1b[1;36m", rst: "\x1b[0m" };
const note = (s: string) => console.log(`\n${C.cyan}== ${s} ==${C.rst}`);
let pass = 0, fail = 0;
const ok = (m: string) => { pass++; console.log(`  ${C.grn}PASS${C.rst} ${m}`); };
const bad = (m: string) => { fail++; console.log(`  ${C.red}FAIL${C.rst} ${m}`); };

const md5 = (rows: unknown[]) => createHash("md5").update(JSON.stringify(rows)).digest("hex");

// One scenario shot: run on clone + source, assert identical, assert the route.
type Shot = { id: string; p?: Partial<Params>; expect: Served; why: string };

async function shot(s: Shot): Promise<void> {
  const q = QUERIES[s.id];
  const sql = q.sql(paramsOf((s.p ?? {}) as Record<string, string | undefined>));
  const cl = await runQuery("clone", sql);
  const sr = await runQuery("source", sql);
  const same = md5(cl.rows) === md5(sr.rows);
  const route = cl.servedFrom;
  const label = `${q.path} ${s.id} — ${s.why}`;
  if (same && route === s.expect) ok(`${label}  ${C.dim}[${route}, ${cl.ms}ms, ${cl.rowCount} rows]${C.rst}`);
  else if (!same) bad(`${label}  ${C.red}clone != source${C.rst} (clone ${cl.rowCount} rows vs source ${sr.rowCount})`);
  else bad(`${label}  route ${C.red}${route}${C.rst}, expected ${s.expect}`);
}

// The router-path script — the same paths the UI walks, in order.
const SCRIPT: Shot[] = [
  { id: "range",     p: { lo: 1_000_000, hi: 1_000_500 }, expect: "fetched",   why: "first touch fetches the missing key range" },
  { id: "range",     p: { lo: 1_000_000, hi: 1_000_500 }, expect: "local",     why: "re-ask covered range → elision (no source)" },
  { id: "selective", p: { val: 25 },                      expect: "federated", why: "first touch federates (second-chance)" },
  { id: "selective", p: { val: 25 },                      expect: "partial",   why: "second touch fetches the selective slice (committed predicate → partial)" },
  { id: "selective", p: { val: 25 },                      expect: "local",     why: "third touch serves local" },
  { id: "q1",                                             expect: "federated", why: "single-table aggregate pushed to source" },
  { id: "q3",                                             expect: "federated", why: "3-table join pushed to source" },
  { id: "q5",                                             expect: "federated", why: "6-table join pushed to source" },
  { id: "temporal",  p: { from: "1994-01-01", to: "1994-03-31" }, expect: "fetched", why: "temporal window fetched (DATE key)" },
  { id: "temporal",  p: { from: "1994-02-01", to: "1994-02-15" }, expect: "local",   why: "narrower window inside it → elision" },
];

// Convergence (the transitive-FK-warming case, planner-hook model): a join
// federates cold; after warming its tables the SAME join is served entirely local
// — zero Foreign Scan — and still equals the source.
async function convergence(): Promise<void> {
  note("Convergence — a federated join becomes fully local after warming");
  const clone = connFor("clone");
  const p = { lo: "1", hi: "500" } as Record<string, string | undefined>;
  const sql = QUERIES.join_local.sql(paramsOf(p));

  const cold = await runQuery("clone", sql);
  cold.servedFrom === "federated"
    ? ok(`cold join federates  ${C.dim}[${cold.servedFrom}, ${cold.ms}ms]${C.rst}`)
    : bad(`expected the cold join to federate, got ${cold.servedFrom}`);

  for (const t of ["customer", "nation", "orders"]) {
    await clone.unsafe(`SELECT gfs.warm('${t}'::regclass)`).catch(() => {});
  }

  const warm = await runQuery("clone", sql);
  const src = await runQuery("source", sql);
  const fs = (await clone.unsafe(`EXPLAIN (VERBOSE, COSTS off) ${sql}`) as { "QUERY PLAN": string }[])
    .filter((r) => r["QUERY PLAN"].includes("Foreign Scan")).length;

  warm.servedFrom === "local" && fs === 0
    ? ok(`warmed join is fully local  ${C.dim}[${warm.servedFrom}, ${fs} Foreign Scan, ${warm.ms}ms]${C.rst}`)
    : bad(`warmed join still federates (route ${warm.servedFrom}, ${fs} Foreign Scan)`);
  md5(warm.rows) === md5(src.rows) ? ok("warmed join result equals source") : bad("warmed join diverged from source");
}

// Objective metrics + append-only regression guard.
async function record(localShots: number, total: number): Promise<void> {
  const clone = connFor("clone");
  const [m] = await clone.unsafe(
    `SELECT COALESCE(sum(fetch_calls),0)::bigint + COALESCE(sum(federate_calls),0)::bigint AS source_ops,
            COALESCE(sum(rows_fetched),0)::bigint AS rows_pulled FROM gfs.clones`,
  ) as { source_ops: number | string; rows_pulled: number | string }[];
  const sourceOps = Number(m.source_ops), rowsPulled = Number(m.rows_pulled);
  const localPct = Math.round((100 * localShots) / Math.max(total, 1));
  const header = "# label\tscenarios\tpassed\tsource_ops\trows_pulled\tlocal_pct  (objective: source_ops DOWN, rows_pulled low, local_pct UP)";

  let prev: string[] | null = null;
  if (existsSync(RESULTS)) {
    const lines = (await readFile(RESULTS, "utf8")).trim().split("\n").filter((l) => l && !l.startsWith("#"));
    if (lines.length) prev = lines[lines.length - 1].split("\t");
  } else {
    await writeFile(RESULTS, header + "\n");
  }
  await appendFile(RESULTS, `${LABEL}\t${total}\t${pass}\t${sourceOps}\t${rowsPulled}\t${localPct}\n`);

  note("Objective metrics (vs previous run)");
  console.log(`  source_ops=${sourceOps}  rows_pulled=${rowsPulled}  local_pct=${localPct}%  scenarios=${total}`);
  if (prev) {
    const pOps = Number(prev[3]), pLocal = Number(prev[5]);
    const verdict = sourceOps < pOps || localPct > pLocal ? `${C.grn}IMPROVED${C.rst}`
      : sourceOps > pOps || localPct < pLocal ? `${C.red}REGRESSED${C.rst}` : "same";
    console.log(`  vs ${prev[0]}: source_ops ${pOps}→${sourceOps}, local_pct ${pLocal}→${localPct}  ${verdict}`);
  }
}

async function main(): Promise<void> {
  note("Provision a fresh lazy clone (copy-on-read)");
  const ms = await provisionClone((m) => console.log(`  ${C.dim}${m}${C.rst}`));
  ok(`clone provisioned in ${ms}ms`);

  note("Router paths — clone == source + the expected route, per shot");
  let localShots = 0;
  for (const s of SCRIPT) {
    await shot(s);
    if (s.expect === "local") localShots++;
  }

  await convergence();
  await record(localShots, SCRIPT.length);

  note(`Result: ${pass} passed, ${fail} failed`);
  await connFor("source").end({ timeout: 5 }).catch(() => {});
  process.exit(fail === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(`\n${C.red}headless benchmark crashed:${C.rst} ${String(e)}`);
  process.exit(1);
});
