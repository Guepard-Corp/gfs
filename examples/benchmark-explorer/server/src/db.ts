import postgres from "postgres";

type Conn = ReturnType<typeof postgres>;
const opts = { max: 6, onnotice: () => {} };

const cloneOpts = { ...opts };

export const SOURCE_URL =
  process.env.SOURCE_URL ?? "postgres://app:pw@localhost:55620/tpch";
export const CLONE_URL =
  process.env.CLONE_URL ?? "postgres://postgres:postgres@localhost:55621/postgres";

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
