# RFC 002: Export (CLI)

## Overview

The **export** exports data from the running database instance associated with a GFS repository to a file. It is invoked via `gfs export --output-dir <dir> --format <fmt>` and follows the same hexagonal architecture as the rest of GFS: a use case in the domain orchestrates the Compute and DatabaseProvider ports; adapters (Docker compute, GFS repository) satisfy those ports. The export runs an ephemeral sidecar container linked to the database instance, which executes the provider-specific export command (e.g. `pg_dump` for PostgreSQL).

This RFC defines the **command interface**, **behaviour**, and **output** of `gfs export`.

---

## Command interface

```
gfs export --output-dir <dir> --format <fmt> [--path <dir>] [--id <name-or-id>]
```

### Arguments

| Flag | Required | Description |
|------|----------|-------------|
| `--output-dir` | **yes** | Directory where the export file will be written. Created if absent. |
| `--format` | **yes** | Export format identifier. For PostgreSQL: `sql` (plain-text SQL dump) or `custom` (binary dump). |
| `--path` | no | Path to the GFS repository root (directory that contains `.gfs/`). Defaults to the current working directory. |
| `--id` | no | Container name or id override. Defaults to `runtime.container_name` from the repo config. Reserved for future use. |

### Supported formats (PostgreSQL)

| Format | Description | Output file |
|--------|-------------|-------------|
| `sql` | Plain-text SQL dump (pg_dump --format=plain) | `export.sql` |
| `custom` | PostgreSQL custom binary format (pg_dump --format=custom) | `export.dump` |

### Examples

```sh
# Export to current directory as SQL
gfs export --output-dir . --format sql

# Export to a specific directory as custom binary
gfs export --output-dir /backups/my-repo --format custom

# Export from a repo at a specific path
gfs export --path /data/my-repo --output-dir /backups --format sql
```

---

## Behaviour

The export use case executes the following steps in order:

1. **Load config** – reads the repository config at `path` (or cwd) to get `environment.database_provider` and `runtime.container_name`. Fails if not configured.
2. **Resolve provider** – looks up the database provider from the registry (e.g. `postgres`).
3. **Get connection info** – calls `Compute::get_task_connection_info` to obtain the internal host:port the sidecar uses to reach the database instance.
4. **Build export spec** – calls `DatabaseProvider::export_spec` to get the sidecar definition and shell command (e.g. `pg_dump -h ... -p ... -U ... -d ... --format=plain -f /data/export.sql`).
5. **Mount output directory** – sets `host_data_dir` on the sidecar definition to the requested `output_dir`. Creates the directory if absent.
6. **Run sidecar** – calls `Compute::run_task` with the sidecar linked to the database instance. The sidecar runs the export command; the output file is written to the mounted directory.
7. **Return path** – on success, returns the absolute path of the exported file on the host.

---

## Output

On success, `gfs export` prints:

```
Exported to <absolute-path-to-file>
```

Example:

```
Exported to /backups/my-repo/export.sql
```

If the export sidecar writes to stderr (e.g. warnings), that output is printed to stderr after the success message.

On error, the command writes to stderr and exits with a non-zero status code:

```
error: export failed: no database provider configured (run gfs init --database-provider <name>)
error: export failed: no container configured (run gfs compute start)
error: export failed: unsupported export format: 'unknown'
error: export failed: export task failed (exit 1): <stderr>
```

---

## Data sources

| Field | Source |
|-------|--------|
| `path` | `--path` flag → current working directory |
| `output_dir` | `--output-dir` flag |
| `format` | `--format` flag |
| Provider name | `GfsConfig.environment.database_provider` |
| Container name | `GfsConfig.runtime.container_name` |
| Connection info | `Compute::get_task_connection_info` (host, port, env) |

---

## Domain use case

The `ExportRepoUseCase<R>` in `domain/src/usecases/repository/export_repo_usecase.rs` drives the flow. It depends on:

- `Arc<dyn Compute>` — `get_task_connection_info`, `run_task` for the export sidecar.
- `Arc<R: DatabaseProviderRegistry>` — look up the provider to call `export_spec`.

The use case does **no direct file I/O**; the output directory is created via `std::fs::create_dir_all` before running the task. The actual export is performed by the sidecar inside the container.

---

## Error handling

| Condition | Error |
|-----------|-------|
| No database provider configured | `ExportRepoError::NotConfigured` |
| No container configured | `ExportRepoError::NotConfigured` |
| Provider not found | `ExportRepoError::ProviderNotFound` |
| Unsupported format | `ExportRepoError::UnsupportedFormat` |
| Cannot create output dir | `ExportRepoError::Config` |
| Export task failed (non-zero exit) | `ExportRepoError::TaskFailed` |
| Compute error (e.g. Docker) | `ExportRepoError::Compute` |

All errors are printed to stderr as `error: export failed: <message>` and cause exit code `1`.

---

## Platform notes

- **Compute** is implemented by Docker and requires a running Docker daemon.
- The export sidecar uses the same image as the database provider (e.g. `postgres:17`). It must be able to reach the database instance over the same Docker network.

---

## Out of scope

- **Streaming** — export writes to a file; streaming to stdout is not supported.
- **Compression** — output is uncompressed; compression can be added later.
- **Incremental export** — only full exports are supported.
- **Remote export** — export targets a local directory; remote destinations are not addressed.
