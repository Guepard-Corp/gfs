import { useState } from 'react';
import {
  createRootRoute,
  createRoute,
  createRouter,
  Link,
  Outlet,
  useNavigate,
  useParams,
} from '@tanstack/react-router';

import { api, type GfsDatabase } from './api';
import { useData } from './data';
import { DatabaseDetail } from './DatabaseDetail';

const STATE_DOT: Record<string, string> = {
  running: 'bg-emerald-500',
  restarting: 'bg-amber-500',
  paused: 'bg-amber-500',
  exited: 'bg-muted-foreground/60',
  dead: 'bg-destructive',
};

// ─── Layout ──────────────────────────────────────────────────────────────────

function RootLayout() {
  const { error, refresh } = useData();
  return (
    <div className="min-h-full bg-background text-foreground">
      <header className="border-b-2 border-border bg-muted/20 px-8 py-5 flex items-center justify-between">
        <Link to="/" className="flex items-center no-underline">
          <img src="/guepard-logo.svg" alt="Guepard" className="h-8" />
        </Link>
        <NewCloneForm onDone={refresh} />
      </header>

      {error ? (
        <div className="m-8 border-2 border-destructive/40 bg-destructive/10 text-destructive p-4 text-xs font-black uppercase tracking-widest">
          {error}
        </div>
      ) : null}

      <Outlet />
    </div>
  );
}

// ─── Pages ───────────────────────────────────────────────────────────────────

function IndexPage() {
  const { databases } = useData();
  return (
    <main className="px-8 py-8">
      <h2 className="text-[10px] font-black uppercase tracking-[0.4em] text-muted-foreground mb-5 flex items-center gap-4">
        GFS Databases
        <span className="text-foreground">{databases.length}</span>
        <div className="h-[2px] flex-1 bg-border" />
      </h2>

      {databases.length === 0 ? (
        <div className="border-2 border-border bg-muted/20 p-6 text-center text-xs font-black uppercase tracking-widest text-muted-foreground">
          No gfs.managed containers found
        </div>
      ) : (
        <div className="grid grid-cols-[repeat(auto-fill,minmax(300px,1fr))] gap-4">
          {databases.map((d) => (
            <DatabaseCard key={d.container_id} db={d} />
          ))}
        </div>
      )}
    </main>
  );
}

function DatabaseCard({ db }: { db: GfsDatabase }) {
  const isClone = db.role === 'clone';
  const dot = STATE_DOT[db.state ?? ''] ?? 'bg-muted-foreground/60';
  return (
    <Link
      to="/db/$name"
      params={{ name: db.name }}
      className="no-underline text-foreground bg-card border-2 border-border p-4 flex flex-col gap-3 cursor-pointer transition-colors hover:border-foreground hover:bg-muted/40"
    >
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0">
          <div className="text-[15px] font-black uppercase tracking-tight truncate leading-none">
            {db.name}
          </div>
          <div className="mt-1.5 flex items-center gap-2 text-[11px] font-mono text-muted-foreground">
            <span className={`h-2 w-2 ${dot}`} />
            {db.provider ?? '?'} · {db.provider_version ?? '?'}
          </div>
        </div>
        <span
          className={`shrink-0 border-2 px-2 h-5 inline-flex items-center text-[10px] font-black uppercase tracking-widest ${
            isClone
              ? 'border-emerald-500/40 text-emerald-400 bg-emerald-500/10'
              : 'border-border text-muted-foreground bg-muted'
          }`}
        >
          {db.role}
        </span>
      </div>
      <div className="flex items-center justify-between text-[11px]">
        <span className="text-[10px] font-black uppercase tracking-[0.2em] text-muted-foreground">
          {db.status ?? db.state ?? '—'}
        </span>
        <span className="font-mono text-foreground">
          {db.host_port ? `:${db.host_port}` : '—'}
        </span>
      </div>
    </Link>
  );
}

function DetailPage() {
  const { name } = useParams({ from: '/db/$name' });
  const navigate = useNavigate();
  const { databases, telemetry, refresh } = useData();
  const db = databases.find((d) => d.name === name);

  if (!db) {
    return (
      <main className="px-8 py-8">
        <button
          onClick={() => navigate({ to: '/' })}
          className="mb-4 text-[10px] font-black uppercase tracking-widest text-muted-foreground hover:text-foreground cursor-pointer bg-transparent border-0"
        >
          ← Databases
        </button>
        <div className="border-2 border-border bg-muted/20 p-6 text-center text-xs font-black uppercase tracking-widest text-muted-foreground">
          {name} not found (refreshing…)
        </div>
      </main>
    );
  }

  return (
    <DatabaseDetail
      db={db}
      databases={databases}
      telemetry={telemetry}
      onBack={() => navigate({ to: '/' })}
      onDone={refresh}
    />
  );
}

// ─── Clone action ──────────────────────────────────────────────────────────────

function NewCloneForm({ onDone }: { onDone: () => Promise<void> }) {
  const [open, setOpen] = useState(false);
  const [from, setFrom] = useState('');
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);

  const submit = async () => {
    if (!from.trim()) return;
    setBusy(true);
    setMsg(null);
    try {
      const res = await api.clone({ from: from.trim() });
      setMsg(res.ok ? 'Clone launched' : `Failed: ${res.stderr.slice(0, 120)}`);
      if (res.ok) {
        setFrom('');
        await onDone();
      }
    } catch (e) {
      setMsg(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  if (!open) {
    return (
      <button
        onClick={() => setOpen(true)}
        className="h-10 px-4 border-2 border-foreground bg-foreground text-background text-xs font-black uppercase tracking-widest cursor-pointer hover:bg-foreground/90"
      >
        Instant Clone
      </button>
    );
  }

  return (
    <div className="flex items-center gap-2">
      <input
        autoFocus
        value={from}
        onChange={(e) => setFrom(e.target.value)}
        placeholder="postgres://user:pass@host:5432/db"
        className="h-10 w-[420px] border-2 border-border bg-muted/40 px-3 text-sm font-mono outline-none focus:border-foreground"
      />
      <button
        disabled={busy}
        onClick={() => void submit()}
        className="h-10 px-4 border-2 border-foreground bg-foreground text-background text-xs font-black uppercase tracking-widest cursor-pointer disabled:opacity-50"
      >
        {busy ? 'Cloning…' : 'Clone'}
      </button>
      <button
        onClick={() => setOpen(false)}
        className="h-10 px-4 border-2 border-border bg-muted text-xs font-black uppercase tracking-widest cursor-pointer"
      >
        Cancel
      </button>
      {msg ? (
        <span className="text-[11px] font-mono text-muted-foreground max-w-[260px] truncate">
          {msg}
        </span>
      ) : null}
    </div>
  );
}

// ─── Router ──────────────────────────────────────────────────────────────────

const rootRoute = createRootRoute({ component: RootLayout });
const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/',
  component: IndexPage,
});
const dbRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/db/$name',
  component: DetailPage,
});

const routeTree = rootRoute.addChildren([indexRoute, dbRoute]);
export const router = createRouter({ routeTree });

declare module '@tanstack/react-router' {
  interface Register {
    router: typeof router;
  }
}
