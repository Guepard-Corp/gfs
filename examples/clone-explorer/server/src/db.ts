import postgres from "postgres";

type Conn = ReturnType<typeof postgres>;
const opts = { max: 4, onnotice: () => {} };

// The clone relies on constraint_exclusion to prune the foreign scan over a
// cached range. The bootstrap sets it via ALTER DATABASE, but we also pin it on
// every clone connection so elision is deterministic regardless of pool timing.
const cloneOpts = { ...opts, connection: { constraint_exclusion: "on" } };

// The source exists from the start. The clone connection is attached only once
// the user triggers `gfs clone` from the UI (its database does not exist before).
const SOURCE_URL =
  process.env.SOURCE_URL ?? "postgres://app:app@localhost:55442/appdb";
const CLONE_URL =
  process.env.CLONE_URL ?? "postgres://postgres:postgres@localhost:55443/postgres";

export const source: Conn = postgres(SOURCE_URL, opts);

let cloneConn: Conn | null = null;
let cloneMs: number | null = null;

export function cloneReady(): boolean {
  return cloneConn != null;
}
export function cloneTimeMs(): number | null {
  return cloneMs;
}

// Attach (or re-attach) the clone connection after a successful clone, recording
// how long the clone took.
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
