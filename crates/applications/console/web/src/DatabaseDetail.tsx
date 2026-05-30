import { useEffect, useState } from 'react';

import {
  api,
  metricTotal,
  type DsTable,
  type GfsDatabase,
  type SchemaSnapshot,
  type Telemetry,
} from './api';

const TABS = ['overview', 'connection', 'lineage', 'telemetry', 'operations'] as const;
type Tab = (typeof TABS)[number];

const STATE_DOT: Record<string, string> = {
  running: 'bg-emerald-500',
  restarting: 'bg-amber-500',
  paused: 'bg-amber-500',
  exited: 'bg-muted-foreground/60',
  dead: 'bg-destructive',
};

/** Per-DB detail, mirroring console-v3's environment service panel. */
export function DatabaseDetail({
  db,
  databases,
  telemetry,
  onBack,
  onDone,
}: {
  db: GfsDatabase;
  databases: GfsDatabase[];
  telemetry: Telemetry | null;
  onBack: () => void;
  onDone: () => void;
}) {
  const [tab, setTab] = useState<Tab>('overview');
  const isClone = db.role === 'clone';
  const tabs = TABS.filter((t) => t !== 'lineage' || isClone);
  const dot = STATE_DOT[db.state ?? ''] ?? 'bg-muted-foreground/60';

  return (
    <div className="flex flex-col">
      {/* Hero */}
      <div className="border-b-2 border-border bg-muted/20 px-8 pt-5 pb-4">
        <button
          onClick={onBack}
          className="mb-4 text-[10px] font-black uppercase tracking-widest text-muted-foreground hover:text-foreground cursor-pointer bg-transparent border-0"
        >
          ← Databases
        </button>
        <div className="flex items-center gap-3">
          <div className="h-12 w-12 border-2 border-border bg-card flex items-center justify-center">
            <span className={`h-3 w-3 ${dot}`} />
          </div>
          <div className="min-w-0">
            <div className="text-xl font-black uppercase tracking-tight leading-none truncate">
              {db.name}
            </div>
            <div className="mt-1.5 flex items-center gap-2 text-[11px] font-mono text-muted-foreground">
              <span
                className={`border-2 px-2 h-5 inline-flex items-center text-[10px] font-black uppercase tracking-widest ${
                  isClone
                    ? 'border-emerald-500/40 text-emerald-400 bg-emerald-500/10'
                    : 'border-border text-muted-foreground bg-muted'
                }`}
              >
                {db.role}
              </span>
              {db.provider ?? '?'} · {db.provider_version ?? '?'}
            </div>
          </div>
        </div>
      </div>

      {/* Tabs */}
      <div className="border-b border-border/60 px-8">
        <div role="tablist" className="flex items-center gap-6">
          {tabs.map((t) => (
            <button
              key={t}
              role="tab"
              aria-selected={tab === t}
              onClick={() => setTab(t)}
              className={`relative -mb-px py-3 text-[11px] font-black uppercase tracking-widest cursor-pointer bg-transparent border-0 ${
                tab === t ? 'text-foreground' : 'text-muted-foreground hover:text-foreground'
              }`}
            >
              {t}
              {tab === t ? (
                <span className="absolute inset-x-0 -bottom-px h-[2px] bg-foreground" />
              ) : null}
            </button>
          ))}
        </div>
      </div>

      <div className="px-8 py-8 max-w-[820px]">
        {tab === 'overview' ? <Overview db={db} /> : null}
        {tab === 'connection' ? <Connection db={db} /> : null}
        {tab === 'lineage' ? <Lineage db={db} databases={databases} /> : null}
        {tab === 'telemetry' ? <TelemetryTab telemetry={telemetry} /> : null}
        {tab === 'operations' ? <Operations db={db} onDone={onDone} /> : null}
      </div>
    </div>
  );
}

function SectionHeader({ title }: { title: string }) {
  return (
    <h3 className="text-[10px] font-black uppercase tracking-[0.4em] text-muted-foreground mb-5 flex items-center gap-4">
      {title}
      <div className="h-[2px] flex-1 bg-border" />
    </h3>
  );
}

function DataRow({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="flex items-center justify-between gap-4 py-2 border-b border-border/40 last:border-b-0">
      <span className="text-[11px] font-black uppercase tracking-[0.2em] text-muted-foreground">
        {label}
      </span>
      <span
        className={`min-w-0 truncate text-right text-foreground ${
          mono ? 'font-mono text-[11px]' : 'font-black uppercase text-xs tracking-widest'
        }`}
      >
        {value}
      </span>
    </div>
  );
}

