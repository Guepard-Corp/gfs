import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, type Db, type RunResult, type Query, type RouterRow, type Cost, type Params } from "./api.js";

const BADGE: Record<string, string> = {
  source: "b-source", fetched: "b-fetched", partial: "b-partial", federated: "b-federated", local: "b-local",
};
const PATH_HINT: Record<string, string> = {
  P1: "range-hydrate", P2: "partial-selective", P3: "federate", P5: "time-range",
};

function bytes(n: number): string {
  const u = ["B", "KB", "MB", "GB", "TB"]; let i = 0, v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(1)} ${u[i]}`;
}
const fmt = (n: number) => n.toLocaleString();

function ResultPane({ db, res }: { db: Db; res?: RunResult & { error?: string } }) {
  const cols = res && res.rows[0] ? Object.keys(res.rows[0]) : [];
  return (
    <section className={`pane pane-${db}`}>
      <div className="pane-head">
        <h3>{db.toUpperCase()}</h3>
        {res && !res.error && (
          <span className={`badge ${BADGE[res.servedFrom]}`}>{res.servedFrom} · {res.ms} ms · {fmt(res.rowCount)} rows</span>
        )}
      </div>
      {res?.error ? <p className="err">{res.error}</p> : (
        <div className="tablewrap">
          <table>
            <thead><tr>{cols.map((c) => <th key={c}>{c}</th>)}</tr></thead>
            <tbody>
              {(res?.rows ?? []).slice(0, 50).map((row, i) => (
                <tr key={i}>{cols.map((c) => <td key={c}>{String(row[c])}</td>)}</tr>
              ))}
            </tbody>
          </table>
          {!res && <p className="muted pad">Run a query to compare.</p>}
        </div>
      )}
    </section>
  );
}

function RouterTable({ rows }: { rows: RouterRow[] }) {
  return (
    <table className="router">
      <thead><tr><th>table</th><th>kind</th><th>whole</th><th>partRows</th><th>ranges</th><th>preds</th><th>fetched</th><th>feder</th><th>acc</th></tr></thead>
      <tbody>
        {rows.map((r) => (
          <tr key={r.table} className={r.wholeCached ? "owned" : ""}>
            <td>{r.table}</td>
            <td>{r.chunkKind}</td>
            <td>{r.wholeCached ? "✓" : r.noPartial ? "noP" : "·"}</td>
            <td>{fmt(r.partialRows)}</td>
            <td>{r.cachedRanges}</td>
            <td>{r.cachedPreds}</td>
            <td>{fmt(r.rowsFetched)}</td>
            <td>{r.federateCalls}</td>
            <td>{r.access}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function CostEditor({ cost, onApply }: { cost: Cost; onApply: (c: Cost) => void }) {
  const [draft, setDraft] = useState<Cost>(cost);
  const keys = Object.keys(cost);
  return (
    <div className="cost">
      {keys.map((k) => (
        <label key={k} className="costrow">
          <span>{k}</span>
          <input type="number" value={String(draft[k] ?? cost[k])} step="any"
            onChange={(e) => setDraft({ ...draft, [k]: Number(e.target.value) })} />
        </label>
      ))}
      <button className="apply" onClick={() => onApply(draft)}>Apply weights</button>
      <p className="muted">Tip: lower <b>ceiling</b> → big tables federate; do NOT calibrate (it would make them ownable).</p>
    </div>
  );
}

export function App() {
  const qc = useQueryClient();
  const mode = useQuery({ queryKey: ["mode"], queryFn: api.mode });
  const meta = useQuery({ queryKey: ["meta"], queryFn: api.meta });
  const queries = useQuery({ queryKey: ["queries"], queryFn: api.queries });
  const clone = useQuery({ queryKey: ["clone"], queryFn: api.cloneStatus, refetchInterval: (q) => (q.state.data?.cloned ? false : 1500) });
  const cloned = clone.data?.cloned ?? false;
  const router = useQuery({ queryKey: ["router"], queryFn: api.router, enabled: cloned });
  const cost = useQuery({ queryKey: ["cost"], queryFn: api.cost, enabled: cloned });

  const [sel, setSel] = useState<string>("range");
  const [params, setParams] = useState<Params>({ lo: 1_000_000, hi: 1_000_500, val: 25, from: "1994-01-01", to: "1994-03-31" });
  const [results, setResults] = useState<{ source?: RunResult & { error?: string }; clone?: RunResult & { error?: string } }>({});
  const [running, setRunning] = useState(false);
  const [plan, setPlan] = useState<string>("");

  const cloneM = useMutation({ mutationFn: api.doClone, onSuccess: () => qc.invalidateQueries(), onError: (e) => alert(`Clone failed:\n${String(e)}`) });
  const resetM = useMutation({ mutationFn: api.reset, onSuccess: () => { setResults({}); setPlan(""); qc.invalidateQueries({ queryKey: ["router"] }); qc.invalidateQueries({ queryKey: ["meta"] }); } });
  const costM = useMutation({ mutationFn: api.setCost, onSuccess: () => qc.invalidateQueries({ queryKey: ["cost"] }) });

  const cur: Query | undefined = queries.data?.find((q) => q.id === sel);

  async function runBoth() {
    if (!cloned) return;
    setRunning(true); setPlan("");
    const dbs: Db[] = ["source", "clone"];
    const out = await Promise.all(dbs.map(async (db) => {
      try { return [db, await api.run(db, sel, params)] as const; }
      catch (e) { return [db, { rows: [], ms: 0, servedFrom: "source", rowCount: 0, path: "", error: String(e) }] as const; }
    }));
    setResults(Object.fromEntries(out));
    setRunning(false);
    qc.invalidateQueries({ queryKey: ["router"] });
    qc.invalidateQueries({ queryKey: ["meta"] });
  }
  async function showPlan() {
    try { setPlan((await api.explain(sel, params)).plan); } catch (e) { setPlan(String(e)); }
  }

  const byPath = (p: string) => (queries.data ?? []).filter((q) => q.path === p);

  return (
    <div className="app">
      <header>
        <h1>GFS Benchmark Explorer</h1>
        <p className="sub">Run TPC-H range / selective / multi-table-join queries on the SOURCE and the lazy CLONE, side by side.
          Watch the router pick a path — <b>fetched</b> (range) · <b>partial</b> (selective slice) · <b>federated</b> (join pushed to source) · <b>local</b> (cached) — inspect its catalog, and tune the cost weights live.</p>
      </header>

      <div className="bar">
        <span className="muted">SF <b>{meta.data?.sf ?? "…"}</b></span>
        <span className="muted">source <b>{meta.data ? bytes(meta.data.sizeBytes) : "…"}</b> · <b>{meta.data ? fmt(meta.data.sourceRows) : "…"}</b> rows</span>
        {cloned && router.data && <span className="muted">clone fetched <b>{fmt(router.data.reduce((a, r) => a + r.rowsFetched, 0))}</b> rows</span>}
        <span className="grow" />
        {cloned ? (
          <>
            <span className="badge b-cloned">cloned{clone.data?.ms != null ? ` in ${(clone.data.ms / 1000).toFixed(2)}s` : ""}</span>
            <button className="warm" disabled={resetM.isPending} onClick={() => resetM.mutate()}>↺ reset clone</button>
          </>
        ) : clone.data?.cloning || cloneM.isPending ? <span className="muted">cloning…</span>
          : <button className="clone-btn" onClick={() => cloneM.mutate()}>▶ Clone the source</button>}
      </div>

      {cloned && mode.data && (
        <div className="bar connbar">
          <span className="muted">source:</span><code className="connstr">{mode.data.sourceUrl}</code>
          <span className="muted">clone:</span><code className="connstr">{mode.data.cloneUrl}</code>
        </div>
      )}

      <div className="grid">
        <aside className="catalog">
          {["P1", "P2", "P3", "P5"].map((p) => (
            <div key={p} className="group">
              <h4>{p} · {PATH_HINT[p]}</h4>
              {byPath(p).map((q) => (
                <button key={q.id} className={`q ${q.id === sel ? "on" : ""}`} title={q.hint} onClick={() => setSel(q.id)}>{q.label}</button>
              ))}
            </div>
          ))}
        </aside>

        <main className="center">
          <div className="controls">
            {sel === "range" && (
              <>
                <label>l_orderkey <input type="number" value={params.lo} onChange={(e) => setParams({ ...params, lo: Number(e.target.value) })} /></label>
                <label>… <input type="number" value={params.hi} onChange={(e) => setParams({ ...params, hi: Number(e.target.value) })} /></label>
              </>
            )}
            {sel === "selective" && (
              <label>l_quantity = <input type="number" value={params.val} onChange={(e) => setParams({ ...params, val: Number(e.target.value) })} /></label>
            )}
            {sel === "temporal" && (
              <>
                <label>o_orderdate <input type="date" value={params.from} onChange={(e) => setParams({ ...params, from: e.target.value })} /></label>
                <label>… <input type="date" value={params.to} onChange={(e) => setParams({ ...params, to: e.target.value })} /></label>
              </>
            )}
            <button className="run" disabled={!cloned || running} onClick={runBoth}>{running ? "running…" : "▶ Run on both"}</button>
            <button className="run again" disabled={!cloned || running} onClick={runBoth} title="re-run to watch the route flip (fetched → local, federated → partial → local)">↻ again</button>
            <button disabled={!cloned} onClick={showPlan} title="EXPLAIN the clone's plan — a single Foreign Scan = the join was pushed to the source">plan</button>
            <span className="grow" />
            <span className="muted hint">{cur?.hint}</span>
          </div>

          <div className="panes">
            <ResultPane db="source" res={results.source} />
            <ResultPane db="clone" res={results.clone} />
          </div>

          {plan && (
            <div className="plan">
              <div className="pane-head"><h3>CLONE plan (EXPLAIN)</h3><button className="x" onClick={() => setPlan("")}>✕</button></div>
              <pre>{plan}</pre>
            </div>
          )}
        </main>

        <aside className="side">
          <h4>Router state <span className="muted">(gfs.clones)</span></h4>
          {cloned && router.data ? <div className="routerwrap"><RouterTable rows={router.data} /></div> : <p className="muted pad">clone to inspect</p>}
          <h4>Cost weights <span className="muted">(gfs.cost)</span></h4>
          {cloned && cost.data ? <CostEditor cost={cost.data} onApply={(c) => costM.mutate(c)} /> : <p className="muted pad">clone to tune</p>}
        </aside>
      </div>
    </div>
  );
}
