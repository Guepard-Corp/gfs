export type Db = "source" | "clone";
export type Served = "source" | "fetched" | "partial" | "federated" | "local";

export type Query = { id: string; label: string; path: "P1" | "P2" | "P3" | "P5"; hint: string; parametric: boolean };
export type RunResult = { rows: Record<string, unknown>[]; ms: number; servedFrom: Served; rowCount: number; path: string };
export type Meta = { tables: { name: string; rows: number }[]; sourceRows: number; sizeBytes: number; sf: number };
export type RouterRow = {
  table: string; chunkKind: string; wholeCached: boolean; noPartial: boolean; partialRows: number; access: number;
  rowsFetched: number; federateCalls: number; cachedRanges: number; cachedPreds: number;
};
export type Cost = Record<string, number>;
export type Params = { lo?: number; hi?: number; val?: number; from?: string; to?: string };

async function j<T>(url: string, init?: RequestInit): Promise<T> {
  const r = await fetch(url, init);
  if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error ?? `HTTP ${r.status}`);
  return r.json() as Promise<T>;
}
const qs = (p: Params) => Object.entries(p).filter(([, v]) => v !== undefined).map(([k, v]) => `&${k}=${v}`).join("");

export const api = {
  mode: () => j<{ sourceUrl: string; cloneUrl: string }>("/api/mode"),
  cloneStatus: () => j<{ cloned: boolean; ms: number | null; cloning: boolean }>("/api/clone"),
  doClone: () => j<{ ok: boolean; ms: number }>("/api/clone", { method: "POST" }),
  meta: () => j<Meta>("/api/meta"),
  queries: () => j<Query[]>("/api/queries"),
  run: (db: Db, id: string, p: Params) => j<RunResult>(`/api/run?db=${db}&id=${id}${qs(p)}`),
  explain: (id: string, p: Params) => j<{ plan: string }>(`/api/explain?id=${id}${qs(p)}`),
  router: () => j<RouterRow[]>("/api/router"),
  cost: () => j<Cost>("/api/cost"),
  setCost: (body: Cost) => j<{ ok: boolean }>("/api/cost", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) }),
  reset: () => j<{ ok: boolean }>("/api/reset", { method: "POST" }),
};
