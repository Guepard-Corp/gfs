\set ON_ERROR_STOP on

CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE EXTENSION IF NOT EXISTS dblink;

DROP SERVER IF EXISTS gfs_remote_srv CASCADE;
CREATE SERVER gfs_remote_srv
  FOREIGN DATA WRAPPER postgres_fdw
  OPTIONS (host '__RHOST__', port '__RPORT__', dbname '__RDB__');

-- FOR PUBLIC so any local role (not just the one that ran the bootstrap) can
-- read through the foreign-data wrapper.
CREATE USER MAPPING FOR PUBLIC
  SERVER gfs_remote_srv
  OPTIONS (user '__RUSER__', password '__RPASS__');

CREATE SCHEMA IF NOT EXISTS gfs_sync;
CREATE TABLE IF NOT EXISTS gfs_sync.table_meta (
  schema_name text NOT NULL,
  table_name  text NOT NULL,
  key_cols    text NOT NULL,  -- conflict target, e.g. '"id"' or '"a", "b"'
  PRIMARY KEY (schema_name, table_name)
);

-- Network-elision metadata: key ranges fully hydrated into the local store.
-- Bounds are stored as text and cast to the key's type when used.
CREATE TABLE IF NOT EXISTS gfs_sync.cached_range (
  schema_name text NOT NULL,
  table_name  text NOT NULL,
  lo text NOT NULL,
  hi text NOT NULL,
  PRIMARY KEY (schema_name, table_name, lo, hi)
);

-- Tables hydrated in full (whole_table strategy, for non-range-able keys like
-- random uuids). Their foreign table gets a CHECK (false) so every read is
-- served locally with zero remote contact.
CREATE TABLE IF NOT EXISTS gfs_sync.fully_cached (
  schema_name text NOT NULL,
  table_name  text NOT NULL,
  PRIMARY KEY (schema_name, table_name)
);

-- Signature of the cached state last applied to a table's exclusion CHECK, so
-- refresh_exclusions() can skip tables whose ranges haven't changed (no needless
-- AccessExclusive ALTER every tick).
CREATE TABLE IF NOT EXISTS gfs_sync.applied_exclusion (
  schema_name text NOT NULL,
  table_name  text NOT NULL,
  sig text NOT NULL,
  PRIMARY KEY (schema_name, table_name)
);

-- Enable constraint exclusion so the planner can refute a query predicate that
-- falls within a cached range against the foreign table's exclusion CHECK and
-- prune the foreign scan entirely (zero remote contact). Applies to new sessions
-- (apps and the warmer connect after the bootstrap runs).
DO $ce$ BEGIN
  EXECUTE format('ALTER DATABASE %I SET constraint_exclusion = on', current_database());
END $ce$;

-- ---------------------------------------------------------------------------
-- gfs_sync helper functions
--
-- The clone is built by orchestrating these installed functions rather than a
-- single inline DO block. `gfs_sync.build_overlay` and `gfs_sync.clone` are
-- reusable (e.g. to overlay a table that appears on the remote after the
-- initial clone). All take the dblink connection string as a parameter; the
-- remote password is never persisted locally.
-- ---------------------------------------------------------------------------

-- Comma-joined, quoted list of writable (non-generated, non-dropped) columns of
-- a relation, in attribute order. Hydration paths use it instead of `SELECT *`
-- so they never try to write a STORED generated column (the local store keeps
-- such columns GENERATED and recomputes them locally).
CREATE OR REPLACE FUNCTION gfs_sync.writable_cols(p_relid regclass)
RETURNS text
LANGUAGE sql STABLE AS $fn$
  SELECT string_agg(quote_ident(attname), ', ' ORDER BY attnum)
    FROM pg_attribute
    WHERE attrelid = p_relid AND attnum > 0 AND NOT attisdropped AND attgenerated = '';
$fn$;

