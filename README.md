![Guepard](/resources/guepard-cover.png)

<div align="center">
    <h1>Git For database Systems</h1>
    <p><strong>Safe database version control for AI coding agents and developers.</strong></p>
    <br />
    <p align="center">
    <a href="https://youtu.be/WlOkLnoY2h8?si=hb6-7kLhlOvVL1u6">
        <img src="https://img.shields.io/badge/Watch-YouTube-%23ffcb51?logo=youtube&logoColor=black" alt="Watch on YouTube" />
    </a>
    <a href="https://discord.gg/SEdZuJbc5V">
        <img src="https://img.shields.io/badge/Join-Community-%23ffcb51?logo=discord&logoColor=black" alt="Join our Community" />
    </a>
    <a href="https://github.com/Guepard-Corp/gfs/actions/workflows/main.yml" target="_blank">
        <img src="https://img.shields.io/github/actions/workflow/status/Guepard-Corp/gfs/main.yml?branch=main" alt="Build">
    </a>
    <a href="https://github.com/Guepard-Corp/gfs/blob/main/LICENSE" target="_blank">
        <img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License" />
    </a>
    <a href="https://github.com/Guepard-Corp/gfs/pulls" target="_blank">
        <img src="https://img.shields.io/badge/PRs-welcome-brightgreen.svg" alt="PRs Welcome" />
    </a>
    <a href="https://www.bestpractices.dev/projects/12172"><img src="https://www.bestpractices.dev/projects/12172/badge"></a>
    <a href="https://scorecard.dev/viewer/?uri=github.com/Guepard-Corp/gfs"><img src="https://api.scorecard.dev/projects/github.com/Guepard-Corp/gfs/badge"></a>
    </p>
    <p>Works with Claude Code, Cursor, Cline, Windsurf, and any skills / MCP-compatible agent</p>
    <img src="resources/GFSShowcase.gif" alt="GFS Showcase" />
</div>

## Table of Contents

