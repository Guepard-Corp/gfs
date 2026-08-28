# RFC 002: Checkout (CLI)

## Overview

The **checkout** switches the active branch or commit (HEAD) and updates the active workspace so that the working tree reflects the chosen revision. It is invoked via `gfs checkout <revision>` and follows the same hexagonal architecture as the rest of GFS: a use case in the domain orchestrates the Repository port; the GFS adapter satisfies that port. Optionally, Storage may be involved to materialise snapshot data into the workspace. No Compute is required for the checkout itself (the database may need to be restarted or re-attached to the new workspace by the user or a separate flow).

This RFC defines the **command interface**, **behaviour**, and **output** of `gfs checkout`.

---

## Command interface

```
gfs checkout <revision> [--path <dir>]
gfs checkout -b <branch_name> [<start_revision>] [--path <dir>]
```

### Arguments

| Argument / Flag | Required | Description |
|-----------------|----------|-------------|
| `revision` | **yes** (unless `-b` used) | Branch name or full 64-character commit hash. Examples: `main`, `develop`, `abc123...`. |
| `-b <branch_name>` | no | Create a new branch named `branch_name` and switch to it. When used, `revision` is replaced by an optional `start_revision` (defaults to current HEAD). |
| `start_revision` | no (when `-b` used) | Branch or commit to use as the starting point for the new branch. Defaults to current HEAD. |
| `--path` | no | Path to the GFS repository root (directory that contains `.gfs/`). Defaults to the current working directory. |

### Semantics

| Invocation | Effect |
|------------|--------|
| `gfs checkout <branch>` | Switch to the specified branch (HEAD becomes `ref: refs/heads/<branch>`). The active workspace is set to the tip commit of that branch. |
| `gfs checkout <commit>` | Checkout a specific commit (detached HEAD). HEAD is set to the 64-char commit hash. The active workspace is set to that commit. |
| `gfs checkout -b <branch_name> [<start_revision>]` | Create a new branch `branch_name` at `start_revision` (or current HEAD if omitted), then switch to it. Fails if `branch_name` already exists. |

### Examples

```sh
# Switch to branch develop
gfs checkout develop

# Checkout a specific commit (detached HEAD)
gfs checkout a3f8c1d0000000000000000000000000000000000000000000000000000000000

# Create a new branch 'dev' from current HEAD and switch to it
gfs checkout -b dev

# Create a new branch 'feature/xyz' from branch main and switch to it
gfs checkout -b feature/xyz main

# Create a new branch from a specific commit
gfs checkout -b hotfix a3f8c1d0000000000000000000000000000000000000000000000000000000000

# Checkout in a repo at a specific path
gfs checkout --path /data/my-repo main
gfs checkout -b dev main --path /data/my-repo
```

---

## Behaviour

The checkout use case executes the following steps in order:

1. **Validate** – the repository must exist at the given path. For normal checkout, `revision` must be non-empty; for `-b`, `branch_name` must be non-empty. Otherwise the command exits with a clear error.
2. **Create-branch mode (`-b`)** – if `-b <branch_name>` is used:
   - Resolve `start_revision` (or current HEAD if omitted) via `Repository::rev_parse` to a full commit hash. Fails with `RevisionNotFound` if the start revision does not exist.
   - If `refs/heads/<branch_name>` already exists, exit with an error (e.g. "branch '<branch_name>' already exists").
   - Create the new branch: write `refs/heads/<branch_name>` pointing to the resolved commit hash.
   - Proceed as for a normal checkout of `<branch_name>` (steps 3–6 below, with the new branch as target).
3. **Resolve revision** (normal mode) – call `Repository::rev_parse(repo, revision)` to resolve the revision to a full commit hash. Branch names resolve to the tip of that branch; a full 64-char hash is accepted as-is if the commit exists. Fails with `RevisionNotFound` if the branch or commit does not exist.
4. **Reject initial state** – if the resolved commit is `"0"` (no commits yet on that ref), checkout is not allowed; exit with an error (e.g. "cannot checkout: branch has no commits").
5. **Determine HEAD update** – if the target is a branch (i.e. `refs/heads/<revision>` exists and matches the resolved hash, or we just created it with `-b`), HEAD is set to `ref: refs/heads/<revision>`. Otherwise HEAD is set to the 64-char commit hash (detached HEAD).
6. **Workspace path** – the active workspace for the target commit is `.gfs/workspaces/<branch_or_detached>/<commit_id>/data`, where `branch_or_detached` is the branch name when checking out a branch, or `"detached"` when checking out a commit by hash. The use case (or adapter) must ensure this directory exists and contains the snapshot data for that commit (e.g. by creating it and copying from `.gfs/snapshots/<2>/<62>/` as populated by the storage layer, or by delegating to a storage restore API if available).
7. **Update WORKSPACE** – write the chosen workspace data directory path to `.gfs/WORKSPACE` via the repository layout (e.g. `set_active_workspace_data_dir`). This is the directory that `gfs commit` will snapshot when making the next commit.

