\set ON_ERROR_STOP on

CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE EXTENSION IF NOT EXISTS dblink;

-- REQUIRED copy-on-read extension (gfs). The clone IS a set of faithful real
-- tables (relkind='r' with the source's indexes); the gfs planner hook fetches
-- each query's matching rows from the source on read and writes them through
-- locally — there is NO overlay fallback. The client image MUST ship this
-- extension; if it is absent this statement raises under \set ON_ERROR_STOP and
-- the clone fails by design. See crates/extensions/gfs.
CREATE EXTENSION IF NOT EXISTS gfs;

-- The gfs copy-on-read logic is a planner hook in the extension's shared library.
-- Load it on EVERY connection to this database (apps connect directly to the
-- connection string), so a fresh app session has the hook active. session-level
-- (not shared) preload: no restart needed; superuser-only ALTER, run here as the
-- bootstrap superuser. Without this the tables read as empty local heaps.
DO $pl$
BEGIN
  EXECUTE format('ALTER DATABASE %I SET session_preload_libraries = %L',
                 current_database(), 'gfs');
END
$pl$;

DROP SERVER IF EXISTS gfs_remote_srv CASCADE;
CREATE SERVER gfs_remote_srv
  FOREIGN DATA WRAPPER postgres_fdw
  -- use_remote_estimate is REQUIRED for join/aggregate PUSHDOWN: without it
  -- postgres_fdw cannot cost the remote join and falls back to fetching base-table
  -- rows over a cursor and joining locally (FETCH 100 at a time) -- which collapses
  -- a federated multi-table query at scale (a 6-table join over 60M rows took 41
  -- min instead of pushing the whole join to the source and returning ~5 rows).
  -- fetch_size raises the cursor batch for the cases that still aren't pushed.
  OPTIONS (host '__RHOST__', port '__RPORT__', dbname '__RDB__',
           use_remote_estimate 'true', fetch_size '10000');

-- FOR PUBLIC so any local role (not just the one that ran the bootstrap) can
-- read through the foreign-data wrapper.
CREATE USER MAPPING FOR PUBLIC
  SERVER gfs_remote_srv
  OPTIONS (user '__RUSER__', password '__RPASS__');

CREATE SCHEMA IF NOT EXISTS gfs_sync;

-- Whether the gfs copy-on-read extension is present (required; the clone is a set
-- of faithful real tables driven by the gfs planner hook).
CREATE OR REPLACE FUNCTION gfs_sync.clone_tam()
RETURNS boolean
LANGUAGE sql STABLE AS $fn$
  SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'gfs')
$fn$;
GRANT EXECUTE ON FUNCTION gfs_sync.clone_tam() TO PUBLIC;

-- Mirror the remote's extensions locally (best-effort) so extension types
-- resolve on import. Extensions absent from the local image fail here and their
-- tables are skipped at import time.
CREATE OR REPLACE FUNCTION gfs_sync.mirror_extensions(p_conn text)
RETURNS void
LANGUAGE plpgsql AS $fn$
DECLARE
  ext record;
BEGIN
  FOR ext IN SELECT * FROM dblink(p_conn, $e$
      SELECT extname FROM pg_extension WHERE extname <> 'plpgsql'
    $e$) AS r(extname text)
  LOOP
    BEGIN
      EXECUTE format('CREATE EXTENSION IF NOT EXISTS %I', ext.extname);
    EXCEPTION WHEN others THEN
      RAISE NOTICE 'gfs: extension % not available locally (tables using it will be skipped)', ext.extname;
    END;
  END LOOP;
END
$fn$;

