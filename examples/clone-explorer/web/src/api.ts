export type Db = "source" | "clone";

export interface Product {
  id: number;
  name: string;
  category: string;
  price_cents: number;
}

export interface ProductsResponse {
  rows: Product[];
  page: number;
  size: number;
  lo: number;
  hi: number;
  q: string;
  ms: number;
  servedFrom: "source" | "remote" | "local";
}

export interface Stats {
  db: Db;
  orders: number;
  sizeBytes: number;
  localProducts: number | null;
  cachedRanges: number | null;
}

async function json<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, {
    ...init,
    headers: init?.body ? { "content-type": "application/json" } : undefined,
  });
  if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
  return res.json() as Promise<T>;
}

export interface CloneStatus {
  cloned: boolean;
  ms: number | null;
  cloning: boolean;
}

export const api = {
  meta: () => json<{ maxId: number }>("/api/meta"),
  cloneStatus: () => json<CloneStatus>("/api/clone"),
  doClone: () => json<{ ok: true; ms: number }>("/api/clone", { method: "POST" }),
  stats: (db: Db) => json<Stats>(`/api/stats?db=${db}`),
  productsCount: (db: Db) => json<{ count: number }>(`/api/products-count?db=${db}`),
  products: (db: Db, page: number, size: number, q: string) =>
    json<ProductsResponse>(
      `/api/products?db=${db}&page=${page}&size=${size}&q=${encodeURIComponent(q)}`,
    ),
  warm: (lo: number, hi: number) =>
    json<{ hydrated: number }>("/api/warm", {
      method: "POST",
      body: JSON.stringify({ lo, hi }),
    }),
  setPrice: (db: Db, id: number, priceCents: number) =>
    json<{ ok: true }>(`/api/price?db=${db}`, {
      method: "POST",
      body: JSON.stringify({ id, priceCents }),
    }),
  placeOrder: (db: Db, productId: number, qty: number) =>
    json<{ order: unknown }>(`/api/orders?db=${db}`, {
      method: "POST",
      body: JSON.stringify({ productId, qty }),
    }),
};
