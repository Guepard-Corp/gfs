export type GfsDatabase = {
  container_id: string;
  name: string;
  role: string;
  provider: string | null;
  provider_version: string | null;
  repo: string | null;
  remote: string | null;
  image: string | null;
  state: string | null;
  status: string | null;
  host_port: number | null;
  created: number | null;
};

export type Metric = {
  name: string;
  labels: Record<string, string>;
  value: number;
};

export type Telemetry = {
  proxy_url: string;
  reachable: boolean;
  error: string | null;
  metrics: Metric[];
  clones: { clones?: Array<Record<string, unknown>> };
};

export type ActionResult = {
  ok: boolean;
  exit_code: number | null;
  output: unknown;
  stderr: string;
};

// Mirrors gfs_domain::model::datasource (reused, not redefined).
export type DsTable = {
  schema: string;
  name: string;
  bytes: number;
  size: string;
  live_rows_estimate: number;
  dead_rows_estimate: number;
};

export type DsSchema = { id: number; name: string; owner: string };

export type DatasourceMetadata = {
  version: string;
  driver: string;
  schemas: DsSchema[];
  tables: DsTable[];
};

export type SchemaSnapshot = {
  reachable: boolean;
  error: string | null;
  metadata: DatasourceMetadata | null;
};

async function json<T>(res: Response): Promise<T> {
  if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
  return res.json() as Promise<T>;
}

export const api = {
  databases: () => fetch('/api/databases').then(json<GfsDatabase[]>),
  clones: () => fetch('/api/clones').then(json<GfsDatabase[]>),
  telemetry: () => fetch('/api/telemetry').then(json<Telemetry>),
  schema: (name: string) =>
    fetch(`/api/databases/${encodeURIComponent(name)}/schema`).then(json<SchemaSnapshot>),
  clone: (body: { from: string; path?: string; port?: number }) =>
    fetch('/api/actions/clone', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    }).then(json<ActionResult>),
  compute: (body: { repo: string; action: 'start' | 'stop' | 'restart' }) =>
    fetch('/api/actions/compute', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    }).then(json<ActionResult>),
};

/** Sum a counter family across label permutations. */
export function metricTotal(metrics: Metric[], name: string): number {
  return metrics
    .filter((m) => m.name === name)
    .reduce((acc, m) => acc + m.value, 0);
}