function Overview({ db }: { db: GfsDatabase }) {
  return (
    <div>
      <SectionHeader title="Details" />
      <div className="flex flex-col">
        <DataRow label="Status" value={db.status ?? db.state ?? '—'} />
        <DataRow label="Role" value={db.role} />
        <DataRow label="Provider" value={db.provider ?? '—'} />
        <DataRow label="Version" value={db.provider_version ?? '—'} mono />
        <DataRow label="Port" value={db.host_port ? String(db.host_port) : '—'} mono />
        {db.remote ? <DataRow label="Remote" value={db.remote} mono /> : null}
        {db.repo ? <DataRow label="Repo" value={db.repo} mono /> : null}
        <DataRow label="Image" value={db.image ?? '—'} mono />
        <DataRow label="Container" value={db.container_id.slice(0, 12)} mono />
      </div>
    </div>
  );
}

function Connection({ db }: { db: GfsDatabase }) {
  const uri = db.host_port
    ? `postgresql://postgres:postgres@localhost:${db.host_port}/postgres`
    : null;
  return (
    <div>
      <SectionHeader title="Connection" />
      {uri ? (
        <CopyBox value={uri} />
      ) : (
        <p className="text-[11px] font-black uppercase tracking-widest text-muted-foreground">
          No published port
        </p>
      )}
    </div>
  );
}

function CopyBox({ value }: { value: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="flex items-center gap-2 border-2 border-border bg-muted/30 px-3 py-2.5 font-mono text-[12px]">
      <span className="min-w-0 flex-1 truncate text-foreground">{value}</span>
      <button
        onClick={() => {
          navigator.clipboard.writeText(value).catch(() => {});
          setCopied(true);
          setTimeout(() => setCopied(false), 1500);
        }}
        className="shrink-0 text-[10px] font-black uppercase tracking-widest text-muted-foreground hover:text-foreground cursor-pointer bg-transparent border-0"
      >
        {copied ? 'Copied' : 'Copy'}
      </button>
    </div>
  );
}

function Lineage({ db, databases }: { db: GfsDatabase; databases: GfsDatabase[] }) {
  const remoteHost = db.remote ?? '';
  const match = databases.find(
    (s) => s.role !== 'clone' && s.host_port && remoteHost.endsWith(`:${s.host_port}`),
  );
  return (
    <div className="flex flex-col gap-8">
      <div>
        <SectionHeader title="Lineage" />
        <div className="flex items-center gap-4">
          <Node label={db.name} sub="clone" tone="clone" />
          <div className="flex items-center gap-2 text-muted-foreground">
            <div className="h-[2px] w-10 bg-border" />
            <span className="text-[9px] font-black uppercase tracking-widest">overlays</span>
            <div className="h-[2px] w-10 bg-border" />
          </div>
          <Node label={match ? match.name : remoteHost || 'remote'} sub={match ? 'source' : 'remote'} tone="remote" />
        </div>
      </div>
      <SchemaExplorer name={db.name} />
    </div>
  );
}

/** Internal GFS plumbing schemas hidden from the explorer. */
function isInternalSchema(name: string): boolean {
  return name.startsWith('gfs_');
}