- [Important Notice](#important-notice)
- [What is GFS?](#what-is-gfs)
- [Built for AI Agents](#built-for-ai-agents)
- [Supported Databases](#supported-databases)
- [Features](#features)
- [Installation](#installation)
- [Quick Start](#quick-start)
- [AI Agent Setup](#ai-agent-setup)
- [MCP Server](#mcp-server)
- [Command Reference](#command-reference)
- [Configuration](#configuration)
- [Troubleshooting](#troubleshooting)
- [Development](#development)
- [Known limitations](#known-limitations)
- [Contributing](#contributing)
- [Community](#community)
- [Roadmap](#roadmap)
- [License](#license)

## Important Notice

This project is under active development. Expect changes, incomplete features, and evolving APIs.

## What is GFS?

GFS (Git For database Systems) brings Git-like version control to your databases. It enables you to:

- **Safe for AI agents**: automatic snapshots protect against agent mistakes and data loss
- **Rollback instantly**: undo any database change in seconds
- **Branch** to let agents and developers experiment without risking data
- **Time travel** through your database history
- **Commit** database states with meaningful messages
- **Collaborate**: agents and humans working on the same database with confidence

GFS uses Docker to manage isolated database environments, making it easy to work with different versions of your database without conflicts.

## Built for AI Agents

AI coding agents are powerful but dangerous around databases. A single bad migration, a dropped table, or corrupted data can be costly to recover from, if recovery is even possible.

GFS makes agent-driven database work safe by default:

- **Every change is a commit.** If an agent makes a mistake, roll back in one command.
- **Branches are free.** Let agents experiment on an isolated branch, then merge only what works.
- **MCP integration.** Agents interact with GFS natively through the Model Context Protocol, no shell wrappers needed.
- **Less token waste.** Import, export, and query operations run through GFS instead of the agent generating boilerplate SQL.

**Without GFS:** an agent drops a table or runs a bad migration, and you're left manually restoring from backups (if they exist).

**With GFS:** `gfs checkout HEAD~1`. Done. Your database is back to the previous state in seconds.

## Supported Databases

- **PostgreSQL** (versions 13-18)
- **MySQL** (versions 8.0-8.1)
- **SQLite** (version 3) — embedded, so no container runtime is required

Run `gfs providers` to see all available providers and their supported versions.

## Features

- Initialize database repositories
- Commit database changes
- View commit history
- Checkout previous commits
- Create and switch branches
- Check database status
- Query database directly from CLI (SQL execution and interactive mode)
- Schema extraction, show, and diff between commits
- Export and import data (SQL, custom, CSV)
- Lazily clone a remote PostgreSQL database (copy-on-read, experimental), with
  detection of source changes so a clone never serves stale rows
- Compute container management (start, stop, logs)
- Repository config (user.name, user.email)

## Installation

```bash
curl -fsSL https://gfs.guepard.run/install | bash
```

## Quick Start

### 1. Check available database providers

```bash
gfs providers
```

This shows all supported database providers and their versions.

### 2. Create a new project directory

```bash
mkdir my_project
cd my_project
```

### 3. Initialize the repository

```bash
gfs init --database-provider postgres --database-version 17
```

This creates a `.gfs` directory and starts a PostgreSQL database in a Docker container.

### 4. Check status

```bash
gfs status
```

This shows the current state of your storage and compute resources.

### 5. Query your database

```bash
# Execute a SQL query directly
gfs query "SELECT 1"

# Or open an interactive terminal session
gfs query
```

### 6. Make changes and commit

```bash
gfs query "CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT NOT NULL);"
gfs query "INSERT INTO users (name) VALUES ('Alice'), ('Bob');"
gfs commit -m "Add users table"
```

### 7. View commit history

```bash
gfs log
```

### 8. Time travel through history

```bash
gfs checkout <commit_hash>
```

Your database will be restored to that exact state.

### 9. Work with branches

```bash
gfs checkout -b feature-branch   # Create and switch to a new branch
gfs checkout main                # Switch back to main
```

## AI Agent Setup

Connect your AI agent to GFS in under a minute.

### Claude Code

GFS works with Claude Code out of the box via MCP:

```bash
claude mcp add gfs -- gfs mcp --path /path/to/your/repo
```

### Claude Desktop

Add to your Claude Desktop configuration (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "gfs": {
      "command": "gfs",
      "args": ["mcp", "--path", "/path/to/your/repo"]
    }
  }
}
```

Restart Claude Desktop and GFS operations will be available as tools.

### Cursor / Cline / Windsurf

Use the stdio MCP server:

```bash
gfs mcp --path /path/to/your/repo
```

Configure your editor's MCP settings to point to this command. Refer to your editor's MCP documentation for the exact configuration format.

### What agents can do with GFS

Once connected, your AI agent can:

- **Commit** before and after making changes, creating safe checkpoints
- **Branch** to try risky migrations without affecting the main database
- **Roll back** if something goes wrong
- **Query** the database to inspect data
- **Diff schemas** between commits to understand what changed
- **Import/export** data without generating large SQL blocks in context

## MCP Server

GFS includes a Model Context Protocol (MCP) server for programmatic access to all GFS operations.

### Stdio mode (default)

```bash
gfs mcp
# or explicitly
gfs mcp stdio
```

Designed for direct integration with MCP-compatible clients.

### HTTP mode

```bash
# Start as a background daemon
gfs mcp start

# Check daemon status
gfs mcp status

# Stop the daemon
gfs mcp stop

# Start in foreground (default port: 3000)
gfs mcp web

# Custom port
gfs mcp web --port 8080
```

### Specifying a Repository Path

```bash
gfs mcp --path /path/to/repo
```

## Command Reference

### Revision References

GFS supports Git-style revision notation for referencing commits in commands like `checkout`, `schema show`, and `schema diff`:

- `HEAD` - Current commit
- `main` - Branch tip (any branch name)
- `abc123...` - Full commit hash (64 characters)
- `HEAD~1` - Parent of HEAD (previous commit)
- `HEAD~5` - 5th ancestor of HEAD
- `main~3` - 3 commits before main branch tip

Examples:
```bash
gfs checkout HEAD~1                    # Checkout previous commit
gfs schema diff HEAD~5 HEAD           # Compare schema with 5 commits ago
gfs schema show main~3                # View schema from 3 commits back
```

### `gfs providers`

List available database providers and their supported versions.

```bash
gfs providers
gfs providers postgres    # Show details for a specific provider
```

### `gfs init`

Initialize a new GFS repository.

```bash
gfs init --database-provider <provider> --database-version <version>
```

### `gfs clone` (experimental)

Lazily clone a remote PostgreSQL database (copy-on-read). Only the schema is
mirrored up front; reads are served live from the remote until rows are written
locally, and writes stay local so the clone diverges. The remote is read-only.

```bash
gfs clone --from 'postgres://user:password@host:5432/dbname' [PATH]
```

Options: `--database-version` (else matched to the remote), `--image` (e.g.
`pgvector/pgvector:pg16` when the source uses an extension), `--platform` (e.g.
`linux/amd64`), `--port`. Add `?schema=a,b` to the URL to mirror specific
schemas. Cloned tables are views: plain CRUD works unchanged, but DDL and
`SELECT … FOR UPDATE` are not supported on them. Quote the URL if the password
contains shell metacharacters.

#### When the source changes

A clone copies each table the first time you read it. If the source changes that
table afterwards, the clone notices and reads from the source instead, so a query
never returns rows that no longer exist upstream:

| situation | what you get | speed |
| --- | --- | --- |
| source unchanged | the local copy (provably identical to the source) | fast, no network |
| source changed | read from the source | slower |
| source changed **and** you wrote to that table | **your** version, plus a conflict warning | — |

**Correct data is automatic.** You never have to run anything to avoid stale
results; the last row is a conflict, and GFS always keeps your local writes
rather than silently discarding them.

Inspect what moved:

```bash
# has the source been written to at all? (a "false" is a guarantee it has not)
gfs query "SELECT gfs.source_changed();"

# which tables changed, and how
gfs query "SELECT * FROM gfs.source_drift();"
```

Reading from the source is correct but slower. To go fast again, put the changed
tables back on the lazy path so the next read re-copies them:

```bash
gfs query "SELECT * FROM gfs.pull();"
```

`pull` copies nothing itself: it clears the cached state and the normal
cost model decides again on the next read (small table → copy, huge table → keep
reading from the source). Tables you have written to are never reset, since that
would discard your work; they are reported as conflicts for you to resolve.

To have that happen on its own, without running anything:

```bash
gfs query "UPDATE gfs.sync_policy SET autopull = true;"
```

With autopull on, a changed table costs **one** query answered from the source,
then it is re-copied in the background and later reads are local again. It is
**off by default** because a clone is a branch: data shifting under a running
test breaks reproducibility. For a read-only clone (analytics, dashboards) it is
usually the better setting.


##### All source-sync settings

Everything above is stored in one row of `gfs.sync_policy`:

```bash
gfs query "SELECT * FROM gfs.sync_policy;"
```

| setting | default | what it controls |
| --- | --- | --- |
| `autopull` | `false` | after a changed table costs one federated read, re-copy it in the background so later reads are local |
| `autoschema` | `false` | apply **additive** source schema changes automatically (a new column, a new partition). Never destructive: a column drop stays a conflict for a human |
| `check_interval` | `5 min` | how stale a drift verdict may be before a read triggers a fresh background check |
| `autopull_interval` | `1 min` | minimum gap between background re-copies of the same table |
| `autopull_max_bytes` | (see table) | ceiling on how much data background work will re-copy unattended |

Both automatic behaviours are **off by default** because a clone is a branch: data
shifting under a running test breaks reproducibility. For a read-only clone
(analytics, dashboards) turning them on is usually the better choice.

> A clone is **not** a point-in-time snapshot of a changing source. Because
> tables are copied at different moments, a clone of a source that is still being
> written to can hold a combination of rows that never existed there at one
> instant. Clone from a frozen source (a storage snapshot, backup, or paused
> replica) if you need reproducibility.

##### New partitions, inherited children and materialized views

Most source changes announce themselves: a changed column raises a clear error, a
changed row set reads from the source. A **new partition** is the quiet one. It is
reached through a parent the clone already has, so the query still succeeds and
simply returns fewer rows than the source holds.

`gfs pull` handles all three:

| what changed on the source | what `pull` does |
| --- | --- |
| a new partition of a partitioned table | creates it locally with the source's bound and registers it for copy-on-read |
| a new `INHERITS` child | creates it under the same parent, with its own key, and registers it |
| a materialized view refreshed upstream | recomputes the clone's own matview from the clone's tables |

A matview on a clone is a **local** object built from copy-on-read tables, so
recomputing it locally is what makes it current; nothing is copied from the
source's stored matview contents.

Adoption only applies to children of a parent the clone already has. A brand new
standalone table still reports `re-clone to include it`, because reproducing
arbitrary DDL (indexes, defaults, triggers, grants) is the clone bootstrap's job,
whereas a partition takes its whole shape from its parent.

```bash
gfs query "SELECT * FROM gfs.pull();"   # action = 'adopt' / 'matview'
```

With `autoschema` on, new partitions and children are adopted in the background
too, so reads never go quietly short:

```bash
gfs query "UPDATE gfs.sync_policy SET autoschema = true;"
```

With `autoschema` off (the default), the gap is reported by `gfs fetch --check`
but reads of the parent stay short until you run `gfs pull`.

### `gfs fetch`

Ask whether the source has changed, without changing anything locally. This is the
read-only half of the source-sync pair: it reports, `gfs pull` acts.

```bash
gfs fetch                 # use the last cached verdict (no network)
gfs fetch --check         # probe the source right now
```

`--check` forces a fresh probe. Without it you get the verdict from the last
background check, which is cheap but only as current as `check_interval`.

What it reports:

| output | meaning |
| --- | --- |
| `source unchanged` | a guarantee nothing was written upstream since the last anchor |
| `N of M tables changed on the source` | which tables drifted, and why |
| `new table <name>` | a table exists upstream that this clone does not have |
| `changed, unattributed` | the source moved but nothing accounts for it, so every copied table is suspect |
| `this clone spans N source moments` | tables were copied at different moments while the source moved; a JOIN across them can return combinations that never existed at the source (run `gfs freeze`) |
| `all copied data is from one source moment` | possibly stale, but coherent: everything copied reflects one instant |

### `gfs pull`

Put tables the source has changed back on the lazy path, so the next read is local
again. `pull` copies nothing itself: it clears cached state and the normal cost model
decides again on the next read.

```bash
gfs pull                      # reconcile now
gfs pull --force              # also reset tables YOU wrote to, discarding your changes
gfs pull --auto on|off        # turn automatic pulling on or off
gfs pull --auto-schema on|off # turn automatic schema repair on or off
```

Each line of output names an action:

| action | what happened |
| --- | --- |
| `adopt` | a new partition or inheritance child of a table you already have was created locally and registered |
| `schema` | the table's shape was repaired (a column the source added was applied) |
| `reset` | the table is back on the lazy path; the next read refetches it |
| `enum` | a label the source added was replicated, in the source's own order |
| `sequence` | a local sequence was advanced to match the source |
| `matview` | a materialized view was recomputed from this clone's tables |
| `conflict` | **needs you.** Either you wrote to a table the source also changed, or the source made a destructive change. Nothing was touched |

A table you have written to is **never** reset without `--force`, because that would
discard your work. It is reported as a conflict, the way git refuses to clobber local
changes.

### `gfs remote`

Show the source a clone reads from. Like `git remote`, it makes no network round
trip: it reports what is recorded locally and leaves probing to `gfs fetch --check`.
The source password is never printed.

```bash
gfs remote           # origin postgres://user@host:5432/db (fetch only)
gfs remote --json
```

### Why there is no `gfs push`

The sync verbs are deliberately asymmetric: `fetch` and `pull` exist, `push` never
will. The source is typically a production database; pushing a clone's test data
into it is exactly the foot-gun GFS exists to prevent. **"The source is never
written to" is a hard rule of the system** -- enforced end-to-end and asserted on
every path in the test suite (`assert_source_untouched`), not a missing feature.
Resolving diverged tables (`merge`/`rebase`) is future work blocked on row-level
change tracking (see RFC 007). And there is no separate `gfs diff`: GFS has no
per-row change log on either side, so `gfs fetch` already shows everything a diff
could know (which tables changed, and why).

### `gfs status`

Show the current state of storage and compute resources. On a lazy clone, a
**Source** section reports where the clone stands: tracked tables, how many are
behind the source, how many have diverged (changed both locally and upstream),
and when that verdict was last checked. The same fields appear as a `source`
object in `--output json` (`tracked`, `behind`, `diverged`, `last_checked`);
the object is omitted when the repository is not a clone.

```bash
gfs status
gfs status --output json
```

### `gfs commit`

Commit the current database state.

```bash
gfs commit -m "commit message"
```

### `gfs log`

Show the commit history.

```bash
gfs log
gfs log -n 10              # Limit to 10 commits
gfs log --full-hash         # Show full 64-char hashes
```

### `gfs checkout`

Switch to a different commit or branch.

```bash
gfs checkout <commit_hash>       # Checkout a specific commit
gfs checkout -b <branch_name>   # Create and checkout a new branch
gfs checkout <branch_name>      # Checkout an existing branch
```

### `gfs query`

Execute SQL queries or open an interactive database terminal.

```bash
gfs query "SELECT * FROM users"   # Execute a query
gfs query                         # Open interactive terminal
```

Options: `--database`, `--path`

### `gfs schema`

Database schema operations: extract, show, and diff.

```bash
gfs schema extract [--output <file>] [--compact]
gfs schema show <commit> [--metadata-only] [--ddl-only]
gfs schema diff <commit1> <commit2> [--pretty] [--json]
```

### `gfs export`

Export data from the running database.

```bash
gfs export --output-dir <dir> --format <fmt>
```

Formats: `sql` (plain-text SQL), `custom` (PostgreSQL binary dump)

> **Known issue on lazy clones.** `gfs export` dumps what the clone holds
> *locally*. A lazy clone only holds rows for tables it has actually read, so
> exporting one can produce a valid-looking file with entire tables empty, with
> no warning. Read every table you need first, or export from a normal (non-clone)
> repository. Tracked in
> [#116](https://github.com/Guepard-Corp/gfs/issues/116).

### `gfs import`

Import data into the running database.

```bash
gfs import --file <path> [--format <fmt>]
```

Supports `.sql`, `.dump`, and `.csv` files. Format is inferred from file extension when omitted.

### `gfs config`

Read or write repository config.

```bash
gfs config user.name              # Read
gfs config user.name "John Doe"   # Write
```

### `gfs compute`

Manage the database container.

```bash
gfs compute start     # Start the container
gfs compute stop      # Stop the container
gfs compute status    # Show container status
gfs compute logs      # View container logs
```

## Configuration

GFS uses Docker to manage database containers. Make sure Docker is installed and running before using GFS.

### Requirements

- Docker (latest version recommended)
- Bash/Zsh shell
- `curl` for installation
- `tar` for extracting releases

## Troubleshooting

### Docker not running

```bash
# Start Docker Desktop or Docker daemon
# On macOS/Windows: Start Docker Desktop
# On Linux: sudo systemctl start docker
```

### Port conflicts

If the default port is already in use, stop the conflicting service or check `gfs status` for the assigned port.

### Connection issues

1. Check that the container is running: `docker ps`
2. Verify the connection details with: `gfs status`
3. Ensure Docker has network access

## Development

### Prerequisites

- Rust (latest stable version)
- Docker
- Cargo

### Running locally

```bash
git clone https://github.com/Guepard-Corp/gfs.git
cd gfs
cargo build
```

Run commands using cargo:

```bash
cargo run --bin gfs init --database-provider postgres --database-version 17 [--port 65432]
cargo run --bin gfs commit -m "v1"
cargo run --bin gfs log
cargo run --bin gfs status
```

### Testing

```bash
cargo test                        # Run all tests
cargo test-all                    # Full suite including E2E (sequential)
cargo test -- --test-threads=1    # Alternative sequential execution
cargo cov                         # Generate coverage report
cargo test <test_name>            # Run specific tests
cargo test -- --nocapture         # Run with output
```

**Optional: Better test reports and code coverage**

- [cargo-nextest](https://nexte.st/): Faster, clearer test output. Install with `cargo install cargo-nextest`, then run `cargo nextest run` or `cargo nt`.
- [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov): Code coverage. Install with `cargo install cargo-llvm-cov` (requires `rustup component add llvm-tools-preview`). Run `cargo llvm-cov --html --open` for an HTML report.

#### Clone behaviour suites

The lazy clone has its own suites, because most of what can go wrong needs a real
source database and a real clone container rather than a unit test.

```bash
cargo build --release                                    # the suites run the real binary
docker build -t gfs-postgres:16 crates/extensions/gfs    # and the real image

tests/paths/run-all.sh          # one test per documented clone behaviour
tests/paths/run-all.sh B        # one family
tests/paths/run-all.sh B4 D3    # specific cases
tests/paths/run-all.sh --list   # what is covered, runs nothing
```

`tests/paths/` has one script per path a clone can take, so a failure names the exact
case rather than "the drift suite broke". Each script builds its own throwaway source
and its own clone, so they cannot contaminate each other. See
[`tests/paths/README.md`](tests/paths/README.md) for what every test proves and for the
traps that have produced false results here (for example: `psql` inside a clone still
goes through the planner hook, so it will hydrate the table you were trying to inspect).

Three statuses matter:

| status | meaning |
| --- | --- |
| `FAIL` | a real product defect |
| `ABORT` | the **environment** failed (a container never came up). Never a product defect |
| `known-open now passes` | a test that documents an unfixed behaviour started passing. Either a fix landed or the assertion is too weak |

To test against a **real** remote database rather than a throwaway container:

```bash
# safe on production-shaped data: asserts router decisions, writes nothing upstream
scripts/e2e-clone-remote-source.sh readonly "postgresql://user:pw@host:5432/db?sslmode=require"

# mutates, but only inside its own gfs_test_<pid> schema, dropped on exit
scripts/e2e-clone-remote-source.sh drift "postgresql://user:pw@host:5432/db?sslmode=require"
```

This is the only way to exercise the cost model at real scale, since the copy-vs-ask
decision depends on measured link speed and real table sizes.

### Concurrency and reclaim scripts

Three things about commit are only observable under load or after a crash, and
each has a script rather than a paragraph, so the claim can be re-checked
instead of believed.

```bash
# Commit repeatedly while a separate process writes. Every snapshot must pass
# integrity_check, hold a row count the database genuinely passed through, and
# contain only whole transactions. The writer stops at GFS_TORTURE_MAX_ROWS
# (default 2,000,000, about 90 MB) — without a cap it wrote 7.5 GB in twelve
# rounds, because each round runs for as long as the growing commit takes.
scripts/sqlite-snapshot-torture.py <repo> ./target/debug/gfs 20

# Try to make a concurrent commit and checkout lose a commit. Needs two
# branches and a workspace big enough that the snapshot copy takes real time.
scripts/gfs-commit-checkout-race.py <repo> ./target/debug/gfs 10

# List snapshot trees no commit refers to, and optionally delete them.
scripts/gfs-reclaim-orphan-snapshots.py <repo> [--delete]
```

**Coverage note.** The SQLite e2e suites (`e2e_sqlite`, `mcp_sqlite_no_runtime`)
are `cfg(unix)`, so they run on Linux as well as macOS. That matters more than
it looks: on Linux `storage-file` uses `cp --reflink=auto`, which silently
degrades to a deep copy on ext4, and a deep copy of a database being written to
is where an unquiesced snapshot would tear. The concurrency test there asserts
the invariants, but at a size sensible for CI it passes with the snapshot guard
removed — the copy is too quick to tear. The torture script above is the one
that shows why the guard has to exist.

**Orphan snapshots.** A commit takes its snapshot before it writes the commit
object, and the destination comes from the workspace path and a timestamp
rather than from the content, so it is created up front. Killing `gfs commit`
between those two points — `SIGKILL`, a lost session, a machine losing power —
leaves a complete-looking snapshot tree that nothing references. Nothing is
corrupt and the next commit works; the tree is simply never read again and
never freed. Reproduced with a 640 MB orphan from one killed commit. There is
no `gfs gc`, so the reclaim script above is the way to get the space back; it
lists before it deletes, and only considers a snapshot orphaned when no object
under `.gfs/objects` names it.

### Building for release

```bash
cargo build --release
```

The binary will be available at `target/release/gfs`.

## Known limitations

These are real and deliberately documented rather than left for you to discover.

### A lazy clone is not a point-in-time snapshot (but it knows when it stopped being one)

Tables are copied at the moment you first read them, so different tables can come from
different moments, and the combination may be a state the source never actually had.

Concretely: read `orders` at 10:00, the source inserts order #4 with its items at 10:05,
you read `order_items` at 10:10. Your clone now holds items belonging to an order it
does not have. A join silently drops those rows, and each table on its own looks
perfectly consistent.

This cannot be *prevented* on a live source — you cannot read the past of a database
that has already thrown the past away — but it is **detected** (#131): every copy
event records where the source's WAL was when those rows arrived (`gfs.copy_watermark`),
so "this clone spans WAL X..Y" is a computable fact rather than a guess. `gfs fetch`
reports the span, `gfs status` shows a `Moments` row, and the drift warning now
distinguishes *torn* (mixes moments) from *stale* (a coherent view of one earlier
moment) — drift and tornness are different facts, and each can occur without the other.
A chunked table gets a min..max span and a moment count, not per-row provenance: which
individual row came from which moment is unknowable once ranges coalesce.

The way out is `gfs freeze` ([#132](https://github.com/Guepard-Corp/gfs/issues/132)):
re-copy everything from one instant and detach; a frozen clone reports a single moment
by construction. `gfs pull` alone does **not** end a tear — it resets changed tables to
be refetched lazily, which narrows the span only if the next reads happen while the
source holds still.

Detection is [#131](https://github.com/Guepard-Corp/gfs/issues/131); the snapshot mode
is [#132](https://github.com/Guepard-Corp/gfs/issues/132). The two properties genuinely
conflict: staying *current* and holding *still* cannot both be true of a source you do
not control, so it is a choice — per clone, per moment you freeze it.

### Table inheritance with unkeyed children cannot be cloned

Copy-on-read needs a unique key per table, and PostgreSQL does not carry a parent's
primary key down to a child created with `INHERITS`. A child without its own key cannot
be registered, and the clone refuses to build rather than leaving it as a silently empty
table. The refusal is correct, but there is no fallback yet.
Tracked as [#139](https://github.com/Guepard-Corp/gfs/issues/139).

### An idle source can make every table look changed

An idle PostgreSQL server still advances its WAL position (checkpoints, autovacuum). If
that movement cannot be attributed to any table, the safe response is to treat every
copied table as suspect, which sends reads back to the source. So a clone can drift into
federating everything shortly after a `gfs pull`, even with no user activity upstream.
Tracked as [#140](https://github.com/Guepard-Corp/gfs/issues/140).

### With `autoschema` off, a new partition reads short

`gfs fetch --check` reports the new partition, but reads of the parent return fewer rows
than the source holds until you run `gfs pull`. Every other kind of drift is loud; this
one is quiet, because the parent still answers. Turning on `autoschema` closes it.

## Contributing

We welcome contributions! Whether you're fixing bugs, adding features, or improving documentation, your help is appreciated.

Please see our [CONTRIBUTING.md](CONTRIBUTING.md) for detailed guidelines on:
- How to submit contributions
- Code contribution workflow
- Good first issues to get started
- Development best practices

For quick questions, join our [Discord community](https://discord.gg/SEdZuJbc5V).

## Community

- **Discord**: [Join our community](https://discord.gg/SEdZuJbc5V)
- **YouTube**: [Watch the demo](https://youtu.be/WlOkLnoY2h8?si=hb6-7kLhlOvVL1u6)
- **Issues**: [Report bugs or request features](https://github.com/Guepard-Corp/gfs/issues)

## Roadmap

Check [Roadmap](ROADMAP.md)

## License

This project is licensed under MIT License. See the [LICENSE](LICENSE) file for details.

---

<div align="center">
Made with love by the Guepard team
</div>
