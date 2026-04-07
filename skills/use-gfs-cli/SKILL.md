---
name: use-gfs-cli
description: Git-like version control for databases using the GFS CLI. Manage database states with commits, branches, time travel, and schema versioning.
---

# GFS CLI (Git For database Systems)

GFS brings Git-like version control to your databases. Commit database states, create branches, switch between versions, travel through your database history, and track schema evolution over time.

## Supported Databases

- PostgreSQL (versions 13-18)
- MySQL (versions 8.0-8.1)

## Installation

### 1. Check if GFS is installed

```shell
gfs version
```

### 2. If not installed, install using

```bash
curl -fsSL https://gfs.guepard.run/install | bash
```

Validate the installation:
```shell
gfs version
```

## Global Flags

### `--json` — Machine-Readable Output

Add `--json` before any subcommand for structured JSON output instead of styled text. This is the recommended approach for scripts and AI agents:

```shell
# Commit and get the hash programmatically
gfs --json commit -m "add users table"
# → {"hash":"b57732d8...","branch":"main","message":"add users table"}

# Init and get the connection string
gfs --json init --database-provider postgres --database-version 17
# → {"path":"/my/project","branch":"main","config":".gfs/config.toml","provider":"postgres"}

# Checkout and confirm
gfs --json checkout -b feature/auth
# → {"hash":"b57732d8...","branch":"feature/auth","new_branch":true}

# Export and get the file path
gfs --json export --format sql
# → {"file_path":".gfs/exports/dump.sql","format":"sql"}
```

**Supported commands**: `init`, `commit`, `checkout`, `export`, `import`
`gfs status --output json` also works (predates the global flag).

### `--color` — Color Control

```shell
gfs --color never status   # No ANSI colors (for piping)
gfs --color always log     # Force colors even when piped
```

### Exit Codes

| Command | Code | Meaning |
|---------|------|---------|
| `gfs status` | 0 | Compute running or not configured |
| `gfs status` | 1 | Compute configured but not running |
| `gfs schema diff` | 0 | No schema changes |
| `gfs schema diff` | 1 | Schema changes detected |
| `gfs schema diff` | 2 | Breaking changes detected |

Use exit codes for conditional logic:
```shell
if gfs status > /dev/null 2>&1; then
    gfs query "SELECT 1"
else
    gfs compute start
fi
```

## Quick Start

### 1. Check available database providers

```shell
gfs providers
```

This shows all supported database providers, versions, and features.

### 2. Initialize a repository

First, ask the user which database provider and version they need. Then initialize:

```shell
# For PostgreSQL
gfs init --database-provider postgres --database-version 17

# For MySQL
gfs init --database-provider mysql --database-version 8.0
```

This creates a `.gfs` directory in your project and starts a Docker container with the database.

### 3. Check status

```shell
gfs status
```

Shows current branch, HEAD commit, and database container status (including connection string if running).

## Revision References

GFS supports Git-style revision notation for referencing commits:

### Basic Formats

- `HEAD` - Current commit
- `main` - Branch tip (any branch name)
- `abc123def456...` - Full commit hash (64 characters)

### Tilde Notation (Ancestor References)

Reference ancestor commits using `~` notation:

- `HEAD~1` - Parent of HEAD (previous commit)
- `HEAD~5` - 5th ancestor of HEAD
- `main~3` - 3 commits before main branch tip
- `abc123~2` - 2nd parent of commit abc123
- `HEAD~` - Same as `HEAD~1` (defaults to 1)

### Usage Examples

```shell
# Checkout previous commit
gfs checkout HEAD~1

# Compare schema with 5 commits ago
gfs schema diff HEAD~5 HEAD

# View schema from 3 commits back
gfs schema show main~3

# Show specific ancestor
gfs schema show abc123def456...~2

# View log from ancestor to current
gfs log --from HEAD~10 --until HEAD
```

Tilde notation works with any command that accepts commit references: `checkout`, `schema show`, `schema diff`, `log`, etc.

## Core Workflow

### Best Practices

**CRITICAL**: Always commit before making mutations to the database. This enables rollback if issues occur.

```shell
gfs commit -m "before adding new feature"
```

### 1. Commit Changes

Create snapshots of your database state (automatically includes schema):

