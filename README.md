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
- Lazily clone a remote PostgreSQL database (copy-on-read, experimental)
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

### `gfs status`

Show the current state of storage and compute resources.

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

### Building for release

```bash
cargo build --release
```

The binary will be available at `target/release/gfs`.

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
