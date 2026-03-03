# Issue: Schema Extraction Only Captures Default Database

## Problem Statement

When using GFS with PostgreSQL, schema extraction and commits only capture the default database (`postgres`), missing any additional databases created in the same PostgreSQL instance.

### Example Scenario

```bash
# Import SQL file that creates a new database
gfs import --file benchmarks/flight-data-300MB/demo-small-en-20170815.sql

# File contains:
# CREATE DATABASE demo;
# CREATE SCHEMA bookings;
# CREATE TABLE bookings.flights (...);
# ... 12 tables with 33,121 rows

# Commit the state
gfs commit -m "Import flight data"

# Problem: Schema shows empty!
gfs schema show HEAD
# Output: "tables": []
```

## Root Cause

File: `crates/adapters/compute-docker/src/containers/postgresql.rs:519`

```rust
fn schema_extraction_spec(&self, params: &ConnectionParams) -> ... {
    let user = params.get_env(ENV_USER).unwrap_or(DEFAULT_USER);
    let password = params.get_env(ENV_PASSWORD).unwrap_or(DEFAULT_PASSWORD);
    let db = params.get_env(ENV_DB).unwrap_or(DEFAULT_DB);  // ← PROBLEM: Only one DB

    // Queries executed against single database:
    psql -h {host} -p {port} -U {user} -d {db} -t -A -c "{query}"
}
```

The extraction connects to a single database (default: `postgres`) and queries its schemas/tables. Any tables in other databases (like `demo`) are ignored.

## Impact

- ✅ **Single-database workflows**: Work correctly (most common case)
- ❌ **Multi-database imports**: Missing schema in commits
- ❌ **Database-per-tenant**: Cannot track multiple tenant databases
- ❌ **Microservices**: Cannot version multiple service databases together

## Proposed Solution

### Option 1: Multi-Database Schema Extraction (Recommended)

Modify schema extraction to iterate over all non-template databases:

**Changes Required:**

1. **Query all databases first:**
```sql
SELECT datname FROM pg_database
WHERE datistemplate = false
AND datname NOT IN ('postgres')  -- Keep postgres separate
ORDER BY datname;
```

2. **For each database, extract schema:**
```bash
for db in $(list_databases); do
  echo "GFS_SCHEMA_DATABASE:$db"
  psql -h {host} -U {user} -d $db -t -A -c "{schemas_query}"
  echo "GFS_SCHEMA_TABLES:$db"
  psql -h {host} -U {user} -d $db -t -A -c "{tables_query}"
  echo "GFS_SCHEMA_COLUMNS:$db"
  psql -h {host} -U {user} -d $db -t -A -c "{columns_query}"
done
```

3. **Update DatasourceMetadata structure:**
```rust
pub struct DatasourceMetadata {
    pub version: String,
    pub driver: String,
    pub databases: Vec<DatabaseMetadata>,  // NEW: Multiple databases
}

pub struct DatabaseMetadata {
    pub name: String,              // NEW: Database name
    pub schemas: Vec<Schema>,
    pub tables: Vec<Table>,
    pub columns: Vec<Column>,
}
```

4. **Update schema extraction parser:**
   - Parse `GFS_SCHEMA_DATABASE:` delimiters
   - Group schemas/tables/columns by database
   - Build hierarchical structure

5. **Update pg_dump export:**
```bash
# Export all databases
for db in $(list_databases); do
  pg_dump -h {host} -U {user} -d $db --clean --if-exists
done
```

### Option 2: Explicit Database Selection (Quick Fix)

Add `--database` flag to import/commit commands:

```bash
# Specify which database to track
gfs import --file demo.sql --database demo
gfs commit -m "Import" --database demo
```

**Pros**: Simple, minimal code changes
**Cons**: User must manage database selection, easy to miss databases

### Option 3: Configuration-Based (Middle Ground)

Add database list to `.gfs/config.toml`:

```toml
[environment]
database_provider = "postgres"
database_version = "17"
databases = ["postgres", "demo"]  # Explicit list
```

**Pros**: Clear, explicit configuration
**Cons**: Requires manual configuration updates