-- Mirror user-defined types (not part of any extension) so foreign-table
-- imports referencing them resolve locally, in dependency order:
-- ENUMs, then DOMAINs, then COMPOSITEs. Each is created in the same schema/name
-- as the remote. Best-effort: a type that can't be recreated is left out and
-- its tables are skipped at import.
CREATE OR REPLACE FUNCTION gfs_sync.mirror_types(p_conn text, p_schemas text[])
RETURNS void
LANGUAGE plpgsql AS $fn$
DECLARE
  schlist  text;
  enumtyp  record;
  domtyp   record;
  comptyp  record;
  pass     int;
  progress boolean;
BEGIN
  schlist := (SELECT string_agg(quote_literal(x), ', ') FROM unnest(p_schemas) AS x);

  -- ENUMs (preserve label order).
  FOR enumtyp IN SELECT * FROM dblink(p_conn, format($en$
      SELECT n.nspname::text, t.typname::text,
             (SELECT array_agg(e.enumlabel ORDER BY e.enumsortorder)
                FROM pg_enum e WHERE e.enumtypid = t.oid)
      FROM pg_type t
      JOIN pg_namespace n ON n.oid = t.typnamespace
      WHERE t.typtype = 'e' AND n.nspname IN (%s)
    $en$, schlist))
    AS r(nsp text, typ text, labels text[])
  LOOP
    EXECUTE format('CREATE SCHEMA IF NOT EXISTS %I', enumtyp.nsp);
    BEGIN
      EXECUTE format('CREATE TYPE %I.%I AS ENUM (%s)', enumtyp.nsp, enumtyp.typ,
        (SELECT string_agg(quote_literal(l), ', ') FROM unnest(enumtyp.labels) AS l));
    EXCEPTION
      WHEN duplicate_object THEN NULL;  -- already present (re-run or from an extension)
      WHEN others THEN
        RAISE NOTICE 'gfs: could not mirror enum %.% (%)', enumtyp.nsp, enumtyp.typ, SQLERRM;
    END;
  END LOOP;

  -- DOMAINs (base type + DEFAULT + NOT NULL + CHECKs).
  FOR domtyp IN SELECT * FROM dblink(p_conn, format($dm$
      SELECT n.nspname::text, t.typname::text,
             format_type(t.typbasetype, t.typtypmod)::text,
             t.typnotnull,
             t.typdefault,
             COALESCE((SELECT string_agg(pg_get_constraintdef(c.oid), ' ' ORDER BY c.oid)
                         FROM pg_constraint c WHERE c.contypid = t.oid), '')
      FROM pg_type t
      JOIN pg_namespace n ON n.oid = t.typnamespace
      WHERE t.typtype = 'd' AND n.nspname IN (%s)
    $dm$, schlist))
    AS r(nsp text, typ text, base text, dnn boolean, deflt text, checks text)
  LOOP
    EXECUTE format('CREATE SCHEMA IF NOT EXISTS %I', domtyp.nsp);
    BEGIN
      EXECUTE format('CREATE DOMAIN %I.%I AS %s%s%s %s',
        domtyp.nsp, domtyp.typ, domtyp.base,
        CASE WHEN domtyp.deflt IS NOT NULL THEN ' DEFAULT ' || domtyp.deflt ELSE '' END,
        CASE WHEN domtyp.dnn THEN ' NOT NULL' ELSE '' END,
        domtyp.checks);
    EXCEPTION
      WHEN duplicate_object THEN NULL;
      WHEN others THEN
        RAISE NOTICE 'gfs: could not mirror domain %.% (%)', domtyp.nsp, domtyp.typ, SQLERRM;
    END;
  END LOOP;

  -- COMPOSITEs (standalone types, relkind 'c'). Multi-pass so a composite that
  -- references another composite is created once its dependency exists; bounded
  -- to guarantee termination. A pass that creates nothing ends the loop.
  FOR pass IN 1..10 LOOP
    progress := false;
    FOR comptyp IN SELECT * FROM dblink(p_conn, format($cp$
        SELECT n.nspname::text, t.typname::text,
               (SELECT string_agg(quote_ident(a.attname) || ' ' || format_type(a.atttypid, a.atttypmod), ', ' ORDER BY a.attnum)
                  FROM pg_attribute a
                  WHERE a.attrelid = t.typrelid AND a.attnum > 0 AND NOT a.attisdropped)
        FROM pg_type t
        JOIN pg_namespace n ON n.oid = t.typnamespace
        JOIN pg_class c ON c.oid = t.typrelid
        WHERE t.typtype = 'c' AND c.relkind = 'c' AND n.nspname IN (%s)
      $cp$, schlist))
      AS r(nsp text, typ text, cols text)
    LOOP
      CONTINUE WHEN to_regtype(format('%I.%I', comptyp.nsp, comptyp.typ)) IS NOT NULL;
      EXECUTE format('CREATE SCHEMA IF NOT EXISTS %I', comptyp.nsp);
      BEGIN
        EXECUTE format('CREATE TYPE %I.%I AS (%s)', comptyp.nsp, comptyp.typ, comptyp.cols);
        progress := true;
      EXCEPTION WHEN others THEN
        NULL;  -- a dependency may not exist yet; retried on the next pass
      END;
    END LOOP;
    EXIT WHEN NOT progress;
  END LOOP;