-- Copy-on-read warming primitive: EXPLAIN the query, find the foreign scans on
-- the overlay's remote side, and copy exactly the rows the remote was about to
-- serve (the predicate pushed by postgres_fdw) into the local store. Pure
-- optimisation; correctness is already guaranteed by the views. Best-effort:
-- selective predicates only (foreign scans without a pushed WHERE are skipped
-- so a full-table scan never silently pulls everything).
--
-- NOTE: this is NOT auto-invoked on reads. Applications connect straight to the
-- PostgreSQL connection string, not through `gfs query`, and PostgreSQL has no
-- SELECT trigger, so transparent warming must come from a wire-protocol proxy
-- (future). This function is the primitive such a proxy (or a background job,
-- or an explicit `gfs warm`) calls. Reads remain correct meanwhile (served live
-- from the remote when not yet local).
CREATE OR REPLACE FUNCTION gfs_sync.warm_for_query(p_sql text)
RETURNS integer
LANGUAGE plpgsql AS $warm$
DECLARE
  line        text;
  m           text[];
  cur_shadow  text := NULL;   -- e.g. gfs_remote_sales
  cur_tab     text := NULL;
  sch         text;
  whereclause text;
  keycols     text;
  collist     text;
  rc          integer;
  n           integer := 0;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (VERBOSE) ' || p_sql LOOP
    m := regexp_match(line, 'Foreign Scan on (gfs_remote_[A-Za-z0-9_]+)\.([A-Za-z0-9_]+)');
    IF m IS NOT NULL THEN
      cur_shadow := m[1];
      cur_tab    := m[2];
      CONTINUE;
    END IF;
    IF cur_shadow IS NOT NULL AND position('Remote SQL:' in line) > 0 THEN
      whereclause := substring(line from ' WHERE (.*)$');
      IF whereclause IS NOT NULL THEN
        whereclause := regexp_replace(whereclause, '\s+ORDER BY .*$', '');
        sch := substring(cur_shadow from 'gfs_remote_(.*)');
        SELECT key_cols INTO keycols
          FROM gfs_sync.table_meta WHERE schema_name = sch AND table_name = cur_tab;
        IF keycols IS NOT NULL THEN
          BEGIN
            collist := gfs_sync.writable_cols(format('%I.%I', cur_shadow, cur_tab)::regclass);
            EXECUTE format(
              'INSERT INTO %I.%I (%s) SELECT %s FROM %I.%I WHERE %s ON CONFLICT (%s) DO NOTHING',
              sch, cur_tab || '_local', collist, collist, cur_shadow, cur_tab, whereclause, keycols);
            GET DIAGNOSTICS rc = ROW_COUNT;
            n := n + rc;
          EXCEPTION WHEN others THEN
            NULL;  -- never let warming break the read
          END;
        END IF;
      END IF;
      cur_shadow := NULL;
      cur_tab := NULL;
    END IF;
  END LOOP;
  RETURN n;
END
$warm$;

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

-- Build the overlay for one table whose foreign table was imported into its
-- shadow schema: a local store, a delete-tombstone table, an updatable view
-- (local wins; remote rows only if neither local nor tombstoned), mirrored
-- defaults/sequences, and INSTEAD OF triggers for copy-on-write. `p_keycols`
-- is the (possibly composite) conflict key. Returns false (and is a no-op) if
-- the foreign table is missing. Reusable to overlay a single table on demand.
CREATE OR REPLACE FUNCTION gfs_sync.build_overlay(
  p_conn text, p_nsp text, p_tab text, p_keycols text[])
RETURNS boolean
LANGUAGE plpgsql AS $fn$
DECLARE
  shadow        text;
  fq_remote     text;  -- shadow-qualified foreign table, regclass-castable
  fname         text;  -- gfs_sync trigger-function name prefix
  collist       text;  -- all columns:            "a", "b", "c"
  newlist       text;  -- NEW per column:         NEW."a", NEW."b"
  setlist       text;  -- non-key upsert assigns: "c" = EXCLUDED."c"
  conflict_cols text;  -- key columns:            "a", "b"
  join_local    text;  -- l."a" = r."a" AND ...
  join_del      text;  -- d."a" = r."a" AND ...
  changed       text;  -- NEW."a" IS DISTINCT FROM OLD."a" OR ...
  where_old     text;  -- "a" = OLD."a" AND ...
  where_new     text;  -- "a" = NEW."a" AND ...
  old_vals      text;  -- OLD."a", OLD."b"
  del_cols_def  text;  -- "a" int, "b" text  (for the tombstone table)
  upsert        text;
  seqcol        record;
  seqname       text;
  startval      bigint;
  defcol        record;
