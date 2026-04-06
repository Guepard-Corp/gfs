---
name: use-gfs-mcp
description: GFS MCP Server for AI agent integration. Provides Model Context Protocol tools for database version control with automatic schema versioning.
---

# GFS MCP Server

GFS provides an MCP (Model Context Protocol) server for AI agent integration, enabling programmatic access to database version control operations including commits, branches, time travel, and schema versioning.

## Supported Databases

- PostgreSQL (versions 13-18)
- MySQL (versions 8.0-8.1)

## Installation

### 1. Verify GFS CLI is installed

The MCP server requires the GFS CLI to be installed on the host system.

### 2. Configure MCP Server

Add GFS to your MCP client configuration (e.g., Claude Desktop, custom MCP client). The MCP server runs as: `gfs mcp`

Configuration requires:
- Command: Path to `gfs` binary
- Args: `["mcp"]`
- Optional: `GFS_REPO_PATH` environment variable

## Quick Start

### 1. Start MCP Server

The server starts automatically when configured in your MCP client. It provides access to all GFS operations through MCP tools.

### 2. Check Available Tools

The MCP server exposes 13 tools for database version control operations. All tools accept an optional `path` parameter to specify repository location.

### 3. Verify Repository Status

Use the `status` tool to check repository state, current branch, HEAD commit, and database container status.

## Revision References

GFS supports Git-style revision notation in all tools that accept commit references (`checkout`, `show_schema`, `diff_schema`, `log`):

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

### MCP Tool Usage Examples

```javascript
// Checkout previous commit
checkout({ revision: "HEAD~1" })

// Compare schema with 5 commits ago
diff_schema({ commit1: "HEAD~5", commit2: "HEAD" })

// View schema from 3 commits back
show_schema({ commit: "main~3" })

// Show ancestor schema
show_schema({ commit: "abc123def456...~2" })

// View log from ancestor to current
log({ from: "HEAD~10", until: "HEAD" })
```

## Core Workflow

### Best Practices

**CRITICAL**: Always use the `commit` tool before database mutations. This enables rollback if issues occur.

Commits automatically capture database schema at commit time, providing complete versioning of both data and structure.

### 1. List Providers

Tool: `list_providers`

Lists all supported database providers, versions, and available features. No parameters required.

Returns: Array of providers with supported versions and feature lists.

### 2. Initialize Repository

Tool: `init`

Creates a new GFS repository with specified database provider and version.

Parameters:
- `database_provider` (required) - Provider name (postgres, mysql)
- `database_version` (required) - Version string (e.g., "17", "8.0")
- `path` (optional) - Repository location

Returns: Repository path and initialization status.

### 3. Check Status

Tool: `status`

Gets repository and database container status including current branch, HEAD commit, and connection information.

Parameters:
- `path` (optional) - Repository location

Returns: Branch name, commit hash, container state, connection info if running.

### 4. Commit Changes

Tool: `commit`

Creates snapshot of database state, automatically capturing schema at commit time.

Parameters:
- `message` (required) - Commit message
- `path` (optional) - Repository location
- `author` (optional) - Author name
- `author_email` (optional) - Author email

Returns: Commit hash.

**Automatic Schema Versioning**: Every commit captures database schema including structured metadata and native DDL.

### 5. View History

Tool: `log`

Views commit history with optional filtering.

Parameters:
- `path` (optional) - Repository location
- `max_count` (optional) - Limit number of commits
- `from` (optional) - Starting revision
- `until` (optional) - Ending revision

Returns: Array of commits with metadata, timestamps, authors, and references.

### 6. Create and Switch Branches

Tool: `checkout`

Switches to existing branch, specific commit, or creates new branch.

Parameters:
- `revision` (required) - Branch name or commit hash
- `path` (optional) - Repository location
- `create_branch` (optional) - Create new branch if true

Returns: Success status and checked out revision.

**Tip**: For branch listing and creation without switching, use the CLI with `--json`:
```shell
# List branches programmatically
gfs branch

# Create branch from checkpoint without switching
gfs branch release/v1.0 <commit_hash>

# Delete a branch
gfs branch -d old-feature
```

