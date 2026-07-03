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

-- Replicate each local sequence's CURRENT POSITION from the source. The faithful
-- schema replay (`pg_dump --schema-only`) emits `CREATE SEQUENCE` but NOT the
-- sequence position (`setval` lives in pg_dump's data section, which a schema-only
-- dump omits), so every local sequence restarts at its initial value (typically 1).
-- A source serial/identity/standalone sequence advanced past its start would then
-- hand out values that COLLIDE with rows already materialized on the clone. For
-- each local sequence (relkind='S' — catches serial-owned, identity-owned, and
-- standalone sequences) we read the matching source sequence's `last_value` and
-- `is_called` straight off the sequence relation via dblink (NOT pg_sequences,
-- whose last_value is NULL/permission-sensitive) and setval() the local one to
-- match. Per-sequence failures warn loudly rather than abort: a clone with one
-- un-synced sequence is recoverable; an aborted clone is not. A sequence absent on
-- the source (local-only) is simply skipped.
CREATE OR REPLACE FUNCTION gfs_sync.replicate_sequences(p_conn text, p_schemas text[])
RETURNS void
LANGUAGE plpgsql AS $fn$
DECLARE
  seqrec record;
  src    record;
  fq     text;
BEGIN
  FOR seqrec IN
    SELECT n.nspname::text AS nsp, c.relname::text AS seq
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE c.relkind = 'S' AND n.nspname = ANY (p_schemas)
  LOOP
    fq := format('%I.%I', seqrec.nsp, seqrec.seq);
    BEGIN
      -- Read the source sequence's current state directly off the relation. A
      -- regclass cast on the source guards a sequence that exists locally but not
      -- remotely: dblink raises, we catch and skip below.
      SELECT * INTO src FROM dblink(p_conn, format(
          'SELECT last_value, is_called FROM %s', fq))
        AS r(last_value bigint, is_called boolean);
      IF src.last_value IS NOT NULL THEN
        PERFORM setval(fq::regclass, src.last_value, src.is_called);
      END IF;
    EXCEPTION WHEN others THEN
      RAISE WARNING 'gfs: could not replicate sequence value for % (%): inserts on the clone may collide with materialized rows', fq, SQLERRM;
    END;
  END LOOP;
END
$fn$;

-- Safeguard: a partitioned table must NEVER be silently dropped from the clone.
-- A partitioned PARENT (relkind='p') holds no rows and is skipped by the table
-- enumeration and the copy-on-read registration (both relkind='r'); it needs no
-- registration of its own because the faithful replay re-creates it and a query on
-- the parent prunes to its leaf partitions. Each LEAF partition (relkind='r') is an
-- ordinary table that the relkind='r' path above imports and registers like any
-- other table — that is how copy-on-read is wired for partitions. The risk is a
-- leaf that does NOT round-trip: it has no usable unique key (so the keycol query
-- skips it), or it failed to import / replay. The current code would leave that
-- leaf as an empty, unregistered local heap and the clone would silently return no
-- rows for that slice of the partitioned table. This function enumerates the
-- SOURCE's partitioned tables and their leaf partitions (pg_partition_tree, so it
-- also covers multi-level subpartitions), then asserts every leaf is locally
-- present AND registered as a copy-on-read clone. Any gap RAISEs (fatal under
-- ON_ERROR_STOP) naming the offending partitioned table, so a partitioned table is
-- never silently dropped — it either round-trips fully or fails the clone loudly.
CREATE OR REPLACE FUNCTION gfs_sync.verify_partitions(p_conn text, p_schemas text[])
RETURNS void
LANGUAGE plpgsql AS $fn$
DECLARE
  schlist  text;
  leaf     record;
  leaf_fq  text;
  problems text[] := ARRAY[]::text[];
