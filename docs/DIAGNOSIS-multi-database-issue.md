# Diagnosis: Why Commits Show Empty Schema

## Current Situation

You imported flight data, but commits show no tables:

```bash
$ cargo run --bin gfs -- import --file benchmarks/flight-data-300MB/demo-small-en-20170815.sql
# Imported successfully

$ cargo run --bin gfs -- commit -m "Import flight data"
$ cargo run --bin gfs -- schema show HEAD
# Output: "tables": [] ← EMPTY!
```

## What I Found

### The Data IS There!

```bash
$ cargo run --bin gfs -- query --database demo "SELECT COUNT(*) FROM bookings.flights"
# Result: 33,121 rows in 12 tables
```

All the data exists, but it's in the **`demo`** database, not the **`postgres`** database.

### The Root Cause

**File**: `crates/adapters/compute-docker/src/containers/postgresql.rs:519`

```rust
let db = params.get_env(ENV_DB).unwrap_or(DEFAULT_DB); // Always uses "postgres"
```

GFS only extracts schema from the default database (`postgres`). When the SQL file creates a new database (`demo`), GFS doesn't see it.

### What the SQL File Does

```sql
Line 22: CREATE DATABASE demo;      ← Creates new database
Line 25: \connect demo;              ← Switches to demo
        CREATE SCHEMA bookings;      ← All tables created here
        CREATE TABLE flights ...;
```

### Current Architecture

```
PostgreSQL Instance
├── postgres (default) ← GFS extracts schema from here only
│   └── public schema
│       └── (empty)
└── demo               ← Your data is here!
    └── bookings schema
        ├── flights (33,121 rows)
        ├── airports
        └── ... (10 more tables)
```

## The Fix Required

GFS needs to support **multi-database schema extraction**:

1. **Query all databases:**
   ```sql
   SELECT datname FROM pg_database WHERE datistemplate = false;
   ```

2. **Extract schema from each:**
   ```bash
   for each database:
     psql -d $database_name -c "extract schemas"
     psql -d $database_name -c "extract tables"
     psql -d $database_name -c "extract columns"
   ```

3. **Store all databases in commits:**
   ```json
   {
     "databases": [
       {"name": "postgres", "tables": [...]},
       {"name": "demo", "tables": [...]}
     ]
   }
   ```

## Immediate Workaround

### Option 1: Query the Demo Database Directly

```bash
# All GFS commands support --database flag
cargo run --bin gfs -- query --database demo "SELECT * FROM bookings.flights LIMIT 10"

# But commits still won't capture it (schema extraction needs the fix)
```

### Option 2: Use Modified SQL File

```bash
# Remove database creation
sed '/^CREATE DATABASE/d; /^\\connect/d' \
  benchmarks/flight-data-300MB/demo-small-en-20170815.sql > /tmp/fixed.sql

# Import into postgres database
cargo run --bin gfs -- import --file /tmp/fixed.sql

# Now commits will work
cargo run --bin gfs -- commit -m "Import flight data"
```

## Impact

This affects:
- ✅ **Single-database use**: Works fine (most users)
- ❌ **Multi-database imports**: Missing data in commits
- ❌ **Multi-tenant apps**: Can't track all tenant DBs
- ❌ **Microservices**: Can't version multiple service DBs together

## Next Steps

**Short Term** (Workaround):
1. Use Option 2 above to reimport data into `postgres` database
2. Document limitation in user guide

**Long Term** (Proper Fix):
1. Implement multi-database schema extraction (see `docs/issues/ISSUE-multi-database-schema-extraction.md`)
2. Update data structures to support multiple databases
3. Update all commands (commit, schema, diff) to handle multiple databases
4. Add migration for existing repositories

## Files to Change

1. **Schema Extraction**:
   - `crates/adapters/compute-docker/src/containers/postgresql.rs` (modify extraction queries)

2. **Data Structures**:
   - `crates/domain/src/model/datasource/metadata.rs` (add database grouping)

3. **Use Cases**:
   - `crates/domain/src/usecases/repository/extract_schema_usecase.rs` (parse multi-DB output)
   - `crates/domain/src/usecases/repository/commit_usecase.rs` (commit all databases)

4. **CLI Commands**:
   - `crates/applications/cli/src/commands/cmd_schema.rs` (show/diff per database)

## Verification

After implementing the fix, this should work:

```bash
# Import creates demo database
cargo run --bin gfs -- import --file benchmarks/flight-data-300MB/demo-small-en-20170815.sql

# Commit captures ALL databases
cargo run --bin gfs -- commit -m "Import flight data"

# Schema shows both databases
cargo run --bin gfs -- schema show HEAD
# Expected:
# {
#   "databases": [
#     {"name": "postgres", "tables": []},
#     {"name": "demo", "tables": [12 tables from bookings schema]}
#   ]
# }

# Can query either database
cargo run --bin gfs -- query --database postgres "SELECT 1"
cargo run --bin gfs -- query --database demo "SELECT COUNT(*) FROM bookings.flights"
```

## References

- Full analysis: `docs/issues/ISSUE-multi-database-schema-extraction.md`
- PostgreSQL docs: https://www.postgresql.org/docs/current/managing-databases.html
- Current behavior: Schema extraction only queries `ENV_DB` (defaults to `postgres`)
