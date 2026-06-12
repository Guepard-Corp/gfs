# GFS remote mode (guepard-console)

Use the console API instead of `KUBECONFIG` on your laptop. k3s runs only on the data plane via `guepard-node`.

## Flow

```
gfs CLI  →  guepard-console /api/engine/* + /api/databases/*  →  CP :8000  →  DP
```

Call the **console**, not CP directly — it owns Supabase rows, port assignment, and `cpDatabaseId`.

## Multipass quickstart

```bash
source data-platform-v3/infra/multipass/.rendered/dev.env

export GUEPARD_CONSOLE_URL="http://${DP_IP}:32298"   # NodePort, not CP :8000
export GUEPARD_SUPABASE_URL="http://127.0.0.1:18786"
export GUEPARD_SUPABASE_ANON_KEY=<from console server env>

gfs login --email you@example.com --password '...'
# agent/CI:
gfs login --token "$JWT"

export GUEPARD_ENGINE_NODE_ID=<from dev.env or `gfs remote nodes --json`>
gfs init --remote --json "$REPO_DIR" \
  --database-provider postgres --database-version 17 \
  --remote-node "$GUEPARD_ENGINE_NODE_ID"

# lifecycle (note: --path goes on `compute`, before the subcommand)
gfs compute --path "$REPO_DIR" stop
gfs remote destroy --path "$REPO_DIR"
```

`gfs init --remote` polls until the deployment is **running** and returns JSON:

```json
{
  "deployment_id": "<supabase-uuid>",
  "database_id": "<cpDatabaseId>",
  "connection": { ... },
  "status": { "computeStatus": "running" }
}
```

Repo config (`.gfs/config.toml`):

- `runtime_provider = "guepard"`
- `[remote]`: `console_url`, `deployment_id`, `node_id`, `database_id` (CP id)

## Auth

| Method | Command |
|--------|---------|
| Password | `gfs login --email … --password …` |
| Agent/CI | `gfs login --token <jwt>` or `export GUEPARD_ACCESS_TOKEN=…` |

Persisted in `~/.config/guepard/credentials.toml`:

```toml
access_token = "..."
console_url = "http://10.x.x.x:32298"
supabase_url = "http://127.0.0.1:18786"
supabase_anon_key = "..."
```

Global config (same file):

```bash
gfs config --global remote.console_url http://10.x.x.x:32298
gfs remote show --json
gfs remote nodes --json
```

Resolution order: env (`GUEPARD_*`) → credentials file → error.

## Command parity

| Command | Local (docker/k8s) | Remote |
|---------|-------------------|--------|
| `init` | local container | `init --remote` + async deploy poll |
| `status` | container status | deployment status + connection |
| `query` | psql/mysql client | console query API |
| `commit` / `log` / `checkout` | local VCS | deployment-scoped console routes |
| `log --graph` | local graph | CP graph proxy |
| `branch` | local refs | log + checkout create_branch |
| `schema extract` | sidecar | `schema/show` |
| `schema diff` | local objects | console `schema/diff` |
| `compute start/stop` | docker/k8s | deployment start/stop |
| `compute logs` | container logs | not supported |

Local `gfs init` + docker/k8s workflows are **unchanged** — remote is opt-in.

## Agent recipe

```bash
gfs login --token "$TOKEN"
export GUEPARD_CONSOLE_URL=http://<console-host>:32298
gfs init --remote --json --database-provider postgres --database-version 17 \
  --remote-node "$NODE_ID" | tee init.json
gfs commit --json -m "snapshot" --path "$REPO"
```

Human-readable output goes to stderr when `--json` is set; stdout is pure JSON.

## E2E

```bash
export GUEPARD_LOGIN_EMAIL=… GUEPARD_LOGIN_PASSWORD=…
./gfs/scripts/multipass-remote-e2e.sh
```

Verifies VCS with dummy data, pause/resume, autoscaler `:9090/health`, and destroy.

## Env reference

| Variable | Purpose |
|----------|---------|
| `GUEPARD_CONSOLE_URL` | Console base (no trailing `/api`) |
| `GUEPARD_ACCESS_TOKEN` | Bearer JWT |
| `GUEPARD_SUPABASE_URL` + `GUEPARD_SUPABASE_ANON_KEY` | `gfs login` |
| `GUEPARD_ENGINE_NODE_ID` | Default node for `gfs init --remote` |

## Not supported on laptop

- `KUBECONFIG` when `runtime_provider` is `guepard` / `console` / `remote`
- Direct CP `:8000` from gfs CLI
- Interactive `gfs query` in remote mode

Never commit Supabase keys, service role keys, or passwords.
