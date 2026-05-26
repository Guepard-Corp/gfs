import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { api, type Db } from "./api.js";
import type { CloneState } from "./App.js";

interface Props {
  db: Db;
  page: number;
  size: number;
  q: string;
  clone?: CloneState;
  onClone?: () => void;
}

const BADGE: Record<string, string> = {
  source: "badge-source",
  remote: "badge-remote",
  local: "badge-local",
};
const BADGE_LABEL: Record<string, string> = {
  source: "source",
  remote: "remote read",
  local: "local (elided)",
};

function prettyBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n;
  let i = -1;
  do {
    v /= 1024;
    i++;
  } while (v >= 1024 && i < units.length - 1);
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

// Live elapsed-seconds counter shown while the clone runs.
function CloneTimer() {
  const [s, setS] = useState(0);
  useEffect(() => {
    const t0 = Date.now();
    const id = setInterval(() => setS((Date.now() - t0) / 1000), 100);
    return () => clearInterval(id);
  }, []);
  return <span className="timer">cloning… {s.toFixed(1)} s</span>;
}

export function Panel({ db, page, size, q, clone, onClone }: Props) {
  const qc = useQueryClient();
  const isCloneReady = db === "source" || !!clone?.cloned;

  const productsQ = useQuery({
    queryKey: ["products", db, page, size, q],
    // Measure the real client round-trip (HTTP + server + serialization) on top
    // of the server-reported query time.
    queryFn: async () => {
      const t0 = performance.now();
      const res = await api.products(db, page, size, q);
      return { ...res, clientMs: performance.now() - t0 };
    },
    enabled: isCloneReady,
    placeholderData: keepPreviousData, // keep the old page visible while loading
  });
  const statsQ = useQuery({
    queryKey: ["stats", db],
    queryFn: () => api.stats(db),
    enabled: isCloneReady,
    refetchInterval: 3000, // live size/counts (cheap)
  });
  // Logical product total: on the clone this federates a full remote scan (the
  // overlay's anti-join blocks aggregate push-down), so fetch it once, not on
  // the poll. staleTime: Infinity keeps it cached.
  const countQ = useQuery({
    queryKey: ["products-count", db],
    queryFn: () => api.productsCount(db),
    enabled: isCloneReady,
    staleTime: Infinity,
  });

  // A write in either panel refetches both panels (divergence becomes visible).
  const invalidate = () => {
    qc.invalidateQueries({ queryKey: ["products"] });
    qc.invalidateQueries({ queryKey: ["stats"] });
  };
  const warmM = useMutation({ mutationFn: (r: { lo: number; hi: number }) => api.warm(r.lo, r.hi), onSuccess: invalidate });
  const priceM = useMutation({ mutationFn: (r: { id: number; cents: number }) => api.setPrice(db, r.id, r.cents), onSuccess: invalidate });
  const orderM = useMutation({ mutationFn: (id: number) => api.placeOrder(db, id, 1), onSuccess: invalidate });
  const busy = warmM.isPending || priceM.isPending || orderM.isPending;

  const data = productsQ.data;
  const stats = statsQ.data;
  const loading = isCloneReady && productsQ.isFetching;
  const err = productsQ.error ?? warmM.error ?? priceM.error ?? orderM.error;

  const warmPage = () => data && warmM.mutate({ lo: data.lo, hi: data.hi });
  const editPrice = (id: number, current: number) => {
    const v = window.prompt(`New price (cents) for product ${id}`, String(current));
    if (v != null && v.trim() !== "") priceM.mutate({ id, cents: Number(v) });
  };
  const order = (id: number) => orderM.mutate(id);

  // Clone not created yet: show the call-to-action instead of the table.
  if (db === "clone" && !clone?.cloned) {
    return (
      <section className="panel panel-clone">
        <div className="panel-head"><h2>CLONE (GFS)</h2></div>
        <div className="clone-cta">
          <p>
            Nothing cloned yet. <code>gfs clone</code> provisions a local engine
            that reads through to the source on demand: it copies no data up front,
            so it is ready in seconds whatever the source size.
          </p>
          {clone?.cloning ? (
            <CloneTimer />
          ) : (
            <button className="clone-btn" onClick={onClone}>▶ Clone the source</button>
          )}
        </div>
      </section>
    );
  }

  return (
    <section className={`panel panel-${db}`}>
      <div className="panel-head">
        <h2>{db === "source" ? "SOURCE (upstream)" : "CLONE (GFS)"}</h2>
        <div className="head-right">
          {db === "clone" && clone?.ms != null && (
            <span className="badge badge-cloned" title="wall time of the gfs clone command">
              cloned in {(clone.ms / 1000).toFixed(2)} s
            </span>
          )}
          {db === "clone" && (
            <button
              className="warm"
              onClick={warmPage}
              disabled={busy || !data || !!q}
              title="Hydrate this id range into the clone, then it reads locally"
            >
              ↧ Warm this page
            </button>
          )}
          {data && (
            <span
              className={`badge ${BADGE[data.servedFrom]}`}
              title="remote/query time (server) · real page-load time (browser round-trip)"
            >
              {BADGE_LABEL[data.servedFrom]} · {data.ms} ms · load {Math.round(data.clientMs)} ms
            </span>
          )}
        </div>
      </div>

      <div className="stats">
        {stats ? (
          <>
            <span className="size" title="whole-database on-disk size (pg_database_size)">
              size: <b>{prettyBytes(stats.sizeBytes)}</b>
            </span>
            <span
              title={
                db === "clone"
                  ? "count(*) over the overlay federates a full remote scan; fetched once, not polled"
                  : "local table count"
              }
            >
              products: <b>{countQ.data ? countQ.data.count.toLocaleString() : "…"}</b>
            </span>
            <span>orders: <b>{stats.orders.toLocaleString()}</b></span>
            {db === "clone" && (
              <>
                <span title="rows owned locally (warmed or written)">local rows: <b>{stats.localProducts ?? "—"}</b></span>
                <span title="cached key ranges (elided)">cached ranges: <b>{stats.cachedRanges ?? "—"}</b></span>
              </>
            )}
          </>
        ) : (
          <span>…</span>
        )}
      </div>

      {err && <div className="err">{(err as Error).message}</div>}

      <div className="table-wrap">
        {loading && (
          <div className="loading-overlay">
            <span className="spinner" />
          </div>
        )}
        <table>
          <thead>
            <tr><th>id</th><th>name</th><th>category</th><th className="num">price</th><th /></tr>
          </thead>
          <tbody>
            {data?.rows.map((r) => (
              <tr key={r.id}>
                <td className="mono">{r.id}</td>
                <td>{r.name}</td>
                <td>{r.category}</td>
                <td className="num mono">
                  <button className="link" disabled={busy} onClick={() => editPrice(r.id, r.price_cents)}>
                    {(r.price_cents / 100).toFixed(2)}
                  </button>
                </td>
                <td><button className="mini" disabled={busy} onClick={() => order(r.id)}>order</button></td>
              </tr>
            ))}
            {data && data.rows.length === 0 && (
              <tr><td colSpan={5} className="empty">no rows</td></tr>
            )}
          </tbody>
        </table>
      </div>
    </section>
  );
}