### 7. Database Container Management

Tool: `compute`

Manages database container lifecycle.

Parameters:
- `action` (required) - Operation: status, start, stop, restart, pause, unpause, logs
- `path` (optional) - Repository location
- `tail` (optional, logs only) - Number of lines to show
- `since` (optional, logs only) - Timestamp filter

Returns: Status information or log output based on action.

Actions:
- **status** - Container state and metadata
- **start** - Start stopped container
- **stop** - Stop running container
- **restart** - Restart container
- **pause** - Pause container (keeps state in memory)
- **unpause** - Resume paused container
- **logs** - View container logs

### 8. Query Database

Tool: `query`

Executes SQL queries against the database or returns connection information.

Parameters:
- `query` (optional) - SQL query to execute (omit for connection info)
- `path` (optional) - Repository location
- `database` (optional) - Override database name

Returns: Query results or connection information if no query provided.

### 9. Schema Operations

#### Extract Current Schema

Tool: `extract_schema`

Extracts schema from running database, returning structured metadata.

Parameters:
- `path` (optional) - Repository location

Returns: Complete schema metadata including schemas, tables, columns, constraints, and relationships as structured JSON.

Use case: Understand current database structure before making changes or writing queries.

#### Show Schema from Commit

Tool: `show_schema`

Views database schema as it existed at any commit in history.

Parameters:
- `commit` (required) - Commit hash or reference (HEAD, main, branch name)
- `path` (optional) - Repository location
- `metadata_only` (optional) - Return only JSON metadata
- `ddl_only` (optional) - Return only SQL DDL

Returns: Schema object with hash, metadata, and/or DDL based on flags.

Output includes:
- Schema hash (content-addressed identifier)
- Database driver and version
- Structured metadata (schemas, tables, columns with types and constraints)
- Native DDL dump (CREATE TABLE statements)

Use case: View schema at any point in history, document database structure, audit schema changes.

#### Compare Schemas

Tool: `diff_schema`

Tracks schema evolution by comparing schemas between two commits.

Parameters:
- `commit1` (required) - First commit hash or reference
- `commit2` (required) - Second commit hash or reference
- `path` (optional) - Repository location

Both `commit1` and `commit2` accept `rev~n` notation for ancestor references (e.g., `HEAD~5`, `main~3`).

Returns: Schema comparison with hashes, table/column count changes, and difference summary.

Comparison includes:
- Schema hash equality check
- Table count changes
- Column count changes
- Summary of additions/removals

Use case: Review schema changes before merging branches, track schema evolution, identify breaking changes.

### 10. Import/Export Data

#### Export Database

Tool: `export_database`

Exports database data to file.

Parameters:
- `format` (required) - Export format (sql, custom)
- `path` (optional) - Repository location
- `output_dir` (optional) - Output directory path

Returns: Export file path and status.

Supported formats:
- **sql** - Plain SQL dump
- **custom** - Database-specific binary format (PostgreSQL pg_dump custom format)

#### Import Database

Tool: `import_database`

Imports data from file into database.

Parameters:
- `file_path` (required) - Path to import file
- `path` (optional) - Repository location
- `format` (optional) - Data format (sql, csv, json, custom)

Returns: Import status.

Supported formats: SQL, CSV, JSON, custom (database-specific formats).

## Common Workflows

### Schema-Aware Development

Track schema changes alongside code changes for complete version control.

Workflow:
1. Use `status` to verify repository and container state
2. Use `show_schema` to view current schema structure
3. Use `checkout` with `create_branch` to create feature branch
4. Make schema changes via `query` tool
5. Use `commit` to capture changes (schema automatically versioned)
6. Use `diff_schema` to compare with main branch
7. Review differences before merging

### Schema Evolution Tracking

Monitor how database schema changes over time.

Workflow:
1. Use `log` to view commit history
2. Use `show_schema` on different commits to see schema at each point
3. Use `diff_schema` to compare schemas across time periods
4. Identify when tables/columns were added or modified
5. Document schema migrations based on commit history

### Rollback to Previous State

Restore database to earlier state when issues occur.

