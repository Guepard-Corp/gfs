# RFC 002: Compute (CLI)

## Overview

The **compute** subcommands manage the lifecycle and inspection of a GFS database compute instance (e.g. a Docker container running PostgreSQL). They are invoked via `gfs compute [--path <dir>] <action> [--id <instance>]` and delegate to the **Compute** port; the CLI adapter uses the Docker compute implementation. The instance is identified by `--id` when given; otherwise it is resolved from the repository at `--path` (or the current directory) as `runtime.container_name` in `.gfs/config.toml`.

This RFC defines the **command interface**, **behaviour**, and **output** of `gfs compute`.

---

## Command interface

The instance is the database container (or equivalent) managed by the Compute port. It is identified either by `--id` or by the repository config when run from a GFS repo (or with `--path`).

```
gfs compute [--path <dir>] status   [--id <name-or-id>]
gfs compute [--path <dir>] start    [--id <name-or-id>]
gfs compute [--path <dir>] stop     [--id <name-or-id>]
gfs compute [--path <dir>] restart [--id <name-or-id>]
gfs compute [--path <dir>] pause    [--id <name-or-id>]
gfs compute [--path <dir>] unpause  [--id <name-or-id>]
gfs compute [--path <dir>] logs     [--id <name-or-id>] [--tail N] [--since <RFC3339>] [--no-stdout] [--no-stderr]
```

### Arguments (common)

| Flag | Required | Description |
|------|----------|-------------|
| `--path` | no | Path to the GFS repository root. Defaults to the current working directory. When `--id` is omitted, the instance is taken from `runtime.container_name` in `.gfs/config.toml` at this path. |
| `--id` | no* | Container name or id. When omitted, the instance is resolved from the repo config (`runtime.container_name`) at `--path` or cwd. *Either `--id` or a valid repo (with `runtime.container_name` set) is required. |

### Arguments (per action)

| Action | Flag | Description |
|--------|------|-------------|
| `logs` | `--tail N` | Return at most N most-recent lines; default is all. |
| `logs` | `--since <RFC3339>` | Only return entries after this timestamp. |
| `logs` | `--no-stdout` | Exclude stdout from log output. |
| `logs` | `--no-stderr` | Exclude stderr from log output. |

### Examples

```sh
# From inside a GFS repo: use container from .gfs/config.toml (no --id needed)
gfs compute status
gfs compute start
gfs compute logs --tail 100

# Explicit repo path (id from that repo's config)
gfs compute --path /data/my-repo status

# Explicit container (overrides config)
gfs compute status --id my-repo-db
gfs compute start   --id my-repo-db
gfs compute stop    --id my-repo-db
gfs compute restart --id my-repo-db
gfs compute pause   --id my-repo-db
gfs compute unpause --id my-repo-db
gfs compute logs    --id my-repo-db --since 2025-02-21T00:00:00Z
```

---

## Summary of subcommands

| Command | Description | Example |
|---------|-------------|---------|
| `gfs compute status` | Show the current status of the compute instance (id, state, pid, started_at, exit_code). | `gfs compute status --id my-db` |
| `gfs compute start` | Start the compute instance. | `gfs compute start --id my-db` |
| `gfs compute stop` | Gracefully stop the compute instance. | `gfs compute stop --id my-db` |
| `gfs compute restart` | Stop then start the compute instance. | `gfs compute restart --id my-db` |
| `gfs compute pause` | Suspend the instance (e.g. cgroups freezer). | `gfs compute pause --id my-db` |
| `gfs compute unpause` | Resume a paused instance. | `gfs compute unpause --id my-db` |
| `gfs compute logs` | Stream or fetch log entries from the instance. | `gfs compute logs --id my-db --tail 50` |

---

## Behaviour

- **status** — Calls `Compute::status(&id)`. Returns current runtime state only; no repository or connection-string resolution (see `gfs status` for aggregated repo + compute info).
- **start** — Calls `Compute::start(&id, options)`. Fails if the instance is already running (`AlreadyRunning`) or not found (`NotFound`).
- **stop** — Calls `Compute::stop(&id)`. Graceful shutdown; fails if not running or not found.
- **restart** — Calls `Compute::restart(&id)`. Equivalent to stop then start.
- **pause** — Calls `Compute::pause(&id)`. Fails if already paused or not running.
- **unpause** — Calls `Compute::unpause(&id)`. Fails if not paused.
- **logs** — Calls `Compute::logs(&id, options)` with `LogsOptions` built from `--tail`, `--since`, `--no-stdout`, `--no-stderr`. Output is ordered by timestamp.