END
$fn$;

-- Import one remote schema's tables into its shadow schema, ONE TABLE AT A TIME
-- so a table whose type cannot resolve locally (missing extension) is skipped
-- rather than aborting the whole clone.
CREATE OR REPLACE FUNCTION gfs_sync.import_schema(p_conn text, p_sch text)
RETURNS void
LANGUAGE plpgsql AS $fn$
DECLARE
  shadow text;
  tb     record;
BEGIN
  shadow := 'gfs_remote_' || p_sch;
  EXECUTE format('DROP SCHEMA IF EXISTS %I CASCADE', shadow);
  EXECUTE format('CREATE SCHEMA %I', shadow);
  EXECUTE format('CREATE SCHEMA IF NOT EXISTS %I', p_sch);

  FOR tb IN SELECT * FROM dblink(p_conn, format($t$
      SELECT c.relname::text FROM pg_class c
      JOIN pg_namespace n ON n.oid = c.relnamespace
      WHERE n.nspname = %L AND c.relkind = 'r'
    $t$, p_sch)) AS r(relname text)
  LOOP
    BEGIN
      EXECUTE format('IMPORT FOREIGN SCHEMA %I LIMIT TO (%I) FROM SERVER gfs_remote_srv INTO %I',
                     p_sch, tb.relname, shadow);
    EXCEPTION WHEN others THEN
      RAISE WARNING 'gfs: skipped table %.%: % (provision a local image that has the required extension, e.g. gfs clone --image <ref>)', p_sch, tb.relname, SQLERRM;
    END;
  END LOOP;
END
$fn$;

-- Copy-on-read clone for one table. The faithful table p_nsp.p_tab already exists
-- (replayed from pg_dump --schema-only) and is empty; it stays a plain heap table
-- (with the source's indexes) — we just register its source (the imported foreign
-- table gfs_remote_<schema>.<table>) so the gfs planner hook fetches matching rows
-- on read. We also DROP its foreign keys: the hook fetches each table by its own
-- predicate, so a child row can arrive before its parent — RI must not trip (the
-- source already enforced FKs; the clone is a working copy, like a replica).
-- Returns false (skips) only when the table/foreign table is absent; any real
-- failure propagates and aborts the clone (no overlay fallback).
CREATE OR REPLACE FUNCTION gfs_sync.build_clone(p_nsp text, p_tab text, p_keycols text[])
RETURNS boolean
LANGUAGE plpgsql AS $fn$
DECLARE
  store_fq  text := format('%I.%I', p_nsp, p_tab);
  fq_remote text := format('%I.%I', 'gfs_remote_' || p_nsp, p_tab);
  fk        record;