BEGIN
  shadow    := 'gfs_remote_' || p_nsp;
  fq_remote := format('%I.%I', shadow, p_tab);
  fname     := p_nsp || '_' || p_tab;

  -- Skip tables whose foreign import was skipped (e.g. unavailable extension type).
  IF to_regclass(fq_remote) IS NULL THEN
    RAISE NOTICE 'gfs: no overlay for %.% (foreign table not imported)', p_nsp, p_tab;
    RETURN false;
  END IF;

  -- Per-key-column fragments (composite-aware), aligned by key ordinal.
  SELECT string_agg(quote_ident(kc), ', ' ORDER BY ord),
         string_agg('l.' || quote_ident(kc) || ' = r.' || quote_ident(kc), ' AND ' ORDER BY ord),
         string_agg('d.' || quote_ident(kc) || ' = r.' || quote_ident(kc), ' AND ' ORDER BY ord),
         string_agg('NEW.' || quote_ident(kc) || ' IS DISTINCT FROM OLD.' || quote_ident(kc), ' OR ' ORDER BY ord),
         string_agg(quote_ident(kc) || ' = OLD.' || quote_ident(kc), ' AND ' ORDER BY ord),
         string_agg(quote_ident(kc) || ' = NEW.' || quote_ident(kc), ' AND ' ORDER BY ord),
         string_agg('OLD.' || quote_ident(kc), ', ' ORDER BY ord)
    INTO conflict_cols, join_local, join_del, changed, where_old, where_new, old_vals
    FROM unnest(p_keycols) WITH ORDINALITY AS u(kc, ord);

  -- Tombstone table column definitions (key columns + their types).
  SELECT string_agg(format('%I %s', u.kc, format_type(a.atttypid, a.atttypmod)), ', ' ORDER BY u.ord)
    INTO del_cols_def
    FROM unnest(p_keycols) WITH ORDINALITY AS u(kc, ord)
    JOIN pg_attribute a ON a.attrelid = fq_remote::regclass AND a.attname = u.kc;

  -- Writable columns + non-key upsert SET list, from the imported foreign table.
  -- Generated columns are excluded: they can't be written and are recomputed by
  -- the local store (created with INCLUDING GENERATED below).
  SELECT string_agg(quote_ident(attname), ', ' ORDER BY attnum),
         string_agg('NEW.' || quote_ident(attname), ', ' ORDER BY attnum),
         string_agg(quote_ident(attname) || ' = EXCLUDED.' || quote_ident(attname),
                    ', ' ORDER BY attnum) FILTER (WHERE NOT (attname = ANY(p_keycols)))
    INTO collist, newlist, setlist
    FROM pg_attribute
    WHERE attrelid = fq_remote::regclass AND attnum > 0 AND NOT attisdropped
      AND attgenerated = '';

  upsert := CASE WHEN setlist IS NULL THEN 'DO NOTHING'
                 ELSE 'DO UPDATE SET ' || setlist END;

  -- Local authoritative store + delete tombstones (in the real schema).
  -- INCLUDING GENERATED so STORED generated columns recompute locally.
  EXECUTE format('CREATE TABLE %I.%I (LIKE %s INCLUDING DEFAULTS INCLUDING GENERATED)',
                 p_nsp, p_tab || '_local', fq_remote);
  EXECUTE format('ALTER TABLE %I.%I ADD PRIMARY KEY (%s)',
                 p_nsp, p_tab || '_local', conflict_cols);
  EXECUTE format('CREATE TABLE %I.%I (%s, PRIMARY KEY (%s))',
                 p_nsp, p_tab || '_deleted', del_cols_def, conflict_cols);

  -- Overlay view: local wins; remote only if neither local nor tombstoned.
  EXECUTE format(
    'CREATE VIEW %I.%I AS '
    || 'SELECT * FROM %I.%I '
    || 'UNION ALL '
    || 'SELECT r.* FROM %s r '
    || ' WHERE NOT EXISTS (SELECT 1 FROM %I.%I l WHERE %s) '
    || '   AND NOT EXISTS (SELECT 1 FROM %I.%I d WHERE %s)',
    p_nsp, p_tab, p_nsp, p_tab || '_local', fq_remote,
    p_nsp, p_tab || '_local', join_local,
    p_nsp, p_tab || '_deleted', join_del);

  -- Surface the overlay nature in tooling (\d+) and guide DDL away from the view.
  EXECUTE format('COMMENT ON VIEW %I.%I IS %L', p_nsp, p_tab, format(
    'GFS lazy-clone overlay of %I.%I (local wins over remote). DDL/indexes: target %I.%I or the source. SELECT ... FOR UPDATE is not supported on this view.',
    p_nsp, p_tab, p_nsp, p_tab || '_local'));

  -- Auto-increment fidelity: IMPORT FOREIGN SCHEMA does not bring identity/
  -- serial defaults, so local INSERTs omitting the key would fail. For each
  -- auto-increment column, create a local sequence starting just past the
  -- remote's current max and set it as the default on BOTH the view (applied
  -- to NEW before the INSTEAD OF trigger fires) and the local store. Local
  -- inserts then work and never collide with existing remote keys.
  FOR seqcol IN
    SELECT * FROM dblink(p_conn, format($seq$
      SELECT a.attname::text
      FROM pg_attribute a
      JOIN pg_class c     ON c.oid = a.attrelid
      JOIN pg_namespace n ON n.oid = c.relnamespace
      LEFT JOIN pg_attrdef ad ON ad.adrelid = c.oid AND ad.adnum = a.attnum
      WHERE n.nspname = %L AND c.relname = %L
        AND a.attnum > 0 AND NOT a.attisdropped
        AND ( a.attidentity IN ('a','d')
              OR (ad.adbin IS NOT NULL AND pg_get_expr(ad.adbin, ad.adrelid) LIKE 'nextval(%%') )
    $seq$, p_nsp, p_tab)) AS r(col text)
  LOOP
    SELECT mx INTO startval FROM dblink(p_conn,
      format('SELECT COALESCE(max(%I),0)+1 FROM %I.%I', seqcol.col, p_nsp, p_tab))
      AS r(mx bigint);
    seqname := p_tab || '_' || seqcol.col || '_gfsseq';
    EXECUTE format('CREATE SEQUENCE %I.%I START WITH %s', p_nsp, seqname, startval);
    EXECUTE format('ALTER TABLE %I.%I ALTER COLUMN %I SET DEFAULT nextval(%L)',
                   p_nsp, p_tab || '_local', seqcol.col,
                   format('%I.%I', p_nsp, seqname));
    EXECUTE format('ALTER VIEW %I.%I ALTER COLUMN %I SET DEFAULT nextval(%L)',
                   p_nsp, p_tab, seqcol.col,
                   format('%I.%I', p_nsp, seqname));
  END LOOP;

  -- Plain defaults (now(), uuid_generate_v4(), constants, ...): IMPORT FOREIGN
  -- SCHEMA drops column DEFAULTs, so an app relying on a NOT NULL DEFAULT now()
  -- column would insert NULL and fail. Mirror the remote's defaults onto BOTH
  -- the view (applied to NEW before the INSTEAD OF trigger fires) and the local
  -- store (for direct inserts). Sequence/identity defaults are handled above.
  -- Best-effort: a default that won't resolve locally is skipped, not fatal.
  FOR defcol IN
    SELECT * FROM dblink(p_conn, format($def$
      SELECT a.attname::text, pg_get_expr(ad.adbin, ad.adrelid)::text
      FROM pg_attribute a
      JOIN pg_class c     ON c.oid = a.attrelid
      JOIN pg_namespace n ON n.oid = c.relnamespace
      JOIN pg_attrdef ad  ON ad.adrelid = c.oid AND ad.adnum = a.attnum
      WHERE n.nspname = %L AND c.relname = %L
        AND a.attnum > 0 AND NOT a.attisdropped
        AND a.attidentity NOT IN ('a','d')
        AND pg_get_expr(ad.adbin, ad.adrelid) NOT LIKE 'nextval(%%'
    $def$, p_nsp, p_tab)) AS r(col text, def text)
  LOOP
    BEGIN
      EXECUTE format('ALTER VIEW %I.%I ALTER COLUMN %I SET DEFAULT %s',
                     p_nsp, p_tab, defcol.col, defcol.def);
      EXECUTE format('ALTER TABLE %I.%I ALTER COLUMN %I SET DEFAULT %s',
                     p_nsp, p_tab || '_local', defcol.col, defcol.def);
    EXCEPTION WHEN others THEN
      RAISE NOTICE 'gfs: could not mirror default for %.%.% (%); leaving unset',
        p_nsp, p_tab, defcol.col, SQLERRM;
    END;
  END LOOP;

  -- INSTEAD OF INSERT: upsert into the local store; clear any tombstone.
  EXECUTE format(
    'CREATE FUNCTION gfs_sync.%I() RETURNS trigger LANGUAGE plpgsql AS $body$ '
    || 'BEGIN '
    || '  INSERT INTO %I.%I (%s) VALUES (%s) ON CONFLICT (%s) %s; '
    || '  DELETE FROM %I.%I WHERE %s; '
    || '  RETURN NEW; END $body$',
    fname || '_ins', p_nsp, p_tab || '_local', collist, newlist, conflict_cols, upsert,
    p_nsp, p_tab || '_deleted', where_new);
  EXECUTE format('CREATE TRIGGER %I INSTEAD OF INSERT ON %I.%I '
                 || 'FOR EACH ROW EXECUTE FUNCTION gfs_sync.%I()',
                 p_tab || '_ins_trg', p_nsp, p_tab, fname || '_ins');

  -- INSTEAD OF UPDATE: copy-on-write upsert; if the key changed, tombstone old.
  EXECUTE format(
    'CREATE FUNCTION gfs_sync.%I() RETURNS trigger LANGUAGE plpgsql AS $body$ '
    || 'BEGIN '
    || '  INSERT INTO %I.%I (%s) VALUES (%s) ON CONFLICT (%s) %s; '
    || '  IF %s THEN '
    || '    DELETE FROM %I.%I WHERE %s; '
    || '    INSERT INTO %I.%I (%s) VALUES (%s) ON CONFLICT DO NOTHING; '
    || '  END IF; '
    || '  RETURN NEW; END $body$',
    fname || '_upd', p_nsp, p_tab || '_local', collist, newlist, conflict_cols, upsert,
    changed,
    p_nsp, p_tab || '_local', where_old,
    p_nsp, p_tab || '_deleted', conflict_cols, old_vals);
  EXECUTE format('CREATE TRIGGER %I INSTEAD OF UPDATE ON %I.%I '
                 || 'FOR EACH ROW EXECUTE FUNCTION gfs_sync.%I()',
                 p_tab || '_upd_trg', p_nsp, p_tab, fname || '_upd');

  -- INSTEAD OF DELETE: remove from local store and tombstone the key.
  EXECUTE format(
    'CREATE FUNCTION gfs_sync.%I() RETURNS trigger LANGUAGE plpgsql AS $body$ '
    || 'BEGIN '
    || '  DELETE FROM %I.%I WHERE %s; '
    || '  INSERT INTO %I.%I (%s) VALUES (%s) ON CONFLICT DO NOTHING; '
    || '  RETURN OLD; END $body$',
    fname || '_del', p_nsp, p_tab || '_local', where_old,
    p_nsp, p_tab || '_deleted', conflict_cols, old_vals);
  EXECUTE format('CREATE TRIGGER %I INSTEAD OF DELETE ON %I.%I '
                 || 'FOR EACH ROW EXECUTE FUNCTION gfs_sync.%I()',
                 p_tab || '_del_trg', p_nsp, p_tab, fname || '_del');

  INSERT INTO gfs_sync.table_meta(schema_name, table_name, key_cols)
    VALUES (p_nsp, p_tab, conflict_cols)
    ON CONFLICT (schema_name, table_name) DO NOTHING;

  RETURN true;