BEGIN
  schlist := (SELECT string_agg(quote_literal(x), ', ') FROM unnest(p_schemas) AS x);

  FOR leaf IN SELECT * FROM dblink(p_conn, format($q$
      SELECT pn.nspname::text AS parent_nsp, pc.relname::text AS parent_tab,
             ln.nspname::text AS leaf_nsp,   lc.relname::text AS leaf_tab
      FROM pg_class pc
      JOIN pg_namespace pn ON pn.oid = pc.relnamespace
      CROSS JOIN LATERAL pg_partition_tree(pc.oid) pt
      JOIN pg_class lc     ON lc.oid = pt.relid AND pt.isleaf
      JOIN pg_namespace ln ON ln.oid = lc.relnamespace
      WHERE pc.relkind = 'p' AND pn.nspname IN (%s)
    $q$, schlist)) AS r(parent_nsp text, parent_tab text, leaf_nsp text, leaf_tab text)
  LOOP
    leaf_fq := format('%I.%I', leaf.leaf_nsp, leaf.leaf_tab);
    IF to_regclass(leaf_fq) IS NULL THEN
      problems := problems || format('%I.%I (partition %s missing locally)',
                    leaf.parent_nsp, leaf.parent_tab, leaf_fq);
    ELSIF NOT EXISTS (
      SELECT 1 FROM gfs.clone_source WHERE relid = leaf_fq::regclass
    ) THEN
      problems := problems || format('%I.%I (partition %s not registered for copy-on-read; it likely lacks a usable unique key)',
                    leaf.parent_nsp, leaf.parent_tab, leaf_fq);
    END IF;
  END LOOP;

  IF array_length(problems, 1) > 0 THEN
    RAISE EXCEPTION 'gfs: partitioned table(s) did not fully round-trip onto the clone: %. Refusing to leave them silently empty.',
      array_to_string(problems, '; ');
  END IF;
END
$fn$;

-- Bug B safeguard: a source table with no usable unique key is skipped by the keycol
-- query in clone() (it needs a unique, non-partial, non-expression index), so it is
-- never registered for copy-on-read and would be a SILENT empty heap on the clone
-- (data loss, no error). Assert every ordinary (non-partition) source table is locally
-- present AND registered; any gap RAISEs (fatal under ON_ERROR_STOP), naming the table.
-- Leaf partitions are covered by verify_partitions. NOTE: until keyless whole-table
-- hydration lands in the gfs extension, a primary-key-less table fails the clone loudly
-- here rather than silently losing its rows.
CREATE OR REPLACE FUNCTION gfs_sync.verify_tables_registered(p_conn text, p_schemas text[])
RETURNS void
LANGUAGE plpgsql AS $fn$
DECLARE
  schlist  text;
  t        record;
  fq       text;
  problems text[] := ARRAY[]::text[];
BEGIN
  schlist := (SELECT string_agg(quote_literal(x), ', ') FROM unnest(p_schemas) AS x);
  FOR t IN SELECT * FROM dblink(p_conn, format($q$
      SELECT n.nspname::text AS nsp, c.relname::text AS tab
      FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
      WHERE c.relkind = 'r' AND NOT c.relispartition AND n.nspname IN (%s)
    $q$, schlist)) AS r(nsp text, tab text)
  LOOP
    fq := format('%I.%I', t.nsp, t.tab);
    IF to_regclass(fq) IS NULL THEN
      problems := problems || format('%s (missing locally)', fq);
    ELSIF NOT EXISTS (SELECT 1 FROM gfs.clone_source WHERE relid = fq::regclass) THEN
      problems := problems || format('%s (no usable unique key -> not registered for copy-on-read; would silently return no rows)', fq);
    END IF;
  END LOOP;
  IF array_length(problems, 1) > 0 THEN
    RAISE EXCEPTION 'gfs: source table(s) did not register for copy-on-read onto the clone: %. Refusing to leave them silently empty.',
      array_to_string(problems, '; ');
  END IF;
END
$fn$;