BEGIN
  IF to_regclass(store_fq) IS NULL THEN
    RAISE NOTICE 'gfs: no clone for %.% (faithful table not present)', p_nsp, p_tab;
    RETURN false;
  END IF;
  IF to_regclass(fq_remote) IS NULL THEN
    RAISE NOTICE 'gfs: no clone for %.% (foreign table not imported)', p_nsp, p_tab;
    RETURN false;
  END IF;
  -- Drop foreign keys so lazy, per-table copy-on-read never trips RI.
  FOR fk IN
    SELECT conname FROM pg_constraint
     WHERE conrelid = store_fq::regclass AND contype = 'f'
  LOOP
    EXECUTE format('ALTER TABLE %s DROP CONSTRAINT %I', store_fq, fk.conname);
  END LOOP;
  -- Register the source; the gfs planner hook does the rest on read.
  PERFORM gfs.register_clone(store_fq::regclass, fq_remote, p_keycols[1]);
  RETURN true;
END
$fn$;

CREATE OR REPLACE FUNCTION gfs_sync.clone(p_conn text, p_schemas text[])
RETURNS void
LANGUAGE plpgsql AS $fn$
DECLARE
  target_schemas text[] := p_schemas;
  schlist text;
  s       text;
  rec     record;
BEGIN
  IF target_schemas IS NULL THEN
    SELECT array_agg(nspname) INTO target_schemas FROM dblink(p_conn, $disc$
      SELECT nspname FROM pg_namespace
      WHERE nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast')
        AND nspname NOT LIKE 'pg_temp%' AND nspname NOT LIKE 'pg_toast%'
    $disc$) AS r(nspname text);
  END IF;

  PERFORM gfs_sync.mirror_extensions(p_conn);
  PERFORM gfs_sync.mirror_types(p_conn, target_schemas);

  FOREACH s IN ARRAY target_schemas LOOP
    PERFORM gfs_sync.import_schema(p_conn, s);
  END LOOP;

  schlist := (SELECT string_agg(quote_literal(x), ', ') FROM unnest(target_schemas) AS x);

  FOR rec IN
    SELECT * FROM dblink(p_conn, format($q$
      SELECT nsp, tab, keycols FROM (
        SELECT n.nspname::text AS nsp, c.relname::text AS tab,
               (SELECT array_agg(a.attname::text ORDER BY k.ord)
                  FROM unnest(i.indkey::int[]) WITH ORDINALITY AS k(attnum, ord)
                  JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = k.attnum) AS keycols,
               row_number() OVER (PARTITION BY c.oid
                  ORDER BY i.indisprimary DESC, i.indnkeyatts ASC, i.indexrelid) AS rn
        FROM pg_index i
        JOIN pg_class c     ON c.oid = i.indrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname IN (%s) AND c.relkind = 'r'
          AND i.indisunique AND i.indpred IS NULL AND 0 <> ALL (i.indkey::int[])
      ) s WHERE rn = 1
    $q$, schlist)) AS r(nsp text, tab text, keycols text[])
  LOOP
    -- The faithful table IS the clone: a plain heap table registered with the gfs
    -- planner hook, which fetches each query's matching rows on read. No overlay
    -- view, no search_path shim — apps read the real table directly. The gfs
    -- extension is required (created above); without it the bootstrap already
    -- aborted.
    PERFORM gfs_sync.build_clone(rec.nsp, rec.tab, rec.keycols);
  END LOOP;
END
$fn$;

-- Run the clone.
SELECT gfs_sync.clone('__CONN__', __SCHEMAS_ARRAY__);

-- Auto-calibrate the cost router by probing the source link (network throughput,
-- scan rate, latency). Best-effort: the clone works even if this is skipped.
DO $cal$
BEGIN
  PERFORM gfs.calibrate();
EXCEPTION WHEN others THEN
  RAISE NOTICE 'gfs: cost calibration skipped (%)', SQLERRM;
END
$cal$;
