import postgres from "postgres";

type Conn = ReturnType<typeof postgres>;
const opts = { max: 6, onnotice: () => {} };

// The gfs copy-on-read is a planner hook: before a query runs it reads the query's
// per-table predicate, fetches ONLY the matching rows from the source into the real
// local table, then the normal plan executes. So the clone uses its indexes
// normally — no need to force seq scans. The laziness is row-granular: a selective
// query fetches only its rows (an unrestricted scan, e.g. a full aggregate, fetches
// the whole table — correct, since it genuinely needs every row).
const cloneOpts = { ...opts };

export const SOURCE_URL =
  process.env.SOURCE_URL ?? "postgres://app:app@localhost:55452/appdb";
export const CLONE_URL =
  process.env.CLONE_URL ?? "postgres://postgres:postgres@localhost:55453/postgres";

export const source: Conn = postgres(SOURCE_URL, opts);

let cloneConn: Conn | null = null;
let cloneMs: number | null = null;

export function cloneReady(): boolean {
  return cloneConn != null;
}
export function cloneTimeMs(): number | null {
  return cloneMs;
}

export async function attachClone(ms: number): Promise<void> {
  await cloneConn?.end({ timeout: 5 }).catch(() => {});
  cloneConn = postgres(CLONE_URL, cloneOpts);
  cloneMs = ms;
}

export type DbName = "source" | "clone";

export function pickDb(v: unknown): DbName {
  return v === "clone" ? "clone" : "source";
}

export function connFor(db: DbName): Conn {
  if (db === "source") return source;
  if (!cloneConn) throw Object.assign(new Error("clone not ready"), { statusCode: 409 });
  return cloneConn;
}