```shell
gfs commit -m "descriptive message"

# With author information
gfs commit -m "added user table" --author "John Doe" --author-email "john@example.com"
```

**New in schema versioning**: Every commit automatically captures the database schema at commit time. This includes both structured metadata (tables, columns, types) and native DDL (SQL).

### 2. View History

```shell
# View all commits
gfs log

# Limit number of commits
gfs log --max-count 10

# View commits in a range
gfs log --from main --until feature-branch

# Show branch topology graph (all branches)
gfs log --graph --all

# Show graph for current branch only
gfs log --graph
```

### 3. Manage Branches

```shell
# List all branches (* marks current)
gfs branch

# Create branch at HEAD (without switching)
gfs branch feature-branch

# Create branch from specific commit/checkpoint
gfs branch release/v1.0 <commit_id>

# Create and switch to new branch
gfs branch -c feature-branch
# or the classic shorthand:
gfs checkout -b feature-branch

# Delete a branch
gfs branch -d feature-branch
```

### 4. Switch Branches

```shell
# Switch to existing branch
gfs checkout main

# Checkout specific commit (use full 64-char hash from gfs log)
gfs checkout <commit_id>
```

### 5. Database Management

#### Start/Stop Database Container

```shell
# Check container status
gfs compute status

# Start the database
gfs compute start

# Stop the database
gfs compute stop

# Restart the database
gfs compute restart

# Pause/Unpause (keeps state in memory)
gfs compute pause
gfs compute unpause

# View logs
gfs compute logs
gfs compute logs --tail 100
gfs compute logs --since "2024-01-01T00:00:00Z"
```

### 6. Query Database

Execute SQL queries directly:

```shell
# Interactive mode (opens database client)
gfs query

# Execute a query
gfs query "SELECT * FROM users;"

# Query specific database
gfs query --database mydb "SHOW TABLES;"
```

### 7. Schema Operations

**NEW**: GFS now provides comprehensive schema versioning and inspection capabilities.

#### Extract Current Schema

Extract schema from the running database:

```shell
# Extract and display schema (JSON metadata)
gfs schema extract

# Save to file
gfs schema extract --output schema.json

# Compact JSON output
gfs schema extract --compact
```

#### Show Schema from Commit

View the schema as it existed at any commit:

```shell
# Show full schema (metadata + DDL) from HEAD
gfs schema show HEAD

# Show schema from specific commit
gfs schema show abc123

# Show only metadata (JSON)
gfs schema show HEAD --metadata-only

# Show only DDL (SQL)
gfs schema show main --ddl-only
```

**Output includes**:
- Schema hash (content-addressed identifier)
- Database driver and version
- Structured metadata (schemas, tables, columns)
- Native DDL dump (CREATE TABLE statements, etc.)

#### Compare Schemas Between Commits

Track schema evolution by comparing commits. Both arguments accept `rev~n` notation for ancestor references:

```shell
# Compare HEAD with previous commit
gfs schema diff HEAD~1 HEAD

# Compare with 5 commits ago using rev~n notation
gfs schema diff HEAD~5 HEAD

# Compare two branches
gfs schema diff main feature-branch

# Compare specific commits (rev~n works with any commit reference)
gfs schema diff abc123~2 def456
```

**Diff output includes**:
- Schema hash comparison (detects if schemas are identical)
- Table count changes (+/- tables)
- Column count changes (+/- columns)
- DDL comparison summary
- Suggestion to use external diff tools for detailed DDL differences

#### Schema Versioning Features

1. **Automatic Capture**: Every `gfs commit` automatically extracts and stores the schema
2. **Content-Addressed**: Identical schemas share the same hash (storage deduplication)
3. **Dual Format**: Both JSON metadata and SQL DDL are stored
4. **Time Travel**: View schema at any point in history
5. **Non-Blocking**: Schema extraction failures don't prevent commits
6. **Backward Compatible**: Old commits without schemas work fine

### 8. Import/Export Data

#### Export

```shell
# Export as SQL dump
gfs export --format sql

# Export to specific directory
gfs export --format sql --output-dir /path/to/exports

# Export using custom format (PostgreSQL's pg_dump custom format)
gfs export --format custom
```

#### Import