## Recommended Implementation Plan

**Phase 1: Multi-Database Support (Breaking Change)**

1. Update `DatasourceMetadata` to support multiple databases
2. Modify PostgreSQL schema extraction to iterate databases
3. Update schema parsers and serialization
4. Update `pg_dump` export to handle all databases
5. Update import to restore all databases
6. Add migration for existing repos (convert single DB → multi-DB format)

**Phase 2: UI/UX Improvements**

1. Add `gfs schema show --database <name>` filter
2. Add `gfs query --database <name>` (already exists!)
3. Update `gfs status` to show all databases
4. Add `gfs schema list-databases` command

**Phase 3: Advanced Features**

1. Per-database schema diff
2. Selective database commits (e.g., only commit `demo`)
3. Cross-database relationship tracking
4. Database-level branching

## Migration Strategy

### For Existing Repositories

Old format (single database):
```json
{
  "version": "PostgreSQL 17...",
  "driver": "postgres",
  "schemas": [...],
  "tables": [...],
  "columns": [...]
}
```

New format (multi-database):
```json
{
  "version": "PostgreSQL 17...",
  "driver": "postgres",
  "databases": [
    {
      "name": "postgres",
      "schemas": [...],
      "tables": [...],
      "columns": [...]
    },
    {
      "name": "demo",
      "schemas": [...],
      "tables": [...],
      "columns": [...]
    }
  ]
}
```

**Migration Script:**
```rust
fn migrate_schema_to_multi_database(old: &DatasourceMetadata) -> DatasourceMetadata {
    DatasourceMetadata {
        version: old.version.clone(),
        driver: old.driver.clone(),
        databases: vec![
            DatabaseMetadata {
                name: "postgres".to_string(),
                schemas: old.schemas.clone(),
                tables: old.tables.clone(),
                columns: old.columns.clone(),
            }
        ],
    }
}
```

## Testing Strategy

1. **Unit Tests:**
   - Test multi-database schema extraction
   - Test database iteration and parsing
   - Test migration from old format

2. **Integration Tests:**
   - Import SQL with multiple databases
   - Commit and verify all databases captured
   - Checkout and verify all databases restored

3. **Benchmark:**
   - Test with flight data (creates `demo` database)
   - Verify 12 tables in `bookings` schema captured
   - Verify commit/checkout cycle preserves all data

## Compatibility Considerations

**Backward Compatibility:**
- Old commits will be readable (migration on load)
- New commits will use multi-database format
- Schema diff must handle both formats

**Database Provider Support:**
- PostgreSQL: Full support (primary use case)
- MySQL: Similar concept (multiple databases per instance)
- SQLite: N/A (single file = single database)

## Workarounds (Until Fixed)

### Workaround 1: Modify Import Files

```bash
# Remove CREATE DATABASE statements
sed '/^CREATE DATABASE/d; /^\\connect/d' original.sql > fixed.sql
gfs import --file fixed.sql
```

### Workaround 2: Use Default Database

```bash
# Connect to demo database manually and dump
docker exec -i <container> pg_dump -U postgres demo > demo_dump.sql

# Drop tables and reimport to postgres database
# ... manual steps ...
```

### Workaround 3: Use Separate GFS Instances

```bash
# One GFS repo per database (not ideal)
mkdir demo_repo && cd demo_repo
gfs init --database-provider postgres
# Manually configure to use demo database
```

## References

- PostgreSQL Multi-Database Architecture: https://www.postgresql.org/docs/current/managing-databases.html
- Issue Report: User unable to commit flight benchmark data
- Related: RFC for multi-tenant support

## Decision

**Recommended:** Implement Option 1 (Multi-Database Schema Extraction)

**Rationale:**
- Most correct solution architecturally
- Aligns with how PostgreSQL actually works
- Enables future features (multi-tenant, microservices)
- Breaking change acceptable (early stage project)

**Timeline:**
- Week 1: Design and prototype data structures
- Week 2: Implement schema extraction changes
- Week 3: Update export/import to handle multiple databases
- Week 4: Testing, migration, documentation
