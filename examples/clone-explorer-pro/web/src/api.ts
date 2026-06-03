export type Db = "source" | "clone";
export type Served = "source" | "fetched" | "federated" | "local";

export interface Result {
  rows: Record<string, unknown>[];
  ms: number;
  servedFrom: Served;
}
export interface Meta {
  maxProduct: number;
  customers: number;
  orders: number;
  sourceRows: number;
  categories: string[];
}
export interface Stats {
  db: Db;
  sizeBytes: number;
  rowsFetched?: number;
  copyOnRead?: { table: string; fetched: number; calls: number }[];
}
export interface CloneStatus {
  cloned: boolean;
  ms: number | null;
  cloning: boolean;
}

async function json<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, {
    ...init,
    headers: init?.body ? { "content-type": "application/json" } : undefined,
  });
  if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
  return res.json() as Promise<T>;
}
const qs = (db: Db, extra: Record<string, string | number> = {}) =>
  new URLSearchParams({ db, ...Object.fromEntries(Object.entries(extra).map(([k, v]) => [k, String(v)])) }).toString();

export const api = {
  mode: () => json<{ proxy: boolean; sourceUrl: string; cloneUrl: string }>("/api/mode"),
  meta: () => json<Meta>("/api/meta"),
  cloneStatus: () => json<CloneStatus>("/api/clone"),
  doClone: () => json<{ ok: true; ms: number }>("/api/clone", { method: "POST" }),
  stats: (db: Db) => json<Stats>(`/api/stats?${qs(db)}`),

  products: (db: Db, lo: number, hi: number) => json<Result>(`/api/products?${qs(db, { lo, hi })}`),
  search: (db: Db, term: string) => json<Result>(`/api/search?${qs(db, { term })}`),
  reviews: (db: Db, term: string) => json<Result>(`/api/reviews?${qs(db, { term })}`),
  category: (db: Db, cat: string) => json<Result>(`/api/category?${qs(db, { cat })}`),
  customerOrders: (db: Db, n: number) => json<Result>(`/api/customer-orders?${qs(db, { n })}`),
  recentOrders: (db: Db, days: number) => json<Result>(`/api/recent-orders?${qs(db, { days })}`),
  dashboard: (db: Db) => json<Result>(`/api/dashboard?${qs(db)}`),

  warm: (table: string) =>
    json<{ hydrated: number }>("/api/warm", { method: "POST", body: JSON.stringify({ table }) }),
  placeOrder: (db: Db, customerN: number, productId: number, qty: number) =>
    json<{ orderId: number; totalCents: number }>(`/api/order?${qs(db)}`, {
      method: "POST",
      body: JSON.stringify({ customerN, productId, qty }),
    }),
};