```shell
# Import data file (format auto-detected from extension)
gfs import /path/to/data.sql

# Specify format explicitly
gfs import --format sql /path/to/dump.sql
gfs import --format csv /path/to/data.csv
gfs import --format json /path/to/data.json

# Import custom database-specific format (e.g., PostgreSQL's pg_dump custom format)
gfs import --format custom /path/to/dump.dump
```

Supported formats: SQL, CSV, JSON, custom (database-specific formats)

## Configuration

GFS stores configuration in `.gfs/config.toml`:

```shell
# View configuration
gfs config

# Configuration includes:
# - Database provider and version
# - Container runtime settings
# - Branch information
```

## Common Workflows

### Experiment with Schema Changes

```shell
# 1. Create a branch for experimentation
gfs checkout -b schema-experiment

# 2. Make schema changes (via gfs query or your DB client)
gfs query "ALTER TABLE users ADD COLUMN age INTEGER;"

# 3. Test the changes
gfs query "SELECT * FROM users;"

# 4. If satisfied, commit (schema is automatically captured)
gfs commit -m "added age column to users"

# 5. Compare schema with main branch
gfs schema diff main schema-experiment

# 6. Switch back to main
gfs checkout main
```

### Track Schema Evolution

```shell
# View schema at current commit
gfs schema show HEAD

# View schema from 5 commits ago
gfs schema show HEAD~5

# Compare schema changes over time
gfs schema diff HEAD~10 HEAD

# View schema across recent commits using rev~n notation
gfs schema show HEAD~1 --ddl-only
gfs schema show HEAD~2 --ddl-only
gfs schema show HEAD~3 --ddl-only
```

### Rollback to Previous State

```shell
# 1. View history to find the commit
gfs log

# 2. Check what the schema looked like then
gfs schema show <commit_id>

# 3. Checkout the specific commit
gfs checkout <commit_id>

# 4. Verify the state
gfs status
gfs query "SHOW TABLES;"

# 5. If this is the correct state, create a new branch or commit
gfs checkout -b rollback-branch
gfs commit -m "rolled back to working state"
```

### Audit Schema Changes

```shell
# View full commit history to find a specific change
gfs log

# Show schema at that commit
gfs schema show <commit_id>

# Compare with current schema
gfs schema diff <commit_id> HEAD

# Compare with pretty visual output
gfs schema diff <commit_id> HEAD --pretty

# Extract DDL for code review
gfs schema show <commit_id> --ddl-only > schema-then.sql
gfs schema show HEAD --ddl-only > schema-now.sql
diff -u schema-then.sql schema-now.sql
```

### Test Data Import

```shell
# 1. Commit current state
gfs commit -m "before import test"

# 2. Import test data
gfs import /path/to/test-data.sql

# 3. Run tests
gfs query "SELECT COUNT(*) FROM users;"

# 4. If failed, rollback by checking out previous commit
gfs log
gfs checkout <previous_commit_id>
```

## Troubleshooting

### Database won't start

```shell
# Check container status
gfs compute status

# View logs for errors
gfs compute logs

# Try restarting
gfs compute restart
```

### Connection issues

```shell
# Get connection info
gfs status

# Ensure container is running
gfs compute status

# Check if port is available
# GFS uses default ports: PostgreSQL (5432), MySQL (3306)
```

### Repository not found

Ensure you're in a directory with `.gfs` folder or set:
```shell
export GFS_REPO_PATH=/path/to/your/gfs/repo
```

### Schema extraction fails

If schema is not captured during commit:
- Check that the database container is running: `gfs compute status`
- View container logs: `gfs compute logs`
- Try manual extraction: `gfs schema extract`
- Note: Schema extraction failures don't prevent commits (best-effort)

## Key Reminders

1. **Always commit before mutations** - This is your safety net for rollbacks
2. **Schema is automatically versioned** - Every commit captures database schema
3. **Use branches for experiments** - Keep main branch stable
4. **Check status regularly** - Understand current state and container status
6. **Export before risky operations** - Create backups with `gfs export`
7. **Use descriptive commit messages** - Makes history navigation easier
8. **Container must be running** - Most operations require active database container
9. **Schema as documentation** - Use `gfs schema show` to document database structure

## Environment Variables

- `GFS_REPO_PATH` - Default repository path for CLI
- Set in your shell for convenience