The use case does **no direct file I/O**; all I/O is delegated to the Repository adapter (and optionally Storage for materialising snapshots).

---

## Output

On success, `gfs checkout` prints a single human-readable line:

```
Switched to <revision> (<short-hash>)
```

Examples:

```
Switched to develop (a3f8c1d)
Switched to a3f8c1d0000000000000000000000000000000000000000000000000000000000 (a3f8c1d)
```

The short hash is the first 7 characters of the 64-character commit hash.

On success with `-b`, the output may indicate branch creation:

```
Switched to new branch 'dev' (a3f8c1d)
```

On error, the command writes to stderr and exits with a non-zero status code:

```
error: repository not found at '/not/a/repo'
error: revision not found: 'unknown-branch'
error: cannot checkout: branch 'new-branch' has no commits
error: branch 'dev' already exists
```

---

## Data sources

| Field | Source |
|-------|--------|
| `path` | `--path` flag → current working directory |
| `revision` | Positional argument (or `start_revision` when `-b` is used) |
| `branch_name` | `-b` flag argument (new branch to create) |
| Commit hash | `Repository::rev_parse(repo, revision)` (or current HEAD for `-b` when no start_revision) |
| Branch vs detached | `refs/heads/<revision>` exists and equals resolved hash → branch; else detached; with `-b` always branch |
| Workspace path | `.gfs/workspaces/<branch_or_detached>/<commit_id>/data` |
| Snapshot data | Commit object's `snapshot_hash` → `.gfs/snapshots/<2>/<62>/` or Storage restore |

---

## Domain use case

A **CheckoutRepoUseCase** in `domain/src/usecases/repository/checkout_repo_usecase.rs` (or equivalent) drives the flow. It depends on:

- `Arc<dyn Repository>` — `rev_parse`, `checkout` (adapter implements the concrete HEAD + WORKSPACE updates and any snapshot materialisation).

The Repository port already defines `checkout(repo, revision)`. For `-b <branch_name>`, the use case must also create the new branch (e.g. a `create_branch(repo, name, start_commit)` or equivalent that writes `refs/heads/<name>`), then checkout that branch. The GFS adapter currently stubs checkout with "not implemented"; this RFC specifies the contract so the adapter can implement it (using `repo_layout` for HEAD, WORKSPACE, refs, and workspace/snapshot paths).

The use case does **no direct file I/O**; all I/O is in the adapter.

---

## Error handling

| Condition | Error |
|-----------|--------|
| Not a GFS repo | `RepositoryError::NotFound("...")` |
| Revision not found (unknown branch or commit) | `RepositoryError::RevisionNotFound("<revision>")` |
| Resolved commit is `"0"` | Clear message: cannot checkout branch with no commits |
| `-b` and branch name already exists | Clear message: branch '<name>' already exists |

All errors are printed to stderr as `error: <message>` and cause exit code `1`.

---

## Integration with commit and workspace

- **Commit** uses `get_active_workspace_data_dir(repo)` to decide which directory to snapshot. That value is read from `.gfs/WORKSPACE`, which checkout updates. So after `gfs checkout other-branch`, the next `gfs commit` will snapshot the workspace for `other-branch`'s tip.
- **Workspace layout** — `.gfs/WORKSPACE` is the single source of truth for "where is the active workspace." Checkout is the command that switches it when changing branch or commit.

---

## Platform notes

- Checkout is repository-only (refs + WORKSPACE + workspace/snapshot paths). It does not start or stop the database container; the user may need to restart the DB or re-run a provision step to attach the container to the new workspace.

---

## Out of scope

- **Short hash** — revision must be a full 64-char hash or a branch name; short hash resolution (e.g. 7-char) is not required for this RFC.
- **Tags** — only branches and full commit hashes are in scope; tag refs can be added later.
- **Dedicated `gfs branch`** — creating branches is supported via `gfs checkout -b`; a separate `gfs branch` command may be added later for listing/other branch operations.
- **Merge / conflict detection** — not applicable for a single-head checkout.
- **Push / pull** — syncing with a remote is a separate command.