END
$fn$;

-- ---------------------------------------------------------------------------
-- Network elision (optional optimisation; correctness never depends on it).
--
-- After a contiguous key range is hydrated into the local store we record it and
-- attach a CHECK to the foreign table declaring it holds ONLY rows OUTSIDE the
-- cached ranges. With constraint_exclusion = on, a query whose key predicate
-- lands inside a cached range is refuted and its foreign scan is pruned — so the
-- remote is never contacted for already-local rows. The CHECK over-states the
-- data on purpose, but is used ONLY for pruning; unconstrained scans still read
-- the foreign table and the overlay's NOT EXISTS anti-join keeps them correct
-- (validated in docs/rfcs/008-remote-clone/poc-elision/).
--
-- Range elision needs a single-column, orderable key; composite-key tables keep
-- the correct overlay but no elision. SECURITY DEFINER so a low-privilege role
-- (proxy / cron) can drive warming without owning the objects.
-- ---------------------------------------------------------------------------

-- Recompute the foreign table's exclusion CHECK from gfs_sync.cached_range.
CREATE OR REPLACE FUNCTION gfs_sync.rebuild_exclusion(p_nsp text, p_tab text)
RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $fn$
DECLARE
  fq      text := format('%I.%I', 'gfs_remote_' || p_nsp, p_tab);
  keycol  text;
  keytype text;
  conj    text;
