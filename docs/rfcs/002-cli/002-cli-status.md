# RFC 002: Status (CLI)

## Overview

The **status** reports the current state of a GFS repository and its associated compute instance. It is consumed by the CLI (e.g. `gfs status`) and may be exposed via the node's API. The response is read-only and aggregates repository, config, and compute runtime data.

This RFC defines the **payload** and **semantics** of the status response. Implementation follows the hexagonal architecture: a use case in the domain orchestrates the Repository and Compute ports; adapters (e.g. GFS repository, Docker compute) provide the data.

---

## Status response shape

The status response SHALL include the following top-level fields.

### Repository

| Field | Type | Description |
|-------|------|-------------|
| `current_branch` | string | Name of the branch at HEAD (e.g. `main`, `develop`). When HEAD is detached, a commit hash or a sentinel such as `(detached)` may be used. |

### Compute

| Field | Type | Description |
|-------|------|-------------|
| `provider` | string | Compute/database provider identifier (e.g. `postgresql`, `mysql`). Sourced from repo config (`environment.database_provider`). |
| `version` | string | Database or image version (e.g. `16`, `latest`). Sourced from repo config (`environment.database_version`). |
| `container_status` | string | Current runtime state of the container. One of: `starting`, `running`, `paused`, `stopping`, `stopped`, `restarting`, `failed`, `unknown`. Maps from the domain `InstanceState`. |
| `container_id` | string | Unique identifier of the compute instance (e.g. Docker container ID). Sourced from the Compute port and/or `runtime.container_name` where applicable. |
| `connection_string` | string | Client connection string for the database (e.g. `postgresql://postgres:postgres@localhost:5432/postgres`). Built from provider, host, port, and credentials according to the provider’s convention; see [Connection string](#connection-string) below. |

If no compute has been provisioned yet (e.g. repo just initialised), the **compute** section MAY be omitted or the fields MAY be null/empty with clear semantics (e.g. `container_status: "not_provisioned"` or a dedicated `compute: null`).

### Source (lazy clones only)

Present only when the repository is a lazy clone (`gfs clone --from`) with a running database; **omitted** otherwise (never `null`), so non-clone output is unchanged. This is the clone's **cached** drift verdict: producing it makes no network round trip (`gfs fetch --check` refreshes it). Counts are table-granular -- GFS has no per-row change log (RFC 007).

| Field | Type | Description |
|-------|------|-------------|
| `tracked` | integer | Tables registered for drift tracking (`gfs.clone_source`). `0` means nothing has been compared yet, not "up to date". |
| `behind` | integer | Tables whose rows changed on the source since last synced (`gfs.drift_state.drifted`). |
| `diverged` | integer | Subset of `behind` that also has local writes; `gfs pull` refuses these without `--force`. |
| `last_checked` | string | When the verdict was last computed (`YYYY-MM-DD HH:MM:SS`). Omitted until the first check. Deliberately named `last_checked` (the underlying column is `checked_at`) so `gfs status --output json` and `gfs fetch --json` agree on one field name. |

---

## Connection string

The **connection string** is the canonical way for a client (CLI, app, or control plane) to connect to the database running in the node’s compute instance.

- **Authority**: Host SHALL be the host the client can reach (e.g. `localhost` for same-machine CLI; node hostname or IP when used from another service). Port SHALL be the **host** port (mapped from the container port), e.g. from the Compute port’s instance config or from runtime config.
- **Credentials and database name**: Sourced from the compute definition (e.g. env vars `POSTGRES_USER`, `POSTGRES_PASSWORD`, `POSTGRES_DB`) or from repo/config overrides. No secrets SHALL be logged; the status response may expose the connection string only to authorised callers.
- **Format**: Provider-specific. Examples:
  - PostgreSQL: `postgresql://<user>:<password>@<host>:<port>/<dbname>`
  - MySQL: `mysql://<user>:<password>@<host>:<port>/<dbname>`

The domain use case MAY depend on a small port (e.g. “resolve connection string for instance”) so that building the string stays out of the core domain and is implemented in adapters (e.g. compute-docker) that know the image and env.

---

## Data sources (domain / config)

| Status field | Source |
|--------------|--------|
| `current_branch` | Repository (e.g. HEAD → branch name). May require a new port method such as `get_current_branch(repo)` or use of existing repo layout helpers. |
| `provider` | Repo config: `GfsConfig.environment.database_provider` |
| `version` | Repo config: `GfsConfig.environment.database_version` |
| `container_status` | Compute port: `Compute::status(&id)` → `InstanceStatus.state` |
| `container_id` | Compute port (instance id) and/or `GfsConfig.runtime.container_name` |
| `connection_string` | Derived from provider, host, port (from Compute/runtime), and credentials (compute definition env or config). Prefer a small port for “connection string for instance” so the use case stays adapter-agnostic. |

---

## Use case (domain)

1. **Resolve repo path** (e.g. from CWD or `--path`).
2. **Load repo config** (e.g. `GfsConfig`) to read `environment` and `runtime`.
3. **Current branch**: Call Repository (or repo layout) to get the branch at HEAD.
4. **Compute status**: If a runtime/instance is configured, call `Compute::status(&instance_id)` to get `InstanceStatus` (id, state, etc.).
5. **Connection string**: If applicable, build or resolve the connection string (via port or adapter) from provider, host port, and credentials.
6. **Aggregate** into the status response and return (e.g. JSON or CLI table).

Errors (e.g. repo not found, not a GFS repo, compute not provisioned) SHALL be returned as clear errors; partial data (e.g. branch but no compute) is allowed where defined above.

---

## CLI behaviour (summary)

- **Command**: e.g. `gfs status` (or `gfs branch --current` for branch only; this RFC focuses on the full status payload).
- **Output**: Human-readable table or JSON (e.g. `--output json`) conforming to the status shape above.
- **Scope**: Current directory or `--path <repo>`; must be inside a GFS repo (`.gfs` present and valid).

---

## Out of scope

- **History / log**: Commit history or branch list are not part of this status payload; they remain under other commands/RFCs.
- **Reporting**: Whether and how the node reports this status to an external control plane is a separate concern, out of scope for this RFC.
- **Mutating operations**: Status is read-only; start/stop/restart are separate commands.

---

## Summary

| Item | Decision |
|------|----------|
| Branch | Include `current_branch` from repository/HEAD. |
| Compute | Include `provider`, `version`, `container_status`, `container_id`, `connection_string`. |
| Provider/version | From repo config `environment`. |
| Container status/id | From Compute port and optionally `runtime` config. |
| Connection string | Provider-specific format; built from host port and credentials; consider a small port for resolution. |