-- Bug C: the faithful pg_dump replay re-creates materialized views but leaves them
-- unpopulated, so reading one errors "has not been populated". Refresh each local
-- matview now that its base tables are registered for copy-on-read (the refresh reads
-- the base tables, hydrating them). Two passes so a matview defined over another matview
-- can still populate; a final failure warns rather than aborting the clone.
CREATE OR REPLACE FUNCTION gfs_sync.refresh_matviews(p_schemas text[])
RETURNS void
LANGUAGE plpgsql AS $fn$
DECLARE
  mv       record;
  base     record;
  pass     int := 0;
  progress boolean := true;
BEGIN
  -- Progress-based multi-pass (bounded): a matview built on another matview
  -- populates on a later pass once its dependency has. Stop as soon as a pass
  -- populates nothing new; cap at 20 as a termination backstop. (A fixed 2 passes
  -- only covers a single level of matview-on-matview; the progress loop covers
  -- arbitrarily deep chains.)
  WHILE progress AND pass < 20 LOOP
    progress := false;
    pass := pass + 1;
    FOR mv IN
      SELECT schemaname, matviewname,
             format('%I.%I', schemaname, matviewname)::regclass AS oid
      FROM pg_matviews
      WHERE schemaname = ANY(p_schemas) AND NOT ispopulated
    LOOP
      BEGIN
        -- A matview on a lazy clone reads copy-on-read base tables that are not yet
        -- materialized at bootstrap, so a bare REFRESH populates it from ZERO rows
        -- (ispopulated=t but empty -- silently wrong). Fully materialize every
        -- registered clone table this matview depends on (gfs.warm = committed FDW
        -- copy; covers direct bases AND partition leaves) BEFORE the REFRESH so it
        -- computes over real data.
        FOR base IN
          SELECT DISTINCT cs.relid::regclass AS rel
          FROM pg_depend d
          JOIN pg_rewrite rw ON rw.oid = d.objid AND rw.ev_class = mv.oid
          JOIN gfs.clone_source cs
            ON cs.relid = d.refobjid
            OR cs.relid IN (SELECT inhrelid FROM pg_inherits WHERE inhparent = d.refobjid)
          WHERE d.refclassid = 'pg_class'::regclass AND d.refobjid <> mv.oid
        LOOP
          PERFORM gfs.warm(base.rel);
        END LOOP;
        EXECUTE format('REFRESH MATERIALIZED VIEW %I.%I', mv.schemaname, mv.matviewname);
        progress := true;   -- populated one; a later pass may now unblock a dependent
      EXCEPTION WHEN OTHERS THEN
        NULL;  -- not yet (e.g. a dependency matview still empty); retried next pass
      END;
    END LOOP;
  END LOOP;
  -- Anything still unpopulated after the loop warns loudly (base unavailable or the
  -- definition failed) but does NOT abort the clone -- that matview just keeps the
  -- pre-fix "REFRESH it manually" behavior.
  FOR mv IN
    SELECT schemaname, matviewname FROM pg_matviews
    WHERE schemaname = ANY(p_schemas) AND NOT ispopulated
  LOOP
    RAISE WARNING 'gfs: could not populate materialized view %.% (REFRESH it manually)', mv.schemaname, mv.matviewname;
  END LOOP;
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

  -- Inherit each local sequence's current position from the source so the clone
  -- does not restart serial/identity/standalone sequences at 1 and collide with
  -- already-materialized rows. Runs after the tables (and their owned sequences)
  -- exist from the faithful replay.
  PERFORM gfs_sync.replicate_sequences(p_conn, target_schemas);

  -- Bug B safeguard: every ordinary source table must register for copy-on-read; a
  -- table with no usable unique key would otherwise be a silent empty heap. Fail loud.
  PERFORM gfs_sync.verify_tables_registered(p_conn, target_schemas);

  -- Fail loudly if any partitioned table did not fully round-trip (a leaf missing
  -- locally or not registered for copy-on-read), so a partitioned table is never
  -- silently dropped from the clone. Runs last, after every table is registered.
  PERFORM gfs_sync.verify_partitions(p_conn, target_schemas);

  -- Bug C: populate materialized views (created empty by the faithful replay) now that
  -- their base tables are registered for copy-on-read.
  PERFORM gfs_sync.refresh_matviews(target_schemas);
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
