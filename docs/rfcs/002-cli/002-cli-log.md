# RFC 002: Log (CLI)

## Overview

The **log** displays commit history reachable from HEAD (or a given revision). It is invoked via `gfs log` and follows the same hexagonal architecture as the rest of GFS: a use case in the domain orchestrates the Repository port; the GFS adapter satisfies that port. No Compute or Storage is involved.

This RFC defines the **command interface**, **behaviour**, and **output** of `gfs log`.

---

## Command interface

```
gfs log [--path <dir>] [-n | --max-count <n>] [--from <revision>] [--until <revision>]
```

### Arguments

| Flag | Required | Description |
|------|----------|-------------|
| `--path` | no | Path to the GFS repository root (directory that contains `.gfs/`). Defaults to the current working directory. |
| `-n`, `--max-count` | no | Limit the number of commits to display. |
| `--from` | no | Start traversal at this revision instead of HEAD. Accepts branch name or full 64-char hash. |
| `--until` | no | Stop before this revision (exclusive). Accepts branch name or full 64-char hash. |

### Examples

```sh
# Log from current directory (HEAD)
gfs log

# Log a repo at a specific path
gfs log --path /data/my-repo

# Show only the 5 most recent commits
gfs log -n 5

# Log starting from a specific branch
gfs log --from develop

# Log until a specific commit (exclusive)
gfs log --until 7b2e91f0000000000000000000000000000000000000000000000000000000000
```

---

## Behaviour

The log use case executes the following steps:

1. **Resolve start** – if `--from` is given, resolve it via `rev_parse` (branch name or full hash); otherwise use `get_current_commit_id` (HEAD).
2. **Empty repo** – if the start commit is `"0"` (no commits yet), output nothing and exit successfully.
3. **Traverse** – walk the parent chain from the start commit, following the first parent only. Stop when:
   - `--until` is reached (exclusive),
   - `--max-count` commits have been collected,
   - or there is no parent (or parent is `"0"`).
4. **Output** – for each commit, print a git-style block (see Output section).

The use case does **no direct file I/O**; all I/O is delegated to the Repository adapter.

---

## Output

On success, `gfs log` prints a git-style multi-line block per commit, newest first:

- `commit <full-64-char-hash> (<refs>)` — refs are comma-separated (e.g. `HEAD -> main`, `main`).
- `Author: <name> <<email>>` — from commit author and author_email (email omitted if absent).
- `Date:   <date>` — author date in human-readable form (e.g. `Sat Jun 7 16:29:37 2025 +0200`).
- Blank line, then the commit message indented with 4 spaces: `    <message>`.
- Blank line between commits.

**Example** (one commit on `main`):

```
commit 35e33b3502418b23f30362227528a3f7a6815fc9 (HEAD -> main, main)
Author: Hani CHALOUATI <hani.chalouati@gmail.com>
Date:   Sat Jun 7 16:29:37 2025 +0200

    feat: mcp
```

**Example** (multiple commits, newest first):

```
commit a3f8c1d0000000000000000000000000000000000000000000000000000000000 (HEAD -> main, main)
Author: Alice <alice@example.com>
Date:   Sat Feb 21 10:00:00 2026 +0000

    add index on orders

commit 7b2e91f0000000000000000000000000000000000000000000000000000000000
Author: Alice <alice@example.com>
Date:   Fri Feb 20 09:00:00 2026 +0000

    initial schema
```

On error, the command writes to stderr and exits with a non-zero status code:

```
error: repository not found at '/not/a/repo'
error: revision not found: 'unknown-branch'
```

---

## Data sources

| Field | Source |
|-------|--------|
| `path` | `--path` flag → current working directory |
| `from` | `--from` flag → resolved via `rev_parse` |
| `until` | `--until` flag → resolved via `rev_parse` |
| `limit` | `-n` / `--max-count` flag |
| Commit data | `Repository::log` (message, author, author_email, author_date, refs) |

---

## Domain use case

The `LogRepoUseCase` in `domain/src/usecases/repository/log_repo_usecase.rs` drives the flow. It depends on:

- `Arc<dyn Repository>` — reads commit history via `log(repo, options)`.

The use case does **no direct file I/O**; all I/O is in the adapter.

---

## Error handling

| Condition | Error |
|-----------|-------|
| Not a GFS repo | `LogRepoError::Repository(RepositoryError::NotFound(...))` |
| Invalid `--from` or `--until` | `LogRepoError::Repository(RepositoryError::RevisionNotFound(...))` |

All errors are printed to stderr as `error: <message>` and cause exit code `1`.

---

## Out of scope

- **Short-hash resolution** — `--from` and `--until` accept only full 64-char hash or branch name.
- **Graph / pretty format** — no ASCII graph or additional formatting.
- **Remotes** — remote refs (e.g. `origin/main`) are not displayed.