/** Live DB → schema → table tree with size + row-count columns (polled). */
function SchemaExplorer({ name }: { name: string }) {
  const [snap, setSnap] = useState<SchemaSnapshot | null>(null);
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});

  useEffect(() => {
    let alive = true;
    const tick = async () => {
      try {
        const s = await api.schema(name);
        if (alive) setSnap(s);
      } catch {
        /* keep last good snapshot */
      }
    };
    void tick();
    const id = setInterval(() => void tick(), 5000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, [name]);

  // Group the flat DatasourceMetadata.tables by schema, dropping gfs_* internals.
  const grouped = (() => {
    const meta = snap?.metadata;
    if (!meta) return [] as { name: string; tables: DsTable[] }[];
    const order = meta.schemas
      .map((s) => s.name)
      .filter((n) => !isInternalSchema(n));
    const bySchema = new Map<string, DsTable[]>();
    for (const t of meta.tables) {
      if (isInternalSchema(t.schema)) continue;
      const list = bySchema.get(t.schema) ?? [];
      list.push(t);
      bySchema.set(t.schema, list);
    }
    return order
      .filter((n) => bySchema.has(n))
      .map((n) => ({
        name: n,
        tables: (bySchema.get(n) ?? []).sort((a, b) => a.name.localeCompare(b.name)),
      }));
  })();

  return (
    <div>
      <SectionHeader title="Data — live" />
      {snap && !snap.reachable ? (
        <p className="text-[11px] font-black uppercase tracking-widest text-muted-foreground">
          Schema unavailable{snap.error ? ` — ${snap.error}` : ''}
        </p>
      ) : !snap ? (
        <p className="text-[11px] font-black uppercase tracking-widest text-muted-foreground">
          Loading…
        </p>
      ) : (
        <div className="border-2 border-border">
          <div className="flex items-center bg-muted/40 border-b-2 border-border px-3 py-2 text-[9px] font-black uppercase tracking-[0.2em] text-muted-foreground">
            <span className="flex-1">{name}</span>
            <span className="w-28 text-right">Size</span>
            <span className="w-28 text-right">Rows</span>
          </div>
          {grouped.length === 0 ? (
            <div className="px-3 py-4 text-[11px] font-black uppercase tracking-widest text-muted-foreground">
              No user tables
            </div>
          ) : (
            grouped.map((s) => {
              const isCollapsed = collapsed[s.name];
              return (
                <div key={s.name}>
                  <button
                    onClick={() =>
                      setCollapsed((c) => ({ ...c, [s.name]: !c[s.name] }))
                    }
                    className="w-full flex items-center gap-2 px-3 py-2 border-b border-border/40 bg-muted/20 text-left cursor-pointer"
                  >
                    <span className="text-[10px] text-muted-foreground w-3">
                      {isCollapsed ? '▸' : '▾'}
                    </span>
                    <span className="flex-1 text-[11px] font-black uppercase tracking-[0.15em]">
                      {s.name}
                    </span>
                    <span className="text-[10px] font-mono text-muted-foreground">
                      {s.tables.length} tables
                    </span>
                  </button>
                  {!isCollapsed
                    ? s.tables.map((t) => (
                        <div
                          key={t.name}
                          className="flex items-center px-3 py-1.5 border-b border-border/20 last:border-b-0 hover:bg-muted/20"
                        >
                          <span className="flex-1 pl-5 font-mono text-[12px] truncate">
                            {t.name}
                          </span>
                          <span className="w-28 text-right font-mono text-[11px] text-muted-foreground tabular-nums">
                            {t.size}
                          </span>
                          <span className="w-28 text-right font-mono text-[12px] tabular-nums">
                            {t.live_rows_estimate.toLocaleString()}
                          </span>
                        </div>
                      ))
                    : null}
                </div>
              );
            })
          )}
        </div>
      )}
    </div>
  );
}

function Node({ label, sub, tone }: { label: string; sub: string; tone: 'clone' | 'remote' }) {
  return (
    <div
      className={`border-2 px-4 py-3 min-w-[220px] ${
        tone === 'clone' ? 'border-emerald-500/40 bg-emerald-500/5' : 'border-border bg-muted/30'
      }`}
    >
      <div className="text-[9px] font-black uppercase tracking-[0.25em] text-muted-foreground mb-1">
        {sub}
      </div>
      <div className="font-mono text-[12px] truncate">{label}</div>
    </div>
  );
}

function TelemetryTab({ telemetry }: { telemetry: Telemetry | null }) {
  if (!telemetry || !telemetry.reachable) {
    return (
      <div>
        <SectionHeader title="Telemetry" />
        <p className="text-[11px] font-black uppercase tracking-widest text-muted-foreground">
          Proxy unreachable at {telemetry?.proxy_url ?? '—'} — start it with --discover
        </p>
      </div>
    );
  }
  const m = telemetry.metrics;
  const stats = [
    { label: 'Connections', value: metricTotal(m, 'proxy_connections_active') },
    { label: 'Queries', value: metricTotal(m, 'proxy_queries_total') },
    { label: 'Warm calls', value: metricTotal(m, 'proxy_warm_calls_total') },
    { label: 'Cache ranges', value: metricTotal(m, 'proxy_cache_ranges') },
    { label: 'Overlay tables', value: metricTotal(m, 'proxy_overlay_tables') },
  ];
  return (
    <div>
      <SectionHeader title="Telemetry" />
      <p className="text-[10px] font-black uppercase tracking-widest text-muted-foreground mb-4">
        Proxy-wide (all fronted clones)
      </p>
      <div className="grid grid-cols-[repeat(auto-fit,minmax(150px,1fr))] gap-4">
        {stats.map((s) => (
          <div key={s.label} className="bg-muted border-2 border-border p-4">
            <div className="text-[9px] font-black uppercase tracking-[0.2em] text-muted-foreground mb-3">
              {s.label}
            </div>
            <div className="text-4xl font-black tracking-tighter tabular-nums leading-none">
              {Number.isInteger(s.value) ? s.value : s.value.toFixed(1)}
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

function Operations({ db, onDone }: { db: GfsDatabase; onDone: () => void }) {
  const [busy, setBusy] = useState(false);
  const compute = async (action: 'start' | 'stop' | 'restart') => {
    if (!db.repo) return;
    setBusy(true);
    try {
      await api.compute({ repo: db.repo, action });
      await onDone();
    } finally {
      setBusy(false);
    }
  };
  return (
    <div>
      <SectionHeader title="Operations" />
      {db.repo ? (
        <div className="flex gap-3 max-w-[420px]">
          {(['start', 'stop', 'restart'] as const).map((a) => (
            <button
              key={a}
              disabled={busy}
              onClick={() => void compute(a)}
              className="flex-1 h-11 border-2 border-border bg-muted text-[11px] font-black uppercase tracking-widest cursor-pointer hover:border-foreground disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {a}
            </button>
          ))}
        </div>
      ) : (
        <p className="text-[11px] font-black uppercase tracking-widest text-muted-foreground">
          No repo path on this container — operations unavailable
        </p>
      )}
    </div>
  );
}
