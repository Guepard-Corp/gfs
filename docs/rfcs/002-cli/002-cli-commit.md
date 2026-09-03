# RFC 002: Commit (CLI)

## Overview

The **commit** records a point-in-time snapshot of a GFS repository and its associated database container. It is invoked via `gfs commit -m <message>` and follows the same hexagonal architecture as the rest of GFS: a use case in the domain orchestrates the Repository, Compute, and Storage ports; adapters (GFS repository, Docker compute, APFS storage) satisfy those ports.

This RFC defines the **command interface**, **behaviour**, and **output** of `gfs commit`.

---

## Command interface

```
gfs commit -m <message> [--path <dir>] [--author <name>] [--author-email <email>]
```

### Arguments

| Flag | Required | Description |
|------|----------|-------------|
| `-m`, `--message` | **yes** | Commit message. Must be non-empty. |
| `--path` | no | Path to the GFS repository root (directory that contains `.gfs/`). Defaults to the current working directory. |
| `--author` | no | Override the author name. Falls back through: repo config → global config → git config → `"user"`. |
| `--author-email` | no | Override the author e-mail. Falls back through: repo config → global config → git config. |

The **committer** is set to the same value as the author (same name/email) unless overridden at the use-case level.

### Examples

```sh
# Commit from the current directory
gfs commit -m "initial schema"

# Commit a repo at a specific path
gfs commit --path /data/my-repo -m "add index on orders"

# Commit with explicit author
gfs commit -m "fix constraint" --author "Alice" --author-email "alice@example.com"
```

---

## Behaviour

The commit use case executes the following steps in order:

1. **Validate** – the commit message must be non-empty; otherwise the command exits with a clear error.
2. **Resolve context** – reads the current branch name, parent commit id, runtime config (container name), environment config (database provider / version), mount point, and user config from the repo.
3. **Prepare database container** *(if a database is provisioned)*:
   - Calls `DatabaseProvider::prepare_for_snapshot()` so the provider can flush/checkpoint (e.g. `CHECKPOINT` in PostgreSQL).
   - Calls `Compute::prepare_for_snapshot()` to quiesce container-level I/O.
   - Pauses the container (`Compute::pause`) if its state is `Running`.
4. **Snapshot** – calls `StoragePort::snapshot` on the workspace volume (identified by the configured mount point, or the workspace data path as fallback). The returned `SnapshotId` becomes the commit's `snapshot_hash`.
5. **Commit object** – builds a `NewCommit` (message, author, committer, snapshot hash, parent(s)) and calls `Repository::commit`. The adapter hashes the commit content with SHA-256, writes the object under `.gfs/objects/<2>/<rest>` as JSON, and advances `refs/heads/<branch>` to the new hash.
6. **Unpause** – if the container was paused in step 3, it is resumed with `Compute::unpause`.

---

## Output

On success, `gfs commit` prints a single human-readable line:

```
[<branch>] <short-hash>  <message>
```

Example:

```
[main] a3f8c1d  initial schema
```

The short hash is the first 7 characters of the 64-character SHA-256 commit hash.

On error, the command writes to stderr and exits with a non-zero status code:

```
error: commit message must not be empty
error: repository error: repository not found at '/not/a/repo'
error: storage error: volume not found: '/mnt/my-vol'
```

---

## Data sources

| Field | Source |
|-------|--------|
| `message` | `--message` / `-m` CLI flag |
| `author` | `--author` flag → `user.name` in `.gfs/config.toml` → `user.name` in `~/.gfs/config.toml` → `git config user.name` → `"user"` |
| `author_email` | `--author-email` flag → `user.email` in `.gfs/config.toml` → `user.email` in `~/.gfs/config.toml` → `git config user.email` |
| `committer` / `committer_email` | Same as author |
| `snapshot_hash` | Returned `SnapshotId.0` from `StoragePort::snapshot` |
| `parents` | Current commit id read from `Repository::get_current_commit_id`; `None` when `"0"` (initial commit) |
| `branch` | `Repository::get_current_branch` (used to advance the branch ref) |
| Container id | `GfsConfig.runtime.container_name` |
| Volume id | `GfsConfig.mount_point` → workspace data path fallback |
| Database provider | `GfsConfig.environment.database_provider` |

---

## Domain use case

The `CommitRepoUseCase<R>` in `domain/src/usecases/repository/commit_repo_usecase.rs` drives the flow. It depends on:

- `Arc<dyn Repository>` — reads context and persists the commit object + branch ref.
- `Arc<dyn Compute>` — prepare / pause / unpause the container.
- `Arc<dyn StoragePort>` — snapshot the workspace volume.
- `Arc<R: DatabaseProviderRegistry>` — look up the provider to call `prepare_for_snapshot`.

The use case does **no direct file I/O**; all I/O is in adapters.

---

## Error handling

| Condition | Error |
|-----------|-------|
| Empty message | `CommitRepoError::EmptyMessage` |
| Not a GFS repo | `CommitRepoError::Repository(RepositoryError::Internal("..."))` |
| Unknown DB provider | `CommitRepoError::UnknownDatabaseProvider("<name>")` |
| Container not found | `CommitRepoError::Compute(ComputeError::NotFound(...))` |
| Volume not found | `CommitRepoError::Storage(StorageError::NotFound(...))` |

All errors are printed to stderr as `error: <message>` and cause exit code `1`.

---

## Platform notes

- **Storage** (`StoragePort`) is currently implemented by APFS on macOS. On other platforms, the commit command will fail with a clear "not supported" message until a portable storage adapter is provided.
- **Compute** is implemented by Docker and requires a running Docker daemon.

---

## Out of scope

- **Branching** — creating branches or switching branches is a separate command/RFC.
- **Merge commits** — multiple parents are not covered by this CLI command.
- **Signing** — commit signing is not addressed here.
- **Push / pull** — uploading the new commit to a remote is a separate command.