BEGIN
  IF to_regclass(fq) IS NULL THEN RETURN; END IF;

  -- Whole-table cache (any key, incl. composite/uuid): serve entirely from the
  -- local store by dropping the foreign branch from the view. This guarantees no
  -- foreign scan is ever planned, even for unconstrained queries — more robust
  -- than CHECK-based pruning (which only refutes query predicates). The INSTEAD
  -- OF triggers (writes) live on the view and are preserved by CREATE OR REPLACE.
  IF EXISTS (SELECT 1 FROM gfs_sync.fully_cached
               WHERE schema_name = p_nsp AND table_name = p_tab) THEN
    EXECUTE format('ALTER FOREIGN TABLE %s DROP CONSTRAINT IF EXISTS gfs_excl', fq);
    EXECUTE format('CREATE OR REPLACE VIEW %I.%I AS SELECT * FROM %I.%I',
                   p_nsp, p_tab, p_nsp, p_tab || '_local');
    RETURN;
  END IF;

  SELECT key_cols INTO keycol FROM gfs_sync.table_meta
    WHERE schema_name = p_nsp AND table_name = p_tab;
  -- Single-column key only (stored conflict key has no comma); it is quoted.
  IF keycol IS NULL OR position(',' in keycol) > 0 THEN RETURN; END IF;
  keycol := btrim(keycol, '"');

  SELECT format_type(a.atttypid, a.atttypmod) INTO keytype
    FROM pg_attribute a WHERE a.attrelid = fq::regclass AND a.attname = keycol;
  IF keytype IS NULL THEN RETURN; END IF;

  -- "(k < lo OR k > hi) AND ..." over every cached range (ANDing overlapping
  -- ranges still correctly excludes their union; no coalescing needed).
  SELECT string_agg(
           format('(%I < %L::%s OR %I > %L::%s)', keycol, lo, keytype, keycol, hi, keytype),
           ' AND ')
    INTO conj
    FROM gfs_sync.cached_range
    WHERE schema_name = p_nsp AND table_name = p_tab;

  EXECUTE format('ALTER FOREIGN TABLE %s DROP CONSTRAINT IF EXISTS gfs_excl', fq);
  IF conj IS NOT NULL THEN
    EXECUTE format('ALTER FOREIGN TABLE %s ADD CONSTRAINT gfs_excl CHECK (%s)', fq, conj);
  END IF;
END
$fn$;

-- Hydrate [p_lo, p_hi] of the key into the local store and record the range.
-- Returns the number of rows hydrated. No-op for composite or missing keys (the
-- overlay stays correct, just not elided).
--
-- It does NOT rebuild the exclusion CHECK: that is decoupled into
-- gfs_sync.refresh_exclusions() so the AccessExclusive ALTER runs periodically
-- (coalesced) instead of on every warm, avoiding read-blocking lock contention.
-- A hydrated range is served correctly meanwhile (live), just not yet elided.
CREATE OR REPLACE FUNCTION gfs_sync.warm_range(p_nsp text, p_tab text, p_lo text, p_hi text)
RETURNS bigint
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $fn$
DECLARE
  fq      text := format('%I.%I', 'gfs_remote_' || p_nsp, p_tab);
  keycol  text;
  keytype text;
  collist text;
  rc      bigint := 0;
