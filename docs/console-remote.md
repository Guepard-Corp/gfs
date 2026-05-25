# GFS remote mode (guepard-console)

Use the console API instead of `KUBECONFIG` on your laptop. k3s runs only on the data plane via `guepard-node`.

## Flow

```
gfs CLI  →  guepard-console /api/engine/*  →  CP :8000  →  NATS  →  guepard-node (DP)  →  GFS + k3s
```

## Setup

```bash
export GUEPARD_CONSOLE_URL=http://<cp-public-ip>:32298
export GUEPARD_SUPABASE_URL=https://<project>.supabase.co
export GUEPARD_SUPABASE_ANON_KEY=<anon-key>

gfs login --email you@example.com --password '...'
# or: export GUEPARD_ACCESS_TOKEN=<user-api-token>

export GUEPARD_ENGINE_NODE_ID=ip-10-0-1-57-...
gfs init --remote --provider postgres --version 17 --remote-node "$GUEPARD_ENGINE_NODE_ID"
```

Writes `.gfs/config.toml` with `runtime_provider = "guepard"` and a `[remote]` block (`console_url`, `org`, `node_id`, `database_id`).

## Commands (remote repo)

| Command | Behavior |
|---------|----------|
| `gfs commit` | `POST /api/engine/nodes/:nid/databases/:dbId/commit` |
| `gfs log` | `GET .../log` |
| `gfs checkout` | `POST .../checkout` (use `-b` for new branch via API) |
| `gfs init --remote` | `POST /api/engine/deployments` |

## Env

| Variable | Purpose |
|----------|---------|
| `GUEPARD_CONSOLE_URL` | Console base (no trailing `/api`) |
| `GUEPARD_ACCESS_TOKEN` | User API token (Bearer) |
| `GUEPARD_SUPABASE_URL` + `GUEPARD_SUPABASE_ANON_KEY` | For `gfs login` |
| `GUEPARD_ENGINE_NODE_ID` | Default node for `gfs init --remote` |
| `GFS_RUNTIME_PROVIDER=guepard` | Same as `--remote` / `console` / `remote` |

Credentials file: `~/.config/guepard/credentials.toml` (from `gfs login`).

## Not supported on laptop

- `KUBECONFIG` + cluster mutations when `runtime_provider` is `guepard` / `console` / `remote`
- Direct k3s API from developer machine (use SSM on DP for ops/debug)

See [k3s-architecture.md](./k3s-architecture.md) and guepard-console `docs/runbooks/aws-dev-environment-inventory.md`.
