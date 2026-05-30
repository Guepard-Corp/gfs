import postgres from "postgres";

type Conn = ReturnType<typeof postgres>;
const opts = { max: 6, onnotice: () => {} };

// Pin constraint_exclusion on the clone so foreign-scan pruning is deterministic
// regardless of pool timing (the bootstrap also sets it via ALTER DATABASE).
const cloneOpts = { ...opts, connection: { constraint_exclusion: "on" } };

const SOURCE_URL =
  process.env.SOURCE_URL ?? "postgres://app:app@localhost:55452/appdb";
const CLONE_URL =
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