BEGIN
  IF to_regclass(fq) IS NULL THEN
    RAISE NOTICE 'gfs: warm_range: no overlay for %.%', p_nsp, p_tab;
    RETURN 0;
  END IF;

  SELECT key_cols INTO keycol FROM gfs_sync.table_meta
    WHERE schema_name = p_nsp AND table_name = p_tab;
  IF keycol IS NULL OR position(',' in keycol) > 0 THEN
    RAISE NOTICE 'gfs: warm_range: %.% has no single-column key; range elision unsupported',
      p_nsp, p_tab;
    RETURN 0;
  END IF;
  keycol := btrim(keycol, '"');

  SELECT format_type(a.atttypid, a.atttypmod) INTO keytype
    FROM pg_attribute a WHERE a.attrelid = fq::regclass AND a.attname = keycol;

  -- Non-generated columns only (the local store keeps generated columns GENERATED
  -- and recomputes them locally).
  collist := gfs_sync.writable_cols(fq::regclass);

  EXECUTE format(
    'INSERT INTO %I.%I (%s) SELECT %s FROM %s WHERE %I BETWEEN %L::%s AND %L::%s ON CONFLICT DO NOTHING',
    p_nsp, p_tab || '_local', collist, collist, fq, keycol, p_lo, keytype, p_hi, keytype);
  GET DIAGNOSTICS rc = ROW_COUNT;

  INSERT INTO gfs_sync.cached_range(schema_name, table_name, lo, hi)
    VALUES (p_nsp, p_tab, p_lo, p_hi)
    ON CONFLICT (schema_name, table_name, lo, hi) DO NOTHING;

  RETURN rc;  -- exclusion CHECK is (re)built later by refresh_exclusions()
END
$fn$;

GRANT EXECUTE ON FUNCTION gfs_sync.warm_range(text, text, text, text) TO PUBLIC;

-- Hydrate an ENTIRE table into the local store and mark it fully cached, for
-- keys that can't be range-chunked (random uuid, text, composite). The exclusion
-- CHECK (false) is applied later by refresh_exclusions(). Returns rows hydrated.
-- Caller is responsible for only doing this on tables small enough to copy.
CREATE OR REPLACE FUNCTION gfs_sync.warm_whole_table(p_nsp text, p_tab text)
RETURNS bigint
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $fn$
DECLARE
  fq      text := format('%I.%I', 'gfs_remote_' || p_nsp, p_tab);
  collist text;
  rc      bigint := 0;
BEGIN
  IF to_regclass(fq) IS NULL THEN
    RAISE NOTICE 'gfs: warm_whole_table: no overlay for %.%', p_nsp, p_tab;
    RETURN 0;
  END IF;
  collist := gfs_sync.writable_cols(fq::regclass);
  EXECUTE format('INSERT INTO %I.%I (%s) SELECT %s FROM %s ON CONFLICT DO NOTHING',
                 p_nsp, p_tab || '_local', collist, collist, fq);
  GET DIAGNOSTICS rc = ROW_COUNT;
  INSERT INTO gfs_sync.fully_cached(schema_name, table_name)
    VALUES (p_nsp, p_tab) ON CONFLICT DO NOTHING;
  RETURN rc;  -- exclusion CHECK (false) applied by refresh_exclusions()
END
$fn$;

GRANT EXECUTE ON FUNCTION gfs_sync.warm_whole_table(text, text) TO PUBLIC;

-- Rebuild the exclusion CHECK for every table with cached ranges. Decoupled from
-- warm_range so the read-blocking AccessExclusive ALTER runs periodically and
-- coalesced (called by the proxy / a job on a timer), not on every warm.
-- `lock_timeout` bounds any read stall: a table it can't lock now is retried on
-- the next call. Safe because cached_range is written in the same transaction as
-- the hydration, so a rebuild only ever sees already-hydrated ranges.
-- `client_min_messages = warning` keeps the routine `IF EXISTS` maintenance
-- (DROP TEMP TABLE / DROP CONSTRAINT) from spamming NOTICEs to the caller.
CREATE OR REPLACE FUNCTION gfs_sync.refresh_exclusions()
RETURNS integer
LANGUAGE plpgsql SECURITY DEFINER
  SET search_path = pg_catalog, pg_temp
  SET client_min_messages = 'warning' AS $fn$
DECLARE
  rec      record;
  cur_sig  text;
  prev_sig text;
  n        integer := 0;
BEGIN
  SET LOCAL lock_timeout = '200ms';
  FOR rec IN
    SELECT schema_name, table_name FROM gfs_sync.cached_range
    UNION
    SELECT schema_name, table_name FROM gfs_sync.fully_cached
  LOOP
    cur_sig := gfs_sync.exclusion_sig(rec.schema_name, rec.table_name);
    SELECT sig INTO prev_sig FROM gfs_sync.applied_exclusion
      WHERE schema_name = rec.schema_name AND table_name = rec.table_name;
    -- Nothing changed since we last applied the CHECK → skip (no coalesce, no ALTER).
    CONTINUE WHEN prev_sig IS NOT DISTINCT FROM cur_sig;

    BEGIN
      -- Coalescing is an optimization; isolate it so a failure never blocks the
      -- rebuild (which is what actually applies elision).
      BEGIN
        PERFORM gfs_sync.coalesce_ranges(rec.schema_name, rec.table_name);
      EXCEPTION WHEN others THEN NULL;
      END;
      PERFORM gfs_sync.rebuild_exclusion(rec.schema_name, rec.table_name);
      -- Record the post-coalesce signature so the next tick is a no-op.
      INSERT INTO gfs_sync.applied_exclusion(schema_name, table_name, sig)
        VALUES (rec.schema_name, rec.table_name,
                gfs_sync.exclusion_sig(rec.schema_name, rec.table_name))
        ON CONFLICT (schema_name, table_name) DO UPDATE SET sig = EXCLUDED.sig;
      n := n + 1;
    EXCEPTION
      WHEN lock_not_available THEN NULL;  -- table busy; retried next call
      WHEN others THEN NULL;              -- best-effort
    END;
  END LOOP;
  RETURN n;
