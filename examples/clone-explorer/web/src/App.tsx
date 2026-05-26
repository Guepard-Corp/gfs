import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "./api.js";
import { Panel } from "./Panel.js";

export interface CloneState {
  cloned: boolean;
  ms: number | null;
  cloning: boolean;
}

export function App() {
  const qc = useQueryClient();
  const [page, setPage] = useState(0);
  const [size, setSize] = useState(20);
  const [q, setQ] = useState("");

  const metaQ = useQuery({ queryKey: ["meta"], queryFn: api.meta });
  const cloneQ = useQuery({ queryKey: ["clone"], queryFn: api.cloneStatus });
  const cloneM = useMutation({
    mutationFn: api.doClone,
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["clone"] });
      qc.invalidateQueries({ queryKey: ["products"] });
      qc.invalidateQueries({ queryKey: ["stats"] });
    },
    onError: (e) => alert(`Clone failed:\n${String(e)}`),
  });

  const maxId = metaQ.data?.maxId ?? 0;
  const lastPage = size > 0 ? Math.max(0, Math.ceil(maxId / size) - 1) : 0;
  const clone: CloneState = {
    cloned: cloneQ.data?.cloned ?? false,
    ms: cloneQ.data?.ms ?? null,
    cloning: cloneM.isPending,
  };

  return (
    <div className="app">
      <header>
        <h1>GFS Clone Explorer</h1>
        <p className="sub">
          Same app, same tables, two databases. Clone the <b>source</b> from the
          panel on the right and watch how long it takes. The <b>clone</b> copies
          nothing up front: it reads through to the source on demand, until you
          warm a page into it or write to it.
        </p>
      </header>

      <div className="toolbar">
        <button onClick={() => setPage((p) => Math.max(0, p - 1))} disabled={page === 0 || !!q}>
          ‹ Prev
        </button>
        <span className="pageinfo">
          {q ? "search" : `page ${page + 1} / ${lastPage + 1} · ids ${page * size + 1}–${page * size + size}`}
        </span>
        <button onClick={() => setPage((p) => Math.min(lastPage, p + 1))} disabled={page >= lastPage || !!q}>
          Next ›
        </button>

        <label>
          size
          <select value={size} onChange={(e) => { setSize(Number(e.target.value)); setPage(0); }}>
            {[20, 50, 100, 200].map((s) => (
              <option key={s} value={s}>{s}</option>
            ))}
          </select>
        </label>

        <input
          className="search"
          placeholder="fuzzy search name (federates)…"
          value={q}
          onChange={(e) => { setQ(e.target.value); setPage(0); }}
        />

        <span className="grow" />
        <span className="total">source rows: {maxId.toLocaleString()}</span>
      </div>

      <div className="panes">
        <Panel db="source" page={page} size={size} q={q} />
        <Panel db="clone" page={page} size={size} q={q} clone={clone} onClone={() => cloneM.mutate()} />
      </div>
    </div>
  );
}
