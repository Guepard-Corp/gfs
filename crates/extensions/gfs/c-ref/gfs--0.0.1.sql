-- gfs: the Table Access Method + the clone catalog/API.
-- \echo Use "CREATE EXTENSION gfs" to load this file. \quit

-- ---------------------------------------------------------------------------
-- The storage engine: a real heap-backed table that fetches missing rows from
-- the source on read (copy-on-read) and writes them through locally.
-- ---------------------------------------------------------------------------
CREATE FUNCTION gfs_handler(internal)
  RETURNS table_am_handler
  AS 'MODULE_PATHNAME'
  LANGUAGE C STRICT;

CREATE ACCESS METHOD gfs TYPE TABLE HANDLER gfs_handler;

COMMENT ON ACCESS METHOD gfs IS
  'GFS lazy-clone storage: a real heap-backed table whose missing rows are fetched from the source on read and written through locally';

-- ---------------------------------------------------------------------------
-- The catalog the TAM reads, and the API GFS calls. GFS does NOT embed any
-- copy-on-read logic: it creates the faithful table USING gfs, then calls
-- gfs.register_clone(...). Everything else is the extension's job.
-- ---------------------------------------------------------------------------
CREATE SCHEMA gfs;

COMMENT ON SCHEMA gfs IS 'GFS clone catalog + API (the TAM reads clone_source; stats land in clone_stats)';

-- Which gfs tables are copy-on-read clones, and where their source lives.
CREATE TABLE gfs.clone_source (
    relid       regclass PRIMARY KEY,          -- the local gfs table
    source_ref  text     NOT NULL,             -- source relation, e.g. 'gfs_remote.orders'
    key_col     text     NOT NULL DEFAULT 'id' -- key used to tell which rows are missing
);
COMMENT ON TABLE gfs.clone_source IS 'TAM config: maps a local gfs table to its source relation + key column';

-- Copy-on-read activity, for observability.
CREATE TABLE gfs.clone_stats (
    relid        regclass PRIMARY KEY
                 REFERENCES gfs.clone_source(relid) ON DELETE CASCADE,
    fetch_calls  bigint NOT NULL DEFAULT 0,    -- # scans that fetched >0 rows from the source
    rows_fetched bigint NOT NULL DEFAULT 0,    -- # rows copied-on-read so far
    last_fetch   timestamptz
);
COMMENT ON TABLE gfs.clone_stats IS 'TAM observability: copy-on-read activity per clone table';

-- Register a gfs table as a copy-on-read clone of source_ref (keyed by key_col).
CREATE FUNCTION gfs.register_clone(local regclass, source_ref text,
                                   key_col text DEFAULT 'id')
RETURNS void
LANGUAGE sql AS $$
    INSERT INTO gfs.clone_source(relid, source_ref, key_col)
         VALUES (local, source_ref, key_col)
    ON CONFLICT (relid)
        DO UPDATE SET source_ref = EXCLUDED.source_ref, key_col = EXCLUDED.key_col;
    INSERT INTO gfs.clone_stats(relid) VALUES (local)
    ON CONFLICT (relid) DO NOTHING;
$$;
COMMENT ON FUNCTION gfs.register_clone(regclass, text, text) IS
  'Tell the TAM that <local> is a copy-on-read clone of <source_ref> (key <key_col>)';

CREATE FUNCTION gfs.unregister_clone(local regclass)
RETURNS void
LANGUAGE sql AS $$
    DELETE FROM gfs.clone_source WHERE relid = local;
$$;

-- Force-materialize a clone in full (a seq scan triggers copy-on-read). Returns
-- the row count. GFS / an operator can pre-warm a table with this.
CREATE FUNCTION gfs.warm(local regclass)
RETURNS bigint
LANGUAGE plpgsql AS $$
DECLARE n bigint;
BEGIN
    -- copy-on-read rides the seq-scan path; force one
    SET LOCAL enable_indexscan = off;
    SET LOCAL enable_indexonlyscan = off;
    SET LOCAL enable_bitmapscan = off;
    EXECUTE format('SELECT count(*) FROM %s', local::text) INTO n;
    RETURN n;
END;
$$;
COMMENT ON FUNCTION gfs.warm(regclass) IS
  'Force a full copy-on-read materialization of a clone table (seq scan)';

-- Human-readable overview of every clone and its copy-on-read activity.
CREATE VIEW gfs.clones AS
    SELECT s.relid::text AS clone,
           s.source_ref,
           s.key_col,
           COALESCE(st.fetch_calls, 0)  AS fetch_calls,
           COALESCE(st.rows_fetched, 0) AS rows_fetched,
           st.last_fetch
      FROM gfs.clone_source s
      LEFT JOIN gfs.clone_stats st USING (relid)
     ORDER BY s.relid::text;

GRANT USAGE ON SCHEMA gfs TO PUBLIC;
GRANT SELECT ON gfs.clone_source, gfs.clone_stats, gfs.clones TO PUBLIC;