END
$fn$;

-- Deterministic signature of a table's cached state (ranges + whole-table flag).
CREATE OR REPLACE FUNCTION gfs_sync.exclusion_sig(p_nsp text, p_tab text)
RETURNS text
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $fn$
  SELECT coalesce(
           (SELECT string_agg(lo || ':' || hi, ',' ORDER BY lo, hi)
              FROM gfs_sync.cached_range WHERE schema_name = p_nsp AND table_name = p_tab),
           '')
         || CASE WHEN EXISTS (SELECT 1 FROM gfs_sync.fully_cached
                                WHERE schema_name = p_nsp AND table_name = p_tab)
                 THEN '|W' ELSE '' END
$fn$;

GRANT EXECUTE ON FUNCTION gfs_sync.refresh_exclusions() TO PUBLIC;

-- Merge overlapping/adjacent cached ranges for a table into the minimal set, so
-- the exclusion CHECK stays compact (and planning fast) as ranges accumulate.
-- Sorts/merges in the key's type; integer keys also merge adjacency
-- ([0,999] ∪ [1000,1999] = [0,1999]). Called by refresh_exclusions().
CREATE OR REPLACE FUNCTION gfs_sync.coalesce_ranges(p_nsp text, p_tab text)
RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $fn$
DECLARE
  fq      text := format('%I.%I', 'gfs_remote_' || p_nsp, p_tab);
  keycol  text;
  keytype text;
  adj     text;
BEGIN
  IF to_regclass(fq) IS NULL THEN RETURN; END IF;
  SELECT key_cols INTO keycol FROM gfs_sync.table_meta
    WHERE schema_name = p_nsp AND table_name = p_tab;
  IF keycol IS NULL OR position(',' in keycol) > 0 THEN RETURN; END IF;
  keycol := btrim(keycol, '"');
  SELECT format_type(a.atttypid, a.atttypmod) INTO keytype
    FROM pg_attribute a WHERE a.attrelid = fq::regclass AND a.attname = keycol;
  IF keytype IS NULL THEN RETURN; END IF;
  -- Coalescing only applies to integer keys (the only ones we range-chunk); it
  -- also needs min()/max() on the key type, which non-integer types may lack.
  IF keytype NOT IN ('smallint', 'integer', 'bigint') THEN RETURN; END IF;
  adj := ' + 1';  -- integer keys merge adjacency ([0,999] ∪ [1000,1999])

  -- Gaps-and-islands merge into a temp table, then replace the table's ranges in
  -- separate statements. (A single DELETE+INSERT would let an unchanged merged
  -- range collide with the row being deleted under ON CONFLICT, emptying it.)
  DROP TABLE IF EXISTS _gfs_coalesce;
  EXECUTE format($q$
    CREATE TEMP TABLE _gfs_coalesce ON COMMIT DROP AS
      WITH r AS (
        SELECT (lo)::%1$s AS klo, (hi)::%1$s AS khi
        FROM gfs_sync.cached_range WHERE schema_name = %2$L AND table_name = %3$L
      ),
      ord AS (
        SELECT klo, khi,
               max(khi) OVER (ORDER BY klo, khi
                              ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) AS prev_max
        FROM r
      ),
      grp AS (
        SELECT klo, khi,
               count(*) FILTER (WHERE prev_max IS NULL OR klo > prev_max%4$s)
                 OVER (ORDER BY klo, khi) AS g
        FROM ord
      )
      SELECT min(klo)::text AS lo, max(khi)::text AS hi FROM grp GROUP BY g
  $q$, keytype, p_nsp, p_tab, adj);

  DELETE FROM gfs_sync.cached_range WHERE schema_name = p_nsp AND table_name = p_tab;
  INSERT INTO gfs_sync.cached_range(schema_name, table_name, lo, hi)
    SELECT p_nsp, p_tab, lo, hi FROM _gfs_coalesce;
  DROP TABLE _gfs_coalesce;
END
$fn$;

GRANT EXECUTE ON FUNCTION gfs_sync.coalesce_ranges(text, text) TO PUBLIC;

-- Query-driven warming entry point (what a proxy/cron calls with the read SQL).
-- EXPLAINs the query, and for each foreign scan with a pushed predicate:
--   * integer single-column key → expand the touched key span to chunk
--     boundaries and warm_range() each chunk (enables elision, and nearby keys
--     in the chunk become local too);
--   * otherwise → copy exactly the predicate's rows into the local store
--     (ownership only, like warm_for_query; no range elision).
-- The key span is measured on the remote (min/max over the pushed predicate),
-- so we never parse the SQL. Best-effort; broad predicates spanning more than
-- p_maxchunks chunks are skipped (a future whole_table strategy can cover those).
-- v1 chunking assumes non-negative integer keys.
-- SECURITY INVOKER (default): the EXPLAIN must resolve the caller's unqualified
-- table names via the caller's search_path. The privileged DDL is encapsulated
-- in warm_range (SECURITY DEFINER), so this function needs only SELECT (to
-- EXPLAIN) + EXECUTE on warm_range.
CREATE OR REPLACE FUNCTION gfs_sync.warm_query_chunks(
  p_sql text, p_chunk bigint DEFAULT 100000, p_maxchunks int DEFAULT 64,
  p_whole_max bigint DEFAULT 50000)