Workflow:
1. Use `log` to find target commit
2. Use `show_schema` to verify schema at that commit
3. Use `checkout` to switch to specific commit
4. Use `status` to verify state
5. Use `query` to validate data
6. Use `checkout` with `create_branch` if state is correct

### Schema Migration Planning

Plan database migrations by comparing schemas.

Workflow:
1. Use `show_schema` on production commit
2. Use `show_schema` on feature branch
3. Use `diff_schema` to identify all schema changes
4. Review table and column count differences
5. Plan migration steps based on differences
6. Test migration in isolated branch

### Automated Schema Documentation

Generate documentation from schema history.

Workflow:
1. Use `log` to get list of releases or major versions
2. Use `show_schema` with `metadata_only` for each version
3. Parse schema metadata to extract table and column information
4. Use `diff_schema` between versions to identify breaking changes
5. Generate documentation highlighting evolution

## Troubleshooting

### Database Container Not Running

Use `compute` with action `status` to check container state. If stopped, use action `start` to start container. Use action `logs` to view error messages if container fails to start.

### Connection Issues

Use `status` tool to get connection information. Ensure container is running via `compute` status. Verify ports are available (PostgreSQL: 5432, MySQL: 3306).

### Repository Not Found

Ensure `path` parameter points to directory with `.gfs` folder, or set `GFS_REPO_PATH` environment variable in MCP server configuration.

### Schema Not Available

If `show_schema` returns error about missing schema, the commit was created before schema versioning was enabled. Use `extract_schema` to capture current schema, then create new commit.

### Tool Execution Fails

Check that GFS CLI is installed on MCP server host. Verify Docker is running for database operations. Review error messages in tool response for specific issues.

## CLI Fallback with `--json`

When MCP tools are unavailable or you need operations not yet exposed as MCP tools (e.g., branch management, graph log), use the GFS CLI with `--json` for structured output:

```shell
# Structured commit output
gfs --json commit -m "checkpoint before migration"
# → {"hash":"b57732d8...","branch":"main","message":"checkpoint before migration"}

# Structured checkout output
gfs --json checkout -b feature/migration
# → {"hash":"b57732d8...","branch":"feature/migration","new_branch":true}

# Branch listing
gfs branch

# Branch topology graph
gfs log --graph --all
```

**Exit codes** for conditional logic:
- `gfs status` → 0 (compute running) or 1 (compute down)
- `gfs schema diff` → 0 (no changes), 1 (changes), 2 (breaking)

## Key Reminders

1. **Always commit before mutations** - Create safety checkpoints with automatic schema capture
2. **Schema automatically versioned** - Every commit captures complete database schema
3. **Use diff_schema before merging** - Review schema changes between branches
4. **Check status first** - Verify repository and container state before operations
5. **Path parameter** - Specify repository location or set GFS_REPO_PATH
6. **Container must be running** - Database operations require active container
7. **Show_schema for documentation** - Use to document database structure at any point
8. **Extract_schema for current state** - Capture current schema without commit
9. **Tool responses are structured** - Parse JSON responses for programmatic access
10. **Use `--json` for CLI fallback** - When MCP tools aren't available, CLI with `--json` gives structured output

## Environment Variables

- `GFS_REPO_PATH` - Default repository path for MCP server
- Set in MCP server configuration environment section
- When set, tools use this as default if `path` parameter omitted

## Schema Versioning Features

Every `commit` operation automatically:
1. Extracts current database schema (metadata + DDL)
2. Stores schema as content-addressed object
3. Links schema hash to commit
4. Enables time-travel schema viewing via `show_schema`

Schema storage:
- **Content-addressed**: Identical schemas share same hash (deduplication)
- **Dual format**: JSON metadata + SQL DDL stored together
- **Immutable**: Content addressing ensures integrity
- **Fast comparison**: Compare via hash equality

Schema metadata includes:
- Database driver and version
- Schemas/namespaces
- Tables with sizes and row estimates
- Columns with types, constraints, and defaults
- Relationships and indexes

Schema DDL includes:
- CREATE TABLE statements
- Indexes and constraints
- Triggers and views
- Database-specific syntax
