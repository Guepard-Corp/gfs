import { useState } from "react";
import { useMutation, useQuery, useQueryClient, keepPreviousData } from "@tanstack/react-query";
import { api, type Db, type Result, type Stats } from "./api.js";

const BADGE: Record<string, string> = { source: "b-source", remote: "b-remote", local: "b-local" };
const LABEL: Record<string, string> = { source: "source", remote: "remote read", local: "local (elided)" };

function prettyBytes(n: number): string {
  const u = ["B", "KB", "MB", "GB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(1)} ${u[i]}`;
}

function Pane({ db, query }: { db: Db; query: { data?: Result; error: unknown; isFetching: boolean } }) {
  const r = query.data;
  const cols = r && r.rows[0] ? Object.keys(r.rows[0]) : [];
  return (
    <section className={`pane pane-${db}`}>
      <div className="pane-head">
        <h3>{db === "source" ? "SOURCE" : "CLONE"}</h3>
        {r && (
          <span className={`badge ${BADGE[r.servedFrom]}`}>
            {LABEL[r.servedFrom]} · {r.ms} ms · {r.rows.length} rows
          </span>
        )}
      </div>
      {query.error ? (
        <p className="err">{String(query.error)}</p>
      ) : (
        // Overlay a spinner while fetching; the previous rows stay visible
        // underneath (placeholderData: keepPreviousData).
        <div className="tablewrap">
          {query.isFetching && (
            <div className="loading-overlay"><span className="spinner" /></div>
          )}
          <table>
            <thead><tr>{cols.map((c) => <th key={c}>{c}</th>)}</tr></thead>
            <tbody>
              {(r?.rows ?? []).slice(0, 50).map((row, i) => (
                <tr key={i}>{cols.map((c) => <td key={c}>{String((row as Record<string, unknown>)[c])}</td>)}</tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function TwoPane({
  keyParts, fetcher, cloned, proxy, canElide,
}: { keyParts: unknown[]; fetcher: (db: Db) => Promise<Result>; cloned: boolean; proxy: boolean; canElide: boolean }) {
  const src = useQuery({ queryKey: ["src", ...keyParts], queryFn: () => fetcher("source"), placeholderData: keepPreviousData });
  const cln = useQuery({
    queryKey: ["cln", ...keyParts], queryFn: () => fetcher("clone"), enabled: cloned, placeholderData: keepPreviousData,
    // In proxy mode the warmer hydrates in the background. Re-poll only for
    // scenarios that can actually flip (key range / whole-cache), and only
    // while still served remotely — stop once it's local. Joins, aggregates and
    // temporal filters federate forever, so we never re-poll them.
    refetchInterval: (q) =>
      proxy && canElide && q.state.data?.servedFrom === "remote" ? 2500 : false,
  });
  return (
    <div className="panes">
      <Pane db="source" query={src} />
      {cloned ? <Pane db="clone" query={cln} /> : <section className="pane pane-clone"><p className="muted">Clone the source to compare.</p></section>}
    </div>
  );
}

type Tab = "products" | "search" | "reviews" | "category" | "customer" | "recent" | "dashboard";
const TABS: { id: Tab; label: string; hint: string }[] = [
  { id: "products", label: "Products (range)", hint: "key predicate → elided once warmed" },
  { id: "search", label: "Fuzzy search", hint: "non-key → federates, then local once whole-cached" },
  { id: "reviews", label: "Reviews search", hint: "fuzzy on text body" },
  { id: "category", label: "By category", hint: "JOIN products⋈categories" },
  { id: "customer", label: "Customer orders", hint: "3-table JOIN" },
  { id: "recent", label: "Recent orders", hint: "temporal filter (federates)" },
  { id: "dashboard", label: "Dashboard", hint: "aggregate JOIN (federates)" },
];

function StatLine({ label, db }: { label: string; db: Db }) {
  const s = useQuery({ queryKey: ["stats", db], queryFn: () => api.stats(db), refetchInterval: 4000 });
  const d = s.data as Stats | undefined;
  if (!d) return <span className="muted">{label}: …</span>;
  return (
    <span className="stat">
      {label}: <b>{prettyBytes(d.sizeBytes)}</b>
      {db === "clone" && (
        <> · local products <b>{d.localProducts ?? "—"}</b> · ranges <b>{d.cachedRanges ?? "—"}</b> · whole <b>{d.fullyCached ?? "—"}</b></>
      )}
    </span>
  );
}

export function App() {
  const qc = useQueryClient();
  const mode = useQuery({ queryKey: ["mode"], queryFn: api.mode });
  const meta = useQuery({ queryKey: ["meta"], queryFn: api.meta });
  // Poll only until the clone exists, then stop (no point re-asking forever).
  const clone = useQuery({
    queryKey: ["clone"], queryFn: api.cloneStatus,
    refetchInterval: (q) => (q.state.data?.cloned ? false : 1500),
  });
  const cloneM = useMutation({
    mutationFn: api.doClone,
    onSuccess: () => qc.invalidateQueries(),
    onError: (e) => alert(`Clone failed:\n${String(e)}`),
  });
  const cloned = clone.data?.cloned ?? false;
  const proxy = mode.data?.proxy ?? false;
  const maxProduct = meta.data?.maxProduct ?? 0;
  const cats = meta.data?.categories ?? [];

  const [tab, setTab] = useState<Tab>("products");
  const [page, setPage] = useState(0);
  const [term, setTerm] = useState("");
  const [cat, setCat] = useState("");
  const [cust, setCust] = useState(1);
  const [days, setDays] = useState(7);
  const size = 50;
  const lo = page * size + 1;
  const hi = lo + size - 1;

  const warmM = useMutation({
    mutationFn: (r: { table: string; lo: number; hi: number }) => api.warm(r.table, r.lo, r.hi),
    onSuccess: () => qc.invalidateQueries(),
  });

  // Only these scenarios can ever be served locally (key-range CHECK, or
  // whole-table promotion for the fuzzy ones); the rest federate by design.
  const canElide = tab === "products" || tab === "search" || tab === "reviews";

  let body = null as React.ReactNode;
  if (tab === "products")
    body = <TwoPane keyParts={["products", lo, hi]} cloned={cloned} proxy={proxy} canElide={canElide} fetcher={(db) => api.products(db, lo, hi)} />;
  else if (tab === "search")
    body = <TwoPane keyParts={["search", term]} cloned={cloned} proxy={proxy} canElide={canElide} fetcher={(db) => api.search(db, term)} />;
  else if (tab === "reviews")
    body = <TwoPane keyParts={["reviews", term]} cloned={cloned} proxy={proxy} canElide={canElide} fetcher={(db) => api.reviews(db, term)} />;
  else if (tab === "category")
    body = <TwoPane keyParts={["category", cat]} cloned={cloned} proxy={proxy} canElide={canElide} fetcher={(db) => api.category(db, cat || cats[0] || "games")} />;
  else if (tab === "customer")
    body = <TwoPane keyParts={["customer", cust]} cloned={cloned} proxy={proxy} canElide={canElide} fetcher={(db) => api.customerOrders(db, cust)} />;
  else if (tab === "recent")
    body = <TwoPane keyParts={["recent", days]} cloned={cloned} proxy={proxy} canElide={canElide} fetcher={(db) => api.recentOrders(db, days)} />;
  else body = <TwoPane keyParts={["dashboard"]} cloned={cloned} proxy={proxy} canElide={canElide} fetcher={(db) => api.dashboard(db)} />;

  return (
    <div className="app">
      <header>
        <h1>GFS Clone Explorer <span className="pro">Pro</span>{proxy && <span className="badge b-proxy"> ⚡ proxy mode</span>}</h1>
        <p className="sub">Multi-table source vs lazy GFS clone: ranges, fuzzy search, joins, temporal filters, an aggregate dashboard, and copy-on-write writes.</p>
      </header>

      <div className="bar">
        <StatLine label="source" db="source" />
        <span className="grow" />
        {cloned ? (
          <StatLine label="clone" db="clone" />
        ) : clone.data?.cloning ? (
          <span className="muted">cloning…</span>
        ) : (
          <button className="clone-btn" onClick={() => cloneM.mutate()}>▶ Clone the source</button>
        )}
        {cloned && clone.data?.ms != null && <span className="badge b-cloned">cloned in {(clone.data.ms / 1000).toFixed(2)} s</span>}
        <span className="muted">source rows: {maxProduct.toLocaleString()}</span>
      </div>

      <div className="tabs">
        {TABS.map((t) => (
          <button key={t.id} className={t.id === tab ? "tab on" : "tab"} onClick={() => setTab(t.id)} title={t.hint}>{t.label}</button>
        ))}
      </div>

      <div className="controls">
        {tab === "products" && (
          <>
            <button onClick={() => setPage((p) => Math.max(0, p - 1))} disabled={page === 0}>‹ Prev</button>
            <span className="pageinfo">ids {lo}–{hi}</span>
            <button onClick={() => setPage((p) => p + 1)}>Next ›</button>
            {cloned && (
              <button className="warm" disabled={warmM.isPending} onClick={() => warmM.mutate({ table: "products", lo, hi })}>↧ warm this page</button>
            )}
            {cloned && (
              <button className="warm" disabled={warmM.isPending} onClick={() => warmM.mutate({ table: "products", lo: 1, hi: maxProduct })}>
                ↧↧ warm whole products
              </button>
            )}
          </>
        )}
        {(tab === "search" || tab === "reviews") && (
          <input className="search" placeholder="fuzzy term (e.g. 'bravo', 'great')…" value={term} onChange={(e) => setTerm(e.target.value)} />
        )}
        {tab === "category" && (
          <select value={cat} onChange={(e) => setCat(e.target.value)}>
            {cats.map((c) => <option key={c} value={c}>{c}</option>)}
          </select>
        )}
        {tab === "customer" && (
          <label>customer n <input type="number" min={1} value={cust} onChange={(e) => setCust(Math.max(1, Number(e.target.value)))} /></label>
        )}
        {tab === "recent" && (
          <label>last <input type="number" min={1} value={days} onChange={(e) => setDays(Math.max(1, Number(e.target.value)))} /> days</label>
        )}
        <span className="grow" />
        <span className="muted">{TABS.find((t) => t.id === tab)?.hint}</span>
      </div>

      {body}
    </div>
  );
}
