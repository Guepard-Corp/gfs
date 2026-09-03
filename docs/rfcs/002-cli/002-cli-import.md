# RFC 002: Import (CLI)

## Overview

The **import** imports data from a file into the running database instance associated with a GFS repository. It is invoked via `gfs import --file <path> [--format <fmt>]` and follows the same hexagonal architecture as the rest of GFS: a use case in the domain orchestrates the Compute and DatabaseProvider ports; adapters (Docker compute, GFS repository) satisfy those ports. The import runs an ephemeral sidecar container linked to the database instance, which executes the provider-specific import command (e.g. `psql -f` for SQL, `pg_restore` for custom, or `\copy` for CSV in PostgreSQL).

This RFC defines the **command interface**, **behaviour**, and **output** of `gfs import`.

---

## Command interface

```
gfs import --file <path> [--format <fmt>] [--path <dir>] [--id <name-or-id>]
```

### Arguments

| Flag | Required | Description |
|------|----------|-------------|
| `--file` | **yes** | Path to the dump file to import. Must exist and be readable. |
| `--format` | no | Import format identifier. When omitted, inferred from the file extension (`.sql` → `sql`, `.dump` → `custom`, `.csv` → `csv`). |
| `--path` | no | Path to the GFS repository root (directory that contains `.gfs/`). Defaults to the current working directory. |
| `--id` | no | Container name or id override. Defaults to `runtime.container_name` from the repo config. Reserved for future use. |

### Supported formats (PostgreSQL)

| Format | Description |
|--------|-------------|
| `sql` | Plain-text SQL file (loaded via psql -f). Any filename with `.sql` extension. |
| `custom` | PostgreSQL custom binary dump (loaded via pg_restore). Any filename with `.dump` extension. |
| `csv` | CSV file (loaded via psql \copy with HEADER). Any filename with `.csv` extension. |

Any filename is accepted; format is inferred from the file extension when `--format` is omitted.

### Format inference

When `--format` is omitted, the format is inferred from the file extension:

| Extension | Inferred format |
|-----------|-----------------|
| `.sql` | `sql` |
| `.dump` | `custom` |
| `.csv` | `csv` |

If the extension is not recognised, the command fails with a message asking the user to pass `--format` explicitly.

### Examples

```sh
# Import a SQL file (format inferred from .sql extension)
gfs import --file ./backup.sql

# Import a CSV file with explicit format
gfs import --file ./data.csv --format csv

# Import a custom binary dump
gfs import --file ./export.dump --format custom

# Import from a repo at a specific path
gfs import --path /data/my-repo --file /backups/restore.sql
```

---

## Behaviour

The import use case executes the following steps in order:

1. **Resolve input file** – converts the path to absolute (relative to repo path if needed). Fails if the file does not exist.
2. **Resolve format** – uses `--format` if set; otherwise infers from the file extension via `format_from_extension`. Fails if format cannot be determined.
3. **Load config** – reads the repository config at `path` (or cwd) to get `environment.database_provider` and `runtime.container_name`. Fails if not configured.
4. **Resolve provider** – looks up the database provider from the registry (e.g. `postgres`).
5. **Get connection info** – calls `Compute::get_task_connection_info` to obtain the internal host:port the sidecar uses to reach the database instance.
6. **Build import spec** – extracts the file basename from the input path and calls `DatabaseProvider::import_spec` with it. The provider builds the shell command using the actual filename (e.g. `demo-small-en-20170815.sql`), so any filename is supported.
7. **Mount input directory** – sets `host_data_dir` on the sidecar definition to the parent directory of the input file. The file is available at `/data/<basename>` inside the sidecar.
8. **Run sidecar** – calls `Compute::run_task` with the sidecar linked to the database instance. The sidecar runs the import command.
9. **Return output** – on success, returns the absolute path of the imported file.

---

## Output

On success, `gfs import` prints:

```
Imported from <absolute-path-to-file>
```

Example:

```
Imported from /backups/restore.sql
```

If the import sidecar writes to stderr (e.g. warnings), that output is printed to stderr after the success message.

On error, the command writes to stderr and exits with a non-zero status code:

```
error: import failed: no database provider configured (run gfs init --database-provider <name>)
error: import failed: no container configured (run gfs compute start)
error: import failed: input file not found: /path/to/missing.sql
error: import failed: cannot infer format from file extension; pass --format explicitly
error: import failed: unsupported import format: 'unknown'
error: import failed: import task failed (exit 1): <stderr>
```

---

## Data sources

| Field | Source |
|-------|--------|
| `path` | `--path` flag → current working directory |
| `file` | `--file` flag |
| `format` | `--format` flag → `format_from_extension(file)` when omitted |
| Provider name | `GfsConfig.environment.database_provider` |
| Container name | `GfsConfig.runtime.container_name` |
| Connection info | `Compute::get_task_connection_info` (host, port, env) |

---

## Domain use case

The `ImportRepoUseCase<R>` in `domain/src/usecases/repository/import_repo_usecase.rs` drives the flow. It depends on:

- `Arc<dyn Compute>` — `get_task_connection_info`, `run_task` for the import sidecar.
- `Arc<R: DatabaseProviderRegistry>` — look up the provider to call `import_spec`.

The use case does **no direct file I/O**; the input file is accessed by the sidecar via the mounted host directory.

---

## Error handling

| Condition | Error |
|-----------|-------|
| No database provider configured | `ImportRepoError::NotConfigured` |
| No container configured | `ImportRepoError::NotConfigured` |
| Provider not found | `ImportRepoError::ProviderNotFound` |
| Input file not found | `ImportRepoError::FileNotFound` |
| Cannot infer format | `ImportRepoError::UnsupportedFormat` |
| Unsupported format | `ImportRepoError::UnsupportedFormat` |
| Import task failed (non-zero exit) | `ImportRepoError::TaskFailed` |
| Compute error (e.g. Docker) | `ImportRepoError::Compute` |

All errors are printed to stderr as `error: import failed: <message>` and cause exit code `1`.

---

## Platform notes

- **Compute** is implemented by Docker and requires a running Docker daemon.
- The import sidecar uses the same image as the database provider (e.g. `postgres:17`). It must be able to reach the database instance over the same Docker network.
- For **CSV** format, the PostgreSQL provider creates a table `csv_import` with columns `(id text, name text)` and loads the file via `\copy`. The CSV schema is fixed for this format; custom schemas are not supported.

---

## Out of scope

- **Streaming** — import reads from a file; no stdin support.
- **Partial import** — only full file imports are supported.
- **Schema customisation** — CSV format uses a fixed table schema; user-defined schemas are not addressed.
- **Remote import** — import is from a local file.