The CLI does **not** run a domain use case for these actions; it invokes the Compute port (via the Docker adapter) directly. When `--id` is omitted, the CLI resolves the instance id from the repository at `--path` (or cwd) by loading `.gfs/config.toml` and using `runtime.container_name`.

---

## Output

### Status, start, stop, restart, pause, unpause

On success, the command prints a block of key-value lines:

```
id          : <instance-id>
state       : <starting|running|paused|stopping|stopped|restarting|failed|unknown>
pid         : <pid>           (if available)
started_at  : <RFC3339>       (if available)
exit_code   : <code>          (if available, e.g. after stop)
```

Example:

```
id          : abc123def456
state       : running
pid         : 12345
started_at  : 2025-02-21T10:00:00Z
```

### Logs

Each log entry is printed as a single line:

```
[<RFC3339>] [stdout|stderr] <message>
```

Example:

```
[2025-02-21T10:01:00Z] [stdout] database system is ready to accept connections
```

On error, the command writes to stderr and exits with a non-zero status code, e.g.:

```
error: instance not found: 'my-db'
error: instance is already running: 'my-db'
error: instance is not paused: 'my-db'
```

---

## Data sources

| Field | Source |
|-------|--------|
| Instance id | `--id` CLI argument when set; otherwise `GfsConfig.runtime.container_name` from the repo at `--path` or current directory. |
| Repo path | `--path` or current working directory. |
| Logs options | `--tail`, `--since`, `--no-stdout`, `--no-stderr` mapped to `LogsOptions`. |

---

## Domain port

The **Compute** port (`domain/src/ports/compute.rs`) defines:

- `status`, `start`, `stop`, `restart`, `pause`, `unpause`, `logs`
- Types: `InstanceId`, `InstanceStatus`, `InstanceState`, `LogsOptions`, `LogEntry`, `LogStream`

The CLI uses the **Docker compute** adapter (`compute-docker`). All I/O and runtime interaction are in the adapter; the CLI only forwards arguments and prints results.

---

## Error handling

Errors from the Compute port are mapped to user-visible messages and non-zero exit. Typical conditions:

| Condition | ComputeError | CLI message (example) |
|-----------|--------------|------------------------|
| Instance not found | `NotFound(id)` | `error: instance not found: '<id>'` |
| Already running | `AlreadyRunning(id)` | `error: instance is already running: '<id>'` |
| Not running | `NotRunning(id)` | `error: instance is not running: '<id>'` |
| Already paused | `AlreadyPaused(id)` | `error: instance is already paused: '<id>'` |
| Not paused | `NotPaused(id)` | `error: instance is not paused: '<id>'` |
| Docker daemon unavailable | (adapter) | `error: failed to connect to Docker daemon: ...` |
| Invalid `--since` | (CLI) | `error: invalid --since timestamp: ...` |
| No repo / not a GFS repo | (resolve_id) | `error: not a gfs repository (use --path <repo> or run from a repo)` |
| No container_name in config | (resolve_id) | `error: no container_name in repo config (set runtime.container_name or pass --id)` |

Exit code on error: non-zero (e.g. `1`).

---

## Platform notes

- **Compute** is implemented by the Docker adapter. A running Docker daemon is required; otherwise the command fails with a connection error.
- **Provisioning** (creating the instance from a definition) is done during `gfs init` or equivalent; `gfs compute` does not expose a `provision` subcommand in this RFC.

---

## Out of scope

- **`start --wait`** — Waiting until the instance reaches `Running` is supported by the Compute port's `StartOptions` but not yet exposed in the CLI.
- **Provision** — Creating a new instance from a `ComputeDefinition` remains part of init (or a dedicated flow); not a `gfs compute` subcommand here.
- **Connection string** — For a human-readable connection string and repo-level status, use `gfs status` (see RFC 002 status).
- **Prepare for snapshot** — Used internally by `gfs commit`; not exposed as a standalone compute command.