RETURNS integer
LANGUAGE plpgsql AS $fn$
DECLARE
  line text; m text[];
  cur_shadow text := NULL; cur_tab text := NULL;
  sch text; whereclause text; keycols text; keycol text; keytype text; collist text;
  kmin bigint; kmax bigint; c bigint; cnt bigint; rc integer; n integer := 0;
BEGIN
  FOR line IN EXECUTE 'EXPLAIN (VERBOSE) ' || p_sql LOOP
    m := regexp_match(line, 'Foreign Scan on (gfs_remote_[A-Za-z0-9_]+)\.([A-Za-z0-9_]+)');
    IF m IS NOT NULL THEN cur_shadow := m[1]; cur_tab := m[2]; CONTINUE; END IF;
    IF cur_shadow IS NULL OR position('Remote SQL:' in line) = 0 THEN CONTINUE; END IF;

    whereclause := substring(line from ' WHERE (.*)$');
    IF whereclause IS NOT NULL THEN
      whereclause := regexp_replace(whereclause, '\s+ORDER BY .*$', '');
      sch := substring(cur_shadow from 'gfs_remote_(.*)');
      SELECT key_cols INTO keycols
        FROM gfs_sync.table_meta WHERE schema_name = sch AND table_name = cur_tab;

      -- Single-column, integer key → chunk-warm (elision); else copy rows.
      keycol := NULL;
      IF keycols IS NOT NULL AND position(',' in keycols) = 0 THEN
        keycol := btrim(keycols, '"');
        SELECT format_type(a.atttypid, a.atttypmod) INTO keytype
          FROM pg_attribute a
          WHERE a.attrelid = format('%I.%I', cur_shadow, cur_tab)::regclass
            AND a.attname = keycol;
      END IF;

      BEGIN
        IF keytype IN ('smallint', 'integer', 'bigint') THEN
          EXECUTE format('SELECT min(%I)::bigint, max(%I)::bigint FROM %I.%I WHERE %s',
                         keycol, keycol, cur_shadow, cur_tab, whereclause)
            INTO kmin, kmax;
          IF kmin IS NOT NULL
             AND (kmax / p_chunk - kmin / p_chunk) < p_maxchunks THEN
            c := (kmin / p_chunk) * p_chunk;
            WHILE c <= kmax LOOP
              PERFORM gfs_sync.warm_range(sch, cur_tab, c::text, (c + p_chunk - 1)::text);
              n := n + 1;
              c := c + p_chunk;
            END LOOP;
          END IF;
        ELSIF keycols IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM gfs_sync.fully_cached
                                WHERE schema_name = sch AND table_name = cur_tab) THEN
          -- Non-range-able key (uuid/text/composite). If the table is small
          -- enough, cache it whole (enables elision via CHECK (false)); else
          -- copy just the predicate's rows (ownership only). The size probe is
          -- bounded by LIMIT so it never scans a huge table.
          EXECUTE format('SELECT count(*) FROM (SELECT 1 FROM %I.%I LIMIT %s) s',
                         cur_shadow, cur_tab, p_whole_max + 1) INTO cnt;
          IF cnt <= p_whole_max THEN
            PERFORM gfs_sync.warm_whole_table(sch, cur_tab);
            n := n + 1;
          ELSE
            collist := gfs_sync.writable_cols(format('%I.%I', cur_shadow, cur_tab)::regclass);
            EXECUTE format(
              'INSERT INTO %I.%I (%s) SELECT %s FROM %I.%I WHERE %s ON CONFLICT (%s) DO NOTHING',
              sch, cur_tab || '_local', collist, collist, cur_shadow, cur_tab, whereclause, keycols);
            GET DIAGNOSTICS rc = ROW_COUNT;
            n := n + rc;
          END IF;
        END IF;
      EXCEPTION WHEN others THEN
        NULL;  -- never let warming break anything
      END;
    END IF;
    cur_shadow := NULL; cur_tab := NULL;
  END LOOP;
  RETURN n;
END
$fn$;

GRANT EXECUTE ON FUNCTION gfs_sync.warm_query_chunks(text, bigint, int, bigint) TO PUBLIC;

-- Orchestrator: resolve the schemas to mirror (all non-system schemas when
-- p_schemas IS NULL), mirror extensions and types, import each schema's tables
-- into its shadow, then build an overlay for every table with a primary key (or
-- a unique index), single-column or composite. Tables with no usable key are
-- skipped. Re-runnable / callable to (re)build the clone.
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
    PERFORM gfs_sync.build_overlay(p_conn, rec.nsp, rec.tab, rec.keycols);
  END LOOP;
END
$fn$;

-- Run the clone.
SELECT gfs_sync.clone('__CONN__', __SCHEMAS_ARRAY__);
