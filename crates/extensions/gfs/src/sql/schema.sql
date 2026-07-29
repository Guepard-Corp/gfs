CREATE SCHEMA gfs;
COMMENT ON SCHEMA gfs IS 'GFS clone catalog + API (the planner hook reads clone_source/cached; stats in clone_stats)';

CREATE TABLE gfs.clone_source (
    relid        regclass PRIMARY KEY,
    source_ref   text     NOT NULL,
    key_col      text     NOT NULL DEFAULT 'id',
    chunk_kind   text     NOT NULL DEFAULT 'whole',  -- 'int' (int range key) | 'time' (date/timestamp range key) | 'whole'
    whole_cached boolean  NOT NULL DEFAULT false,
    source_rows  bigint   NOT NULL DEFAULT 0,        -- Tr: source size (cost model)
    row_bytes    int      NOT NULL DEFAULT 100,      -- B: avg bytes/row
    access_count bigint   NOT NULL DEFAULT 0,        -- H: query frequency (amortization)
    partial_rows bigint   NOT NULL DEFAULT 0,        -- cumulative rows pulled by COMMITTED partial hydrations
    no_partial   boolean  NOT NULL DEFAULT false,    -- terminal: too big to own; federate per call, no more probes
    has_local_writes boolean NOT NULL DEFAULT false  -- set when a local INSERT/UPDATE/DELETE diverges this table from
                                                     -- the source. A diverged table must NOT be federated (the source
                                                     -- would not reflect the local write); the router whole-owns it and
                                                     -- serves local instead. See gfs_mark_local_write / relation_diverged.
);
COMMENT ON TABLE gfs.clone_source IS 'Per clone table: source ref, range key, ownership, and cost-model stats';

-- Cost/energy weights for the hydrate-vs-federate router (single row, tunable).
-- E(own)   = net * bytes_pulled              (one-time)
-- E(feder) = source * rows_scanned_at_source (per call; incl. prod-load penalty)
-- Own when E(own) <= negligible, or amortized over <= horizon future calls.
CREATE TABLE gfs.cost (
    net        float8 NOT NULL DEFAULT 1,           -- MEASURED: seconds per byte pulled (network)
    source     float8 NOT NULL DEFAULT 20,          -- MEASURED: seconds per row the source scans
    negligible float8 NOT NULL DEFAULT 100000,      -- MEASURED: one round-trip (own if cheaper)
    ceiling    float8 NOT NULL DEFAULT 1000000000,  -- POLICY: never own above this (capacity cap)
    horizon    float8 NOT NULL DEFAULT 1000,        -- POLICY: cap on H (expected future calls)
    prod_load  float8 NOT NULL DEFAULT 1,           -- POLICY: penalty multiplier on source scans (offload prod)
    -- PARTIAL hydration is now COST-COMPUTED (no flag): it is the third leg of the
    -- router, reachable ONLY for a table that is NOT whole-ownable (too big) AND
    -- whose predicate slice is selective enough to fit the budget below. These are
    -- policy knobs in the same class as ceiling/horizon.
    partial_max_frac  float8 NOT NULL DEFAULT 0.05, -- POLICY: max slice fraction S/Tr to partial-own;
                                                    --   ALSO the hard real-pull cap (LIMIT ceil(frac*Tr)+1).
    promote_frac      float8 NOT NULL DEFAULT 0.5,  -- POLICY: cumulative partial-pulled fraction of Tr at which
                                                    --   piecemeal slices auto-promote to ONE whole-own.
    max_partial_preds int    NOT NULL DEFAULT 10,   -- POLICY: max distinct partial predicates (CONTACTS) before
                                                    --   promote; bounds tiny-slice floods the row cap can't see.
    -- PARALLEL BACKFILL: a large whole/int-range fetch fans the source scan over N
    -- concurrent dblink connections (CTID-block for whole, key-range split for a
    -- range) instead of one FDW cursor. Pure read; no slot. parallel_workers=1
    -- disables it entirely (hot kill-switch, no redeploy).
    parallel_workers   int    NOT NULL DEFAULT 4,    -- POLICY: N concurrent dblink scans (1 = disabled; hard-capped in code)
    parallel_min_pages bigint NOT NULL DEFAULT 4096, -- POLICY: est. source heap pages above which we parallelize (~32MB @ 8KB)
    parallel_min_frac  float8 NOT NULL DEFAULT 0.5   -- POLICY: a RANGE fetch parallelizes only when its key span covers > this fraction of Tr
);
INSERT INTO gfs.cost DEFAULT VALUES;
COMMENT ON TABLE gfs.cost IS 'Router weights: net/source/negligible are MEASURED by gfs.calibrate(); ceiling/horizon/prod_load are policy';

-- Prod protection: a token bucket capping this clone's rate of SOURCE contact
-- (hydrate fetches + federated queries). 100s of clones must not hammer the prod
-- source -- set max_rate = total_prod_budget / expected_clones. The hook waits
-- (back-pressure) when out of tokens; it NEVER serves a wrong/partial result.
CREATE TABLE gfs.budget (
    max_rate float8       NOT NULL DEFAULT 0,   -- source contacts/sec allowed (0 = unlimited)
    tokens   float8       NOT NULL DEFAULT 0,
    ts       timestamptz  NOT NULL DEFAULT clock_timestamp()
);
INSERT INTO gfs.budget DEFAULT VALUES;
COMMENT ON TABLE gfs.budget IS 'Per-clone source-contact rate limit (token bucket); protects the prod source';

-- Consume one token; return the seconds the caller must wait (0 if available).
CREATE FUNCTION gfs.take_token() RETURNS float8 LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp AS $$
DECLARE rate float8; tok float8; last timestamptz; elapsed float8; wait float8 := 0;
BEGIN
    SELECT max_rate, tokens, ts INTO rate, tok, last FROM gfs.budget FOR UPDATE;
    IF rate IS NULL OR rate <= 0 THEN RETURN 0; END IF;            -- unlimited
    elapsed := GREATEST(extract(epoch FROM clock_timestamp() - last), 0);
    tok := LEAST(rate, tok + rate * elapsed);                       -- refill (1s bucket)
    IF tok >= 1 THEN
        UPDATE gfs.budget SET tokens = tok - 1, ts = clock_timestamp();
    ELSE
        wait := (1 - tok) / rate;
        UPDATE gfs.budget SET tokens = 0, ts = clock_timestamp();
    END IF;
    RETURN wait;
END;
$$;
COMMENT ON FUNCTION gfs.take_token() IS 'Token-bucket gate for source contact; returns seconds to wait';

-- Auto-calibrate the cost weights by probing the source over the live FDW link:
-- network throughput (sec/byte), source scan rate (sec/row), round-trip latency.
-- The hydrate-vs-federate flip then self-tunes to the real link + source speed.
-- Run at clone time and periodically (load/throughput drift).
CREATE FUNCTION gfs.calibrate(sample int DEFAULT 5000) RETURNS gfs.cost
LANGUAGE plpgsql AS $$
DECLARE fref text; tr bigint; b int; t0 timestamptz; t1 timestamptz;
        lat float8; net_s float8; src_s float8; pl float8; scanned bigint; r gfs.cost;
BEGIN
    -- probe the largest registered source (network + source speed are global)
    SELECT source_ref, GREATEST(source_rows,1), GREATEST(row_bytes,1)
      INTO fref, tr, b
      FROM gfs.clone_source WHERE to_regclass(source_ref) IS NOT NULL
     ORDER BY source_rows DESC LIMIT 1;
    IF fref IS NULL THEN RETURN (SELECT c FROM gfs.cost c LIMIT 1); END IF;
    SELECT prod_load INTO pl FROM gfs.cost LIMIT 1;

    -- Warm the postgres_fdw connection first. The first remote call pays a one-time
    -- connect + TLS handshake cost (often ~1s); measuring latency on it would inflate
    -- the baseline so far that the later data probe's (duration - lat) underflows to
    -- ~0 and the link looks free (issue #112). Discard this call's timing.
    EXECUTE format('SELECT 1 FROM %s LIMIT 1', fref);

    t0 := clock_timestamp();
    EXECUTE format('SELECT 1 FROM %s LIMIT 1', fref);   -- latency: warm round-trip
    t1 := clock_timestamp();
    lat := GREATEST(extract(epoch FROM t1 - t0), 1e-6);

    -- Pull `sample` rows of REAL column data over the link and time it. We must
    -- reference every column or postgres_fdw column-prunes the foreign scan down to
    -- `SELECT NULL FROM <src> LIMIT n`, transferring nothing and mismeasuring the
    -- link as ~free (issue #112). Casting the whole row to text forces all columns
    -- onto the wire; sum(octet_length(...)) is a cheap local sink the FDW cannot
    -- push down past the LIMIT, so the timing reflects an actual download.
    t0 := clock_timestamp();                                   -- pull `sample` rows (real bytes)
    EXECUTE format('SELECT sum(octet_length(t::text)) FROM (SELECT * FROM %s LIMIT %s) t', fref, sample);
    t1 := clock_timestamp();
    net_s := GREATEST(extract(epoch FROM t1 - t0) - lat, 0) / GREATEST(sample * b, 1);
    net_s := GREATEST(net_s, 1e-10);   -- floor at a sane minimum (~10 GB/s); a degenerate
                                       -- probe must never report the link as ~free and
                                       -- stampede whole-table copies over a slow link.

    t0 := clock_timestamp();                                   -- source scans up to `sample` rows
    EXECUTE format('SELECT count(*) FROM (SELECT 1 FROM %s LIMIT %s) s', fref, sample);
    t1 := clock_timestamp();
    scanned := LEAST(sample::bigint, tr);
    src_s := GREATEST(extract(epoch FROM t1 - t0) - lat, 0) / GREATEST(scanned, 1);
    src_s := GREATEST(src_s, 1e-9);    -- floor: the source never scans a row in < ~1ns.

    UPDATE gfs.cost SET net = net_s, source = src_s * pl, negligible = lat
      RETURNING * INTO r;
    RETURN r;
END;
$$;
COMMENT ON FUNCTION gfs.calibrate(int) IS
  'Probe the source (network throughput, scan rate, latency) and set the cost weights accordingly';

-- SOURCE DRIFT GUARD ---------------------------------------------------------
-- A copy-on-read clone fetches each table the FIRST time it is read, so tables
-- arrive at different moments. That is only safe while the source holds still.
-- If the source is written to during the clone's life, the clone mixes tables
-- captured at different points in time, producing a state that never existed at
-- the source (e.g. 1,000,000 orders but payments for 1,050,000).
--
-- Postgres cannot cheaply serve "this table AS OF clone time" for a long-lived
-- clone, so this cannot be PREVENTED on a live primary. It can be DETECTED.
-- Everything below is OBSERVATION ONLY: it never touches the read path, so it
-- cannot change the result of any query.
--
-- Detection uses three signals, with deliberately different trust levels:
--   * WAL position   -- NEVER misses a change (safe), but cannot attribute it.
--   * write counters -- attribute change to a table, but MISS truncate/rewrite.
--   * schema digests -- the only way to see DDL (it moves no row counter).
-- Hence the load-bearing rule: WAL moved but nothing attributed means
-- "changed, unattributed" and every materialized table is suspect. It must
-- never be read as "nothing changed".
CREATE TABLE gfs.source_baseline (
    lsn         text        NOT NULL,   -- source WAL position when captured
    captured_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
COMMENT ON TABLE gfs.source_baseline IS 'Where the source was when this clone was created (drift detection)';

CREATE TABLE gfs.source_table_baseline (
    relid    regclass PRIMARY KEY REFERENCES gfs.clone_source(relid) ON DELETE CASCADE,
    writes   bigint NOT NULL DEFAULT 0,  -- n_tup_ins+upd+del on the source
    live_tup bigint NOT NULL DEFAULT 0,  -- n_live_tup: catches TRUNCATE, which moves no counter
    src_fp   text,                       -- digest of the SOURCE table's columns
    src_cfp  text,                       -- digest of the SOURCE table's constraints (separate: see relation_cons_fp)
    ft_fp    text,                       -- digest of our FOREIGN TABLE declaration
    loc_fp   text                        -- digest of our LOCAL table
);
COMMENT ON TABLE gfs.source_table_baseline IS 'Per-table source counters + schema digests at clone time';

-- Map each clone table to its real name ON THE SOURCE, from the foreign table's
-- own options rather than the gfs_remote_<schema> naming convention.
CREATE VIEW gfs.source_map AS
SELECT cs.relid,
       to_regclass(cs.source_ref) AS ftrel,
       (SELECT option_value FROM pg_options_to_table(ft.ftoptions) WHERE option_name = 'schema_name') AS src_schema,
       (SELECT option_value FROM pg_options_to_table(ft.ftoptions) WHERE option_name = 'table_name')  AS src_table
  FROM gfs.clone_source cs
  JOIN pg_foreign_table ft ON ft.ftrelid = to_regclass(cs.source_ref);
COMMENT ON VIEW gfs.source_map IS 'clone table -> its schema/table name on the source';

-- Column digest of ANY local relation (local table or foreign table). The SAME
-- formula is evaluated on the source inside gfs.source_probe(), so the three
-- digests are directly comparable.
CREATE FUNCTION gfs.relation_fp(p_rel regclass) RETURNS text LANGUAGE sql STABLE AS $$
    SELECT md5(string_agg(a.attname || ':' || format_type(a.atttypid, a.atttypmod)
                          || ':' || a.attnotnull::text, ',' ORDER BY a.attnum))
      FROM pg_attribute a
     WHERE a.attrelid = p_rel AND a.attnum > 0 AND NOT a.attisdropped;
$$;

-- Constraints, hashed SEPARATELY from columns. They cannot live in relation_fp:
-- that digest is also compared against our imported FOREIGN table, and a foreign
-- table never carries constraints, so folding them in made every table look
-- permanently mismatched. FOREIGN KEYS are excluded because the clone bootstrap
-- drops them so lazy per-table fetching never trips referential integrity.
CREATE FUNCTION gfs.relation_cons_fp(p_rel regclass) RETURNS text LANGUAGE sql STABLE AS $$
    SELECT md5(COALESCE((SELECT string_agg(pg_get_constraintdef(c.oid), ',' ORDER BY pg_get_constraintdef(c.oid))
                           FROM pg_constraint c
                          WHERE c.conrelid = p_rel AND c.contype IN ('c','u','p','x')), ''));
$$;
COMMENT ON FUNCTION gfs.relation_fp(regclass) IS 'Column digest of a relation (comparable across clone and source)';

-- ONE round trip returning WAL position + per-table counters + schema digests.
-- It is a single statement, so all three are read from ONE source snapshot: a
-- verdict assembled from separate round trips would be internally inconsistent
-- (the source can move between them). dblink connects by FOREIGN SERVER NAME,
-- reusing gfs_remote_srv + the PUBLIC mapping, so no credentials are duplicated
-- and no superuser rights on the source are needed.
CREATE FUNCTION gfs.source_probe() RETURNS jsonb LANGUAGE plpgsql AS $$
DECLARE t text;
BEGIN
    SELECT probe INTO t FROM dblink('gfs_remote_srv', $q$
      SELECT json_build_object(
        'lsn', (CASE WHEN pg_is_in_recovery() THEN pg_last_wal_replay_lsn()
                     ELSE pg_current_wal_lsn() END)::text,
        'tables', COALESCE((SELECT json_agg(x) FROM (
              SELECT schemaname AS s, relname AS r,
                     (n_tup_ins + n_tup_upd + n_tup_del)::bigint AS w,
                     n_live_tup::bigint AS l
                FROM pg_stat_user_tables) x), '[]'::json),
        'schema', COALESCE((SELECT json_agg(y) FROM (
              SELECT n.nspname AS s, c.relname AS r,
                     md5(string_agg(a.attname || ':' || format_type(a.atttypid, a.atttypmod)
                                    || ':' || a.attnotnull::text, ',' ORDER BY a.attnum)) AS fp,
                     md5(COALESCE((SELECT string_agg(k.def, ',' ORDER BY k.def)
                                     FROM (SELECT pg_get_constraintdef(con.oid) AS def
                                             FROM pg_constraint con
                                            WHERE con.conrelid = c.oid
                                              AND con.contype IN ('c','u','p','x')) k), '')) AS cfp
                FROM pg_class c
                JOIN pg_namespace n ON n.oid = c.relnamespace
                JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
               WHERE c.relkind IN ('r','p')
                 AND n.nspname NOT IN ('pg_catalog','information_schema')
               GROUP BY 1,2, c.oid) y), '[]'::json)
      )::text
    $q$) AS d(probe text);
    RETURN t::jsonb;
END;
$$;
COMMENT ON FUNCTION gfs.source_probe() IS 'One round trip: source WAL position, per-table counters and schema digests, from a single snapshot';

-- Current source WAL position. The cheap signal, used by gfs.source_changed().
CREATE FUNCTION gfs.source_lsn() RETURNS text LANGUAGE plpgsql AS $$
DECLARE l text;
BEGIN
    SELECT lsn INTO l FROM dblink('gfs_remote_srv',
        'SELECT (CASE WHEN pg_is_in_recovery() THEN pg_last_wal_replay_lsn()
                      ELSE pg_current_wal_lsn() END)::text') AS t(lsn text);
    RETURN l;
END;
$$;
COMMENT ON FUNCTION gfs.source_lsn() IS 'Current WAL position of the source (cheap change signal)';

-- Record where the source is now. Called once at clone time by the bootstrap.
CREATE FUNCTION gfs.capture_source_baseline() RETURNS void LANGUAGE plpgsql AS $$
DECLARE p jsonb;
BEGIN
    p := gfs.source_probe();

    DELETE FROM gfs.source_baseline;
    INSERT INTO gfs.source_baseline(lsn) VALUES (p->>'lsn');

    DELETE FROM gfs.source_table_baseline;
    INSERT INTO gfs.source_table_baseline(relid, writes, live_tup, src_fp, src_cfp, ft_fp, loc_fp)
    SELECT m.relid,
           COALESCE(st.w, 0), COALESCE(st.l, 0), sc.fp, sc.cfp,
           gfs.relation_fp(m.ftrel), gfs.relation_fp(m.relid)
      FROM gfs.source_map m
      LEFT JOIN LATERAL (
            SELECT (e->>'w')::bigint AS w, (e->>'l')::bigint AS l
              FROM jsonb_array_elements(p->'tables') e
             WHERE e->>'s' = m.src_schema AND e->>'r' = m.src_table) st ON true
      LEFT JOIN LATERAL (
            SELECT e->>'fp' AS fp, e->>'cfp' AS cfp
              FROM jsonb_array_elements(p->'schema') e
             WHERE e->>'s' = m.src_schema AND e->>'r' = m.src_table) sc ON true;
END;
$$;
COMMENT ON FUNCTION gfs.capture_source_baseline() IS 'Record the source WAL position, per-table counters and schema digests at clone time';

-- Cheap yes/no: has the source moved at all? One tiny round trip. FALSE is a
-- hard guarantee (the WAL position is unchanged, so nothing was written); NULL
-- means we have no baseline to compare against.
CREATE FUNCTION gfs.source_changed() RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE base text;
BEGIN
    SELECT lsn INTO base FROM gfs.source_baseline LIMIT 1;
    IF base IS NULL THEN RETURN NULL; END IF;
    RETURN gfs.source_lsn() IS DISTINCT FROM base;
END;
$$;
COMMENT ON FUNCTION gfs.source_changed() IS 'TRUE if the source was written to since clone time (FALSE is a guarantee it was not)';

-- What changed on the source since clone time, and of what kind.
--   kind='data'            row counters or live-row count moved
--   kind='schema'          the source table's columns changed since clone time
--   kind='schema_mismatch' our foreign table no longer matches the source, so
--                          queries REFERENCING the differing columns will ERROR
--                          (measured: count(*) still works, SELECT * does not)
--   kind='unattributed' the WAL moved but no table accounts for it (TRUNCATE,
--                        VACUUM FULL, DDL elsewhere) -- every materialized
--                        table must be treated as suspect
-- Returns no rows when the source is provably untouched.
CREATE FUNCTION gfs.source_drift()
RETURNS TABLE(kind text, src_table text, detail text)
LANGUAGE plpgsql AS $$
DECLARE base text; p jsonb; cur text; attributed int := 0;
BEGIN
    SELECT lsn INTO base FROM gfs.source_baseline LIMIT 1;
    IF base IS NULL THEN
        RAISE WARNING 'gfs: no source baseline recorded; cannot tell whether the source changed';
        RETURN;
    END IF;

    p   := gfs.source_probe();
    cur := p->>'lsn';
    IF cur IS NOT DISTINCT FROM base THEN
        RETURN;   -- provably untouched: this clone is a consistent point in time
    END IF;

    IF to_regclass('pg_temp.gfs_drift_now') IS NULL THEN
        CREATE TEMP TABLE gfs_drift_now (kind text, src_table text, detail text) ON COMMIT DROP;
    END IF;
    DELETE FROM gfs_drift_now;

    INSERT INTO gfs_drift_now(kind, src_table, detail)
    SELECT d.kind, q.tbl, d.detail FROM (
        SELECT m.src_schema || '.' || m.src_table AS tbl,
               b.writes, b.live_tup, b.src_fp, b.src_cfp,
               COALESCE(st.w, 0) AS w_now, COALESCE(st.l, 0) AS l_now,
               sc.fp AS fp_now, sc.cfp AS cfp_now,
               gfs.relation_fp(m.ftrel) AS ft_fp_now
          FROM gfs.source_table_baseline b
          JOIN gfs.source_map m ON m.relid = b.relid
          LEFT JOIN LATERAL (
                SELECT (e->>'w')::bigint AS w, (e->>'l')::bigint AS l
                  FROM jsonb_array_elements(p->'tables') e
                 WHERE e->>'s' = m.src_schema AND e->>'r' = m.src_table) st ON true
          LEFT JOIN LATERAL (
                SELECT e->>'fp' AS fp, e->>'cfp' AS cfp
                  FROM jsonb_array_elements(p->'schema') e
                 WHERE e->>'s' = m.src_schema AND e->>'r' = m.src_table) sc ON true
    ) q
    CROSS JOIN LATERAL (VALUES
        ('data',
         CASE WHEN q.w_now <> q.writes OR q.l_now <> q.live_tup
              THEN format('writes %s -> %s, live rows %s -> %s',
                          q.writes, q.w_now, q.live_tup, q.l_now) END),
        ('schema',
         CASE WHEN q.fp_now IS DISTINCT FROM q.src_fp
              THEN 'the source table''s columns changed since clone time'
              WHEN q.cfp_now IS DISTINCT FROM q.src_cfp
              THEN 'the source table''s constraints changed since clone time' END),
        -- NOTE (measured): a column mismatch does NOT break federation wholesale.
        -- postgres_fdw only sends the columns a query actually references, so
        -- count(*) still succeeds while SELECT * or any reference to a changed
        -- column fails with 'column ... does not exist'. Degradation is partial.
        ('schema_mismatch',
         CASE WHEN q.fp_now IS DISTINCT FROM q.ft_fp_now
              THEN 'our foreign table no longer matches the source: queries referencing the differing columns will ERROR (queries touching none of them still work)' END)
    ) AS d(kind, detail)
    WHERE d.detail IS NOT NULL;

    -- Only BASELINE-RELATIVE findings explain this WAL movement. 'schema_mismatch'
    -- is a STANDING hazard (source vs our foreign table), which persists across
    -- checks and would otherwise mask a genuinely unattributed change.
    -- Tables created on the source after clone time are not registered here, so
    -- nothing else in this function looks at them: the clone simply does not know
    -- they exist and a query gets a bare "relation does not exist". Worse, creating
    -- one moves the WAL without touching any tracked counter, so it used to trip
    -- the unattributed blanket below and mark every COPIED table suspect -- a false
    -- positive about tables that had not changed at all.
    --
    -- Scoped to schemas the clone already mirrors: a clone made with ?schema=a,b
    -- deliberately excludes the rest, and those are not "new".
    --
    -- "New" means NOT PRESENT LOCALLY, which is not the same as "absent from
    -- gfs.source_map". A PARTITIONED PARENT (relkind='p') is replayed onto the
    -- clone by the faithful dump but is deliberately never registered for
    -- copy-on-read: it stores no rows of its own, and a query on it prunes to the
    -- leaf partitions, which ARE registered. gfs.source_map is built from
    -- clone_source JOIN pg_foreign_table, so a parent can never appear in it, and
    -- matching on source_map alone reported every partitioned parent as a brand
    -- new table on every check, forever, against a completely unchanged source.
    --
    -- That was not merely noise: 'new_table' counts as attribution just below, so
    -- a permanent phantom finding kept `attributed` non-zero and the unattributed
    -- blanket could never fire. On a partitioned clone that MASKED real drift that
    -- nothing else could account for.
    --
    -- Testing local presence instead is both narrower and more honest, and it
    -- still reports what it should: a genuinely new table (and a genuinely new
    -- leaf partition, which the dump never created here) has no local relation.
    INSERT INTO gfs_drift_now(kind, src_table, detail)
    SELECT 'new_table', e->>'s' || '.' || (e->>'r'),
           'created on the source after clone time; this clone does not have it (re-clone to include it)'
      FROM jsonb_array_elements(p->'schema') e
     WHERE (e->>'s') IN (SELECT DISTINCT src_schema FROM gfs.source_map)
       AND NOT EXISTS (SELECT 1 FROM gfs.source_map m
                        WHERE m.src_schema = e->>'s' AND m.src_table = e->>'r')
       AND to_regclass(quote_ident(e->>'s') || '.' || quote_ident(e->>'r')) IS NULL;

    -- 'new_table' counts as attribution: it explains the WAL movement, so the
    -- blanket below must not also fire and call unrelated tables suspect.
    SELECT count(*) INTO attributed FROM gfs_drift_now g
     WHERE g.kind IN ('data','schema','new_table');

    -- Attribution is never PROOF that the WAL movement is fully explained: we
    -- cannot map WAL bytes to tables, and several real changes move no counter at
    -- all (a sequence advancing, an enum gaining a label, a table rewrite). Making
    -- this conditional on `attributed = 0` meant such a change was hidden whenever
    -- ANY other table happened to move -- i.e. it stopped working precisely on a
    -- busy source. Report it on its own terms instead.
    IF attributed = 0 THEN
        -- nothing at all accounts for the movement: every copied table is suspect
        INSERT INTO gfs_drift_now(kind, src_table, detail)
        VALUES ('unattributed', '(all materialized tables)',
                format('source moved (WAL %s -> %s) but no table accounts for it; treat every copied table as suspect', base, cur));
    ELSE
        -- something is explained, but the rest may not be. Lower severity: it does
        -- not mark tables suspect, it tells the user what to run to be sure.
        INSERT INTO gfs_drift_now(kind, src_table, detail)
        VALUES ('unaccounted', '(objects without row counters)',
                format('source moved (WAL %s -> %s); %s table(s) explain part of it. Sequences, enum labels and rewrites move no counter -- run `gfs pull` to re-sync them', base, cur, attributed));
    END IF;

    RAISE WARNING 'gfs: the source changed since this clone was created (WAL % -> %). This clone may mix data from different points in time.', base, cur;
    RETURN QUERY SELECT g.kind, g.src_table, g.detail FROM gfs_drift_now g ORDER BY g.kind, g.src_table;
END;
$$;
COMMENT ON FUNCTION gfs.source_drift() IS
  'What changed on the source since clone time (empty = untouched); kinds: data, schema, unfederatable, unattributed';

-- SYNC POLICY + DRIFT STATE ---------------------------------------------------
-- Policy knobs for staying in step with the source. Single row, same pattern as
-- gfs.cost / gfs.budget: a hot switch, no redeploy.
--
-- Set directly today, e.g. UPDATE gfs.sync_policy SET autopull = true. There is
-- deliberately no CLI wrapper yet: a `gfs config clone.autopull` would have to
-- write BOTH .gfs/config.toml (durable intent, survives container recreation)
-- and this row (the live setting the worker actually reads, since the worker
-- runs inside the database and cannot read the repo's config file). Tracked in
-- https://github.com/Guepard-Corp/gfs/issues/133.
CREATE TABLE gfs.sync_policy (
    autopull           boolean  NOT NULL DEFAULT false,      -- follow the source automatically (DATA drift)
    autoschema         boolean  NOT NULL DEFAULT false,      -- repair the table's SHAPE automatically (SCHEMA drift)
    autopull_interval  interval NOT NULL DEFAULT '5 min',    -- how often the worker pulls
    autopull_max_bytes bigint   NOT NULL DEFAULT 500000000,  -- never auto-copy a table larger than this
    check_interval     interval NOT NULL DEFAULT '1 min'     -- how stale the drift verdict may be
);
INSERT INTO gfs.sync_policy DEFAULT VALUES;
COMMENT ON TABLE gfs.sync_policy IS 'Source-sync policy (autopull off by default: a clone is a branch, not a mirror)';

-- Per-table verdict the PLANNER HOOK reads on every scan. It must be LOCAL and
-- cheap: the hook never does network I/O. gfs.refresh_drift_state() fills it.
CREATE TABLE gfs.drift_state (
    relid      regclass PRIMARY KEY REFERENCES gfs.clone_source(relid) ON DELETE CASCADE,
    drifted    boolean     NOT NULL DEFAULT false,   -- the source's ROWS changed
    schema_drifted boolean NOT NULL DEFAULT false,   -- the source's SHAPE changed (columns added/dropped/retyped)
    reason     text,
    checked_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
COMMENT ON TABLE gfs.drift_state IS 'Per-table "is the local copy stale?" verdict, read by the planner hook';

-- Findings that belong to NO registered table: a table created on the source, or
-- movement nothing accounts for. gfs.drift_state is keyed by relid and joined to
-- source_map, so anything not registered has nowhere to live there and would stay
-- invisible to `gfs fetch` no matter how well it was detected.
CREATE TABLE gfs.drift_notes (
    kind     text NOT NULL,
    subject  text NOT NULL,
    detail   text,
    noted_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
COMMENT ON TABLE gfs.drift_notes IS 'Drift findings not attached to a registered table (new source tables, unaccounted movement)';

-- Recompute the verdict from the source. One round trip (gfs.source_drift).
-- 'unattributed' means the source moved but no table accounts for it, so EVERY
-- materialized table is marked suspect: failing safe is the entire point.
CREATE FUNCTION gfs.refresh_drift_state() RETURNS int LANGUAGE plpgsql AS $$
DECLARE n int; blanket boolean; r record;
BEGIN
    -- Only create it when it is genuinely absent: CREATE ... IF NOT EXISTS still
    -- raises a NOTICE when the relation is already there, and gfs.pull() calls
    -- this twice in one transaction, so a successful command printed a warning
    -- that looked like something had gone wrong.
    IF to_regclass('pg_temp.gfs_drift_scan') IS NULL THEN
        CREATE TEMP TABLE gfs_drift_scan(kind text, src_table text, detail text) ON COMMIT DROP;
    END IF;
    DELETE FROM gfs_drift_scan;
    INSERT INTO gfs_drift_scan SELECT * FROM gfs.source_drift();

    SELECT EXISTS(SELECT 1 FROM gfs_drift_scan WHERE kind = 'unattributed') INTO blanket;

    INSERT INTO gfs.drift_state(relid, drifted, schema_drifted, reason, checked_at)
    SELECT m.relid,
           COALESCE(d.kind IS NOT NULL, false) OR blanket,
           EXISTS (SELECT 1 FROM gfs_drift_scan s2
                    WHERE s2.src_table = m.src_schema || '.' || m.src_table
                      AND s2.kind IN ('schema','schema_mismatch')),
           COALESCE(d.kind || ': ' || d.detail,
                    CASE WHEN blanket THEN 'source moved but unattributed; treated as suspect' END),
           clock_timestamp()
      FROM gfs.source_map m
      LEFT JOIN LATERAL (
            SELECT s.kind, s.detail FROM gfs_drift_scan s
             WHERE s.src_table = m.src_schema || '.' || m.src_table
               AND s.kind IN ('data','schema')
             LIMIT 1) d ON true
    ON CONFLICT (relid) DO UPDATE
        SET drifted = EXCLUDED.drifted, schema_drifted = EXCLUDED.schema_drifted,
            reason = EXCLUDED.reason, checked_at = EXCLUDED.checked_at;

    -- Repair SHAPE drift here when the user opted in. This is the only place that
    -- can: the planner hook detects the drift but must raise an error (the query is
    -- unanswerable), and an error rolls back anything it tried to enqueue. This
    -- function runs from the background drift check, whose transaction commits.
    IF COALESCE((SELECT autoschema FROM gfs.sync_policy LIMIT 1), false) THEN
        FOR r IN SELECT relid FROM gfs.drift_state WHERE schema_drifted LOOP
            -- best-effort: a destructive change returns 'conflict: ...' and is left
            -- for the user, exactly as gfs.pull() reports it
            PERFORM gfs.repair_schema(r.relid);
        END LOOP;

        -- Adopt new partitions / inheritance children here too, not only on an
        -- explicit pull. Every other kind of drift makes the clone LOUD: a column
        -- change raises, a data change federates. A missing partition is the one
        -- case that stays quiet, because the parent still answers and simply
        -- returns fewer rows than the source has. Leaving that until someone
        -- happens to run a pull means a clone that is silently short in the
        -- meantime, which is the failure mode this whole guard exists to prevent.
        --
        -- Adding a partition is an ADDITIVE source schema change, which is exactly
        -- what autoschema opts into, and adoption never writes to the source and
        -- never touches rows the user already has.
        --
        -- Gated on a new_table finding so the ordinary quiet case does not pay for
        -- an extra source round trip on every background check.
        IF EXISTS (SELECT 1 FROM gfs_drift_scan WHERE kind = 'new_table') THEN
            PERFORM gfs.adopt_source_tables();
        END IF;
    END IF;

    -- keep the non-table findings too, so `gfs fetch` can show them locally
    DELETE FROM gfs.drift_notes;
    INSERT INTO gfs.drift_notes(kind, subject, detail)
    SELECT g.kind, g.src_table, g.detail FROM gfs_drift_scan g
     WHERE g.kind IN ('new_table','unattributed','unaccounted');

    SELECT count(*) INTO n FROM gfs.drift_state WHERE drifted;
    RETURN n;
END;
$$;
COMMENT ON FUNCTION gfs.refresh_drift_state() IS 'Recompute the per-table stale verdict the hook reads; returns how many tables are stale';

-- Adopt tables that appeared on the source UNDERNEATH A PARENT THIS CLONE ALREADY
-- HAS: new partitions of a partitioned table, and new INHERITS children.
--
-- These go wrong in a way an ordinary new table does not. A brand new table at
-- least errors with "relation does not exist" when somebody reads it, so the gap
-- announces itself. A new partition or child is reached THROUGH a parent the
-- clone already serves, so the query SUCCEEDS and quietly returns fewer rows: the
-- clone answers 3 where the source has 5, with nothing to say the answer is short.
--
-- Only children of an ALREADY CLONED parent are adopted. A standalone new table
-- still reports "re-clone to include it": reproducing arbitrary DDL (indexes,
-- defaults, triggers, grants) is the clone bootstrap's job, whereas a partition
-- takes its whole shape from its parent and needs only the partition bound.
--
-- The adopted table is created EMPTY and registered for copy-on-read, so the very
-- next read hydrates it from the source through the ordinary lazy path. Nothing
-- is ever written to the source.
CREATE FUNCTION gfs.adopt_source_tables()
RETURNS TABLE(tbl text, detail text) LANGUAGE plpgsql AS $$
DECLARE
    r        record;
    col      record;
    p        jsonb;
    local_fq text;
    ft_fq    text;
    ftnsp    text;
    newrel   regclass;
BEGIN
    p := gfs.source_probe();

    FOR r IN
        SELECT * FROM dblink('gfs_remote_srv', $q$
            SELECT n.nspname::text, c.relname::text, c.relispartition,
                   pn.nspname::text, pc.relname::text,
                   CASE WHEN c.relispartition
                        THEN pg_get_expr(c.relpartbound, c.oid) END,
                   (SELECT a.attname::text
                      FROM pg_index i2
                      JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = i2.indkey[0]
                     WHERE i2.indrelid = c.oid AND i2.indisunique AND i2.indimmediate
                       AND i2.indpred IS NULL AND 0 <> ALL (i2.indkey::int[])
                     ORDER BY i2.indisprimary DESC, i2.indnkeyatts ASC, i2.indexrelid
                     LIMIT 1)
              FROM pg_class c
              JOIN pg_namespace n  ON n.oid = c.relnamespace
              JOIN pg_inherits ih  ON ih.inhrelid = c.oid
              JOIN pg_class pc     ON pc.oid = ih.inhparent
              JOIN pg_namespace pn ON pn.oid = pc.relnamespace
             WHERE c.relkind = 'r'
               AND n.nspname NOT IN ('pg_catalog','information_schema')
        $q$) AS d(nsp text, tab text, is_part boolean,
                  pnsp text, ptab text, bound text, keycol text)
    LOOP
        local_fq := format('%I.%I', r.nsp, r.tab);
        CONTINUE WHEN to_regclass(local_fq) IS NOT NULL;              -- already here
        CONTINUE WHEN to_regclass(format('%I.%I', r.pnsp, r.ptab)) IS NULL;  -- parent not cloned
        -- a clone made with ?schema=a,b deliberately excludes the rest
        CONTINUE WHEN r.nsp NOT IN (SELECT DISTINCT src_schema FROM gfs.source_map);

        IF r.keycol IS NULL THEN
            tbl := r.nsp || '.' || r.tab;
            detail := 'not adopted: no usable unique key on the source, so it cannot be registered for copy-on-read';
            RETURN NEXT; CONTINUE;
        END IF;

        ftnsp := 'gfs_remote_' || r.nsp;
        ft_fq := format('%I.%I', ftnsp, r.tab);
        BEGIN
            EXECUTE format('IMPORT FOREIGN SCHEMA %I LIMIT TO (%I) FROM SERVER gfs_remote_srv INTO %I',
                           r.nsp, r.tab, ftnsp);
        EXCEPTION WHEN others THEN
            tbl := r.nsp || '.' || r.tab;
            detail := format('not adopted: could not import the foreign table (%s)', SQLERRM);
            RETURN NEXT; CONTINUE;
        END;

        BEGIN
            IF r.is_part THEN
                -- the shape comes from the parent; only the bound is ours to supply
                EXECUTE format('CREATE TABLE %s PARTITION OF %I.%I %s',
                               local_fq, r.pnsp, r.ptab, r.bound);
            ELSE
                EXECUTE format('CREATE TABLE %s () INHERITS (%I.%I)', local_fq, r.pnsp, r.ptab);
                -- columns the child adds on top of what it inherits
                FOR col IN
                    SELECT a.attname, format_type(a.atttypid, a.atttypmod) AS typ
                      FROM pg_attribute a
                     WHERE a.attrelid = ft_fq::regclass AND a.attnum > 0 AND NOT a.attisdropped
                       AND NOT EXISTS (SELECT 1 FROM pg_attribute la
                                        WHERE la.attrelid = local_fq::regclass
                                          AND la.attnum > 0 AND NOT la.attisdropped
                                          AND la.attname = a.attname)
                LOOP
                    EXECUTE format('ALTER TABLE %s ADD COLUMN %I %s', local_fq, col.attname, col.typ);
                END LOOP;
                -- copy-on-read needs a unique key of its own, and INHERITS does NOT
                -- carry the parent's primary key down to the child
                EXECUTE format('ALTER TABLE %s ADD PRIMARY KEY (%I)', local_fq, r.keycol);
            END IF;
        EXCEPTION WHEN others THEN
            EXECUTE format('DROP FOREIGN TABLE IF EXISTS %s', ft_fq);
            tbl := r.nsp || '.' || r.tab;
            detail := format('not adopted: could not create it locally (%s)', SQLERRM);
            RETURN NEXT; CONTINUE;
        END;

        newrel := local_fq::regclass;
        PERFORM gfs.register_clone(newrel, ft_fq, r.keycol);

        -- Anchor a baseline NOW. gfs.source_drift() compares per table by joining
        -- source_table_baseline, so a registered table with no baseline row would
        -- never be drift-checked again: adopted once, then never noticed changing.
        INSERT INTO gfs.source_table_baseline(relid, writes, live_tup, src_fp, src_cfp, ft_fp, loc_fp)
        SELECT newrel, COALESCE(st.w, 0), COALESCE(st.l, 0), sc.fp, sc.cfp,
               gfs.relation_fp(ft_fq::regclass), gfs.relation_fp(newrel)
          FROM (SELECT 1) z
          LEFT JOIN LATERAL (
                SELECT (e->>'w')::bigint AS w, (e->>'l')::bigint AS l
                  FROM jsonb_array_elements(p->'tables') e
                 WHERE e->>'s' = r.nsp AND e->>'r' = r.tab) st ON true
          LEFT JOIN LATERAL (
                SELECT e->>'fp' AS fp, e->>'cfp' AS cfp
                  FROM jsonb_array_elements(p->'schema') e
                 WHERE e->>'s' = r.nsp AND e->>'r' = r.tab) sc ON true;

        tbl := r.nsp || '.' || r.tab;
        detail := format('adopted as a %s of %I.%I; next read fetches its rows',
                         CASE WHEN r.is_part THEN 'partition' ELSE 'child' END, r.pnsp, r.ptab);
        RETURN NEXT;
    END LOOP;
END;
$$;
COMMENT ON FUNCTION gfs.adopt_source_tables() IS 'Adopt new partitions / INHERITS children of an already cloned parent, registered lazily';

-- A matview on the clone is a LOCAL object computed from the clone's own tables,
-- and those tables are copy-on-read. So a REFRESH here reads current source data
-- through the ordinary lazy path: no separate fetch of the matview is needed, and
-- nothing has to be copied from the source's stored matview contents.
--
-- Refreshed in dependency order (a matview built on another matview goes last),
-- otherwise a nested matview would be recomputed from its parent's stale contents
-- and stay one pull behind.
CREATE FUNCTION gfs.refresh_clone_matviews()
RETURNS TABLE(mv text, detail text) LANGUAGE plpgsql AS $$
DECLARE r record;
BEGIN
    FOR r IN
        WITH RECURSIVE m AS (
            SELECT c.oid FROM pg_class c
              JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind = 'm' AND n.nspname NOT IN ('pg_catalog','information_schema')
        ),
        edge AS (   -- a matview -> the matview it reads
            SELECT DISTINCT rw.ev_class AS child, d.refobjid AS parent
              FROM pg_rewrite rw
              JOIN pg_depend d ON d.objid = rw.oid AND d.classid = 'pg_rewrite'::regclass
             WHERE d.refobjid <> rw.ev_class
               AND d.refobjid IN (SELECT oid FROM m)
               AND rw.ev_class IN (SELECT oid FROM m)
        ),
        lvl AS (
            SELECT oid, 0 AS depth FROM m WHERE oid NOT IN (SELECT child FROM edge)
            UNION ALL
            SELECT e.child, l.depth + 1 FROM edge e JOIN lvl l ON l.oid = e.parent
        )
        SELECT oid::regclass::text AS fq, max(depth) AS depth
          FROM lvl GROUP BY 1 ORDER BY 2, 1
    LOOP
        BEGIN
            EXECUTE format('REFRESH MATERIALIZED VIEW %s', r.fq);
            mv := r.fq; detail := 'recomputed from this clone''s tables';
        EXCEPTION WHEN others THEN
            -- e.g. a base table still blocked by an unresolved schema conflict
            mv := r.fq; detail := format('could not refresh (%s)', SQLERRM);
        END;
        RETURN NEXT;
    END LOOP;
END;
$$;
COMMENT ON FUNCTION gfs.refresh_clone_matviews() IS 'Recompute local matviews from the clone tables, in dependency order';

-- gfs pull: put stale tables BACK ON THE LAZY PATH.
-- It copies nothing itself. It clears the cached state so the table looks
-- "never fetched", and the existing router then makes its normal cost decision
-- on the next read (small table -> copy, huge table -> federate), exactly as it
-- does for a freshly cloned table.
--
-- Runs in ONE transaction, so every table is reset against a single source
-- snapshot: pulling tables one at a time would leave the clone fresh but still
-- torn across different moments.
--
-- A table with local writes is NEVER reset (that would destroy the user's work);
-- it is reported as a conflict for a human to resolve, like git refusing to
-- clobber local changes.
CREATE FUNCTION gfs.pull(force boolean DEFAULT false)
RETURNS TABLE(action text, tbl text, detail text) LANGUAGE plpgsql AS $$
DECLARE r record; seen text; skipped int := 0; res text;
BEGIN
    -- Where the source is as we start. Everything below reconciles against this
    -- point, so it becomes the new "accounted for up to here" marker at the end.
    seen := gfs.source_lsn();
    PERFORM gfs.refresh_drift_state();

    -- Adopt new partitions / inheritance children FIRST. They are reached through
    -- a parent this clone already serves, so until they exist locally the parent
    -- silently answers with fewer rows than the source has. Doing it before the
    -- reset loop also means an adopted table is in place for everything below.
    FOR r IN SELECT * FROM gfs.adopt_source_tables() LOOP
        action := 'adopt'; tbl := r.tbl; detail := r.detail;
        RETURN NEXT;
    END LOOP;

    FOR r IN
        SELECT d.relid, d.reason, m.src_schema || '.' || m.src_table AS name,
               gfs.relation_diverged_sql(d.relid) AS diverged,
               d.schema_drifted
          FROM gfs.drift_state d
          JOIN gfs.source_map m ON m.relid = d.relid
         WHERE d.drifted OR d.schema_drifted
         ORDER BY 3
    LOOP
        -- SHAPE first: refetching rows cannot help while our definition of the
        -- table is stale, and hydration would ask the source for columns it no
        -- longer has. repair_schema resyncs the table itself on success.
        IF r.schema_drifted THEN
            res := gfs.repair_schema(r.relid);
            IF res LIKE 'conflict:%' THEN
                -- Unresolved, so it must keep reporting. Counting it as skipped
                -- stops the global WAL marker advancing past this change, which
                -- would make the next check short-circuit and silently clear the
                -- conflict (the failure mode fixed for data conflicts).
                skipped := skipped + 1;
                action := 'conflict'; tbl := r.name; detail := res;
                RETURN NEXT; CONTINUE;
            END IF;
            action := 'schema'; tbl := r.name; detail := res;
            RETURN NEXT; CONTINUE;
        END IF;

        IF r.diverged AND NOT force THEN
            skipped := skipped + 1;
            action := 'conflict'; tbl := r.name;
            detail := 'you have local writes AND the source changed; not touched (use force to discard yours)';
            RETURN NEXT; CONTINUE;
        END IF;

        -- Delegate to resync_table: it resets the table AND re-anchors THIS
        -- table's baseline. Doing the reset inline here (as an earlier version
        -- did) left the baseline untouched, so the table stayed flagged as
        -- drifted forever and every later read went to the source.
        PERFORM gfs.resync_table(r.relid, force);

        action := 'reset'; tbl := r.name;
        detail := 'back on the lazy path; next read fetches from the source';
        RETURN NEXT;
    END LOOP;

    -- Enum labels first: a table whose type cannot represent a fetched value is
    -- unreadable outright, so this has to be repaired before anything refetches.
    FOR r IN SELECT * FROM gfs.resync_enums() LOOP
        action := 'enum'; tbl := r.typ;
        detail := format('added label %s from the source', r.added);
        RETURN NEXT;
    END LOOP;

    -- Sequences drift silently: nothing reads them, so no row counter moves, yet a
    -- clone whose counter is behind its own fetched rows fails the next insert with
    -- a duplicate key. Re-sync as part of pull, and report it like any other repair.
    FOR r IN SELECT * FROM gfs.resync_sequences() LOOP
        action := 'sequence'; tbl := r.seq;
        detail := format('advanced %s -> %s to match the source', r.was, r.now_at);
        RETURN NEXT;
    END LOOP;

    -- Matviews LAST: they are recomputed from the clone's tables, so every table
    -- reset, adoption and enum repair above has to have happened first or the
    -- matview would be rebuilt from data we are about to replace.
    FOR r IN SELECT * FROM gfs.refresh_clone_matviews() LOOP
        action := 'matview'; tbl := r.mv; detail := r.detail;
        RETURN NEXT;
    END LOOP;

    -- Advance the GLOBAL WAL marker ONLY when nothing was skipped.
    --
    -- Why advance at all: otherwise the marker stays at clone time forever, so
    -- every later check sees "the source moved", finds nothing to attribute
    -- (the per-table baselines were just re-anchored) and blankets every table
    -- as suspect, meaning a pull could never reach a clean state.
    --
    -- Why only when nothing was skipped: gfs.source_drift() short-circuits when
    -- the marker equals the source's current position, so advancing it while a
    -- conflict is still outstanding would stop the per-table comparison running
    -- at all and the conflict would silently disappear -- the same class of bug
    -- as re-anchoring a skipped table's baseline.
    --
    -- Leaving the marker in place when a conflict remains is safe: that conflict
    -- still attributes the source's movement, so the "unattributed" blanket does
    -- not fire on the tables we just reset.
    IF skipped = 0 THEN
        UPDATE gfs.source_baseline SET lsn = seen;
    END IF;
    PERFORM gfs.refresh_drift_state();
END;
$$;

-- Reset ONE table to "never fetched" and re-anchor ONLY its baseline, so the
-- lazy path owns it again from the next read. This is what the background
-- worker runs for an autopull; gfs.pull() is the same thing done by hand for
-- every stale table at once.
--
-- It must run OUTSIDE the query that noticed the drift: TRUNCATE needs an
-- ACCESS EXCLUSIVE lock, which cannot be taken on a table the current query is
-- already reading.
CREATE FUNCTION gfs.resync_table(p_relid regclass, p_force boolean DEFAULT false)
RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE p jsonb; m record;
BEGIN
    IF gfs.relation_diverged_sql(p_relid) AND NOT p_force THEN
        RETURN false;   -- local writes: a conflict, never resolved automatically
    END IF;
    IF p_force THEN
        -- the caller is explicitly discarding local work, so drop the markers too;
        -- otherwise the table would stay flagged as diverged after being reset
        UPDATE gfs.clone_source SET has_local_writes = false WHERE relid = p_relid;
        DELETE FROM gfs.tombstone WHERE relid = p_relid;
    END IF;

    EXECUTE format('TRUNCATE ONLY %s', p_relid::regclass);  -- ONLY: keep inheritance children
    DELETE FROM gfs.cached           WHERE relid = p_relid;
    DELETE FROM gfs.cached_predicate WHERE relid = p_relid;
    UPDATE gfs.clone_source
       SET whole_cached = false, partial_rows = 0, no_partial = false, access_count = 0
     WHERE relid = p_relid;

    -- re-anchor THIS table only
    SELECT * INTO m FROM gfs.source_map WHERE relid = p_relid;
    p := gfs.source_probe();
    UPDATE gfs.source_table_baseline b
       SET writes   = COALESCE((SELECT (e->>'w')::bigint FROM jsonb_array_elements(p->'tables') e
                                 WHERE e->>'s' = m.src_schema AND e->>'r' = m.src_table), 0),
           live_tup = COALESCE((SELECT (e->>'l')::bigint FROM jsonb_array_elements(p->'tables') e
                                 WHERE e->>'s' = m.src_schema AND e->>'r' = m.src_table), 0),
           src_fp   = (SELECT e->>'fp' FROM jsonb_array_elements(p->'schema') e
                        WHERE e->>'s' = m.src_schema AND e->>'r' = m.src_table),
           ft_fp    = gfs.relation_fp(m.ftrel),
           loc_fp   = gfs.relation_fp(p_relid)
     WHERE b.relid = p_relid;

    INSERT INTO gfs.drift_state(relid, drifted, reason, checked_at)
    VALUES (p_relid, false, NULL, clock_timestamp())
    ON CONFLICT (relid) DO UPDATE
        SET drifted = false, reason = NULL, checked_at = clock_timestamp();
    RETURN true;
END;
$$;
COMMENT ON FUNCTION gfs.resync_table(regclass, boolean) IS 'Reset one table to never-fetched and re-anchor its baseline (the autopull unit of work)';
COMMENT ON FUNCTION gfs.pull(boolean) IS 'Reset stale tables to "never fetched" so the lazy path refetches them; skips tables with local writes';

-- Does this table have local writes or tombstones? (mirrors relation_diverged in
-- catalog.rs, which the hook uses; kept in SQL so gfs.pull can consult it too.)
CREATE FUNCTION gfs.relation_diverged_sql(p_relid regclass) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT COALESCE((SELECT has_local_writes FROM gfs.clone_source WHERE relid = p_relid), false)
        OR EXISTS (SELECT 1 FROM gfs.tombstone WHERE relid = p_relid);
$$;

-- ENUM RE-SYNC ----------------------------------------------------------------
-- Enum types are mirrored once, at clone time. When the source gains a label the
-- clone's copy of the type cannot represent it, and fetching ANY row of a table
-- using that type fails -- not just the new row:
--
--   ERROR:  invalid input value for enum mood: "excited"
--
-- Nothing detects it either: adding an enum label is DDL on a TYPE, so it moves
-- no row counter and does not change any table's column list.
--
-- Add the labels the source has and we do not, preserving the source's ordering
-- by inserting each one AFTER the label that precedes it there. Only ever adds:
-- a label the clone has and the source does not is left alone, since removing it
-- could invalidate rows already stored locally.
--
-- Note ALTER TYPE ... ADD VALUE may run inside a transaction (PG12+), but the new
-- label cannot be USED until that transaction commits. This is called from
-- gfs.pull(), which only resets catalog state and never fetches rows, so the
-- label is committed and usable well before the next read needs it.
CREATE FUNCTION gfs.resync_enums()
RETURNS TABLE(typ text, added text) LANGUAGE plpgsql AS $$
DECLARE r record; lbl record; prev text; have boolean;
BEGIN
    FOR r IN
        SELECT n.nspname AS nsp, t.typname AS tname, format('%I.%I', n.nspname, t.typname) AS fq
          FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
         WHERE t.typtype = 'e'
           AND n.nspname NOT IN ('pg_catalog','information_schema','gfs','gfs_sync')
         ORDER BY 1, 2
    LOOP
        prev := NULL;
        FOR lbl IN
            SELECT * FROM dblink('gfs_remote_srv', format($q$
                SELECT e.enumlabel::text
                  FROM pg_enum e
                  JOIN pg_type t ON t.oid = e.enumtypid
                  JOIN pg_namespace n ON n.oid = t.typnamespace
                 WHERE n.nspname = %L AND t.typname = %L
                 ORDER BY e.enumsortorder
            $q$, r.nsp, r.tname)) AS s(label text)
        LOOP
            SELECT EXISTS (SELECT 1 FROM pg_enum e
                             JOIN pg_type t2 ON t2.oid = e.enumtypid
                             JOIN pg_namespace n2 ON n2.oid = t2.typnamespace
                            WHERE n2.nspname = r.nsp AND t2.typname = r.tname
                              AND e.enumlabel = lbl.label) INTO have;
            IF NOT have THEN
                BEGIN
                    IF prev IS NULL THEN
                        EXECUTE format('ALTER TYPE %s ADD VALUE %L BEFORE %L',
                                       r.fq, lbl.label,
                                       (SELECT e.enumlabel FROM pg_enum e
                                          JOIN pg_type t3 ON t3.oid = e.enumtypid
                                          JOIN pg_namespace n3 ON n3.oid = t3.typnamespace
                                         WHERE n3.nspname = r.nsp AND t3.typname = r.tname
                                         ORDER BY e.enumsortorder LIMIT 1));
                    ELSE
                        EXECUTE format('ALTER TYPE %s ADD VALUE %L AFTER %L', r.fq, lbl.label, prev);
                    END IF;
                    typ := r.fq; added := lbl.label; RETURN NEXT;
                EXCEPTION WHEN others THEN
                    typ := r.fq; added := format('FAILED %s (%s)', lbl.label, SQLERRM);
                    RETURN NEXT;
                END;
            END IF;
            prev := lbl.label;
        END LOOP;
    END LOOP;
END;
$$;
COMMENT ON FUNCTION gfs.resync_enums() IS
  'Add enum labels the source has gained since clone time (never removes), so fetched rows using them are representable';

-- SEQUENCE RE-SYNC ------------------------------------------------------------
-- Sequence positions are replicated ONCE, at clone time. The source keeps
-- consuming ids, so the clone's counter falls behind, and once the table's rows
-- are (re)fetched the clone holds rows whose ids are ABOVE its own sequence. The
-- next local insert then collides:
--
--   ERROR:  duplicate key value violates unique constraint "items_pkey"
--   DETAIL:  Key (id)=(102) already exists.
--
-- which reads like an application bug rather than clone drift.
--
-- Advance each local sequence to at least the source's position. Deliberately
-- monotonic: a clone that has consumed ids of its own must never be wound BACK,
-- or it would hand out ids it has already used. Reads the source through the
-- existing gfs_remote_srv server, so no credentials are duplicated (unlike the
-- bootstrap's replicate_sequences, which needs a connection string).
--
-- Returns one row per sequence it moved.
CREATE FUNCTION gfs.resync_sequences()
RETURNS TABLE(seq text, was bigint, now_at bigint) LANGUAGE plpgsql AS $$
DECLARE r record; src record; local_last bigint; local_called boolean;
BEGIN
    FOR r IN
        SELECT n.nspname AS nsp, c.relname AS rel, format('%I.%I', n.nspname, c.relname) AS fq
          FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE c.relkind = 'S' AND n.nspname NOT IN ('pg_catalog','information_schema','gfs','gfs_sync')
         ORDER BY 1, 2
    LOOP
        BEGIN
            SELECT * INTO src FROM dblink('gfs_remote_srv',
                format('SELECT last_value, is_called FROM %s', r.fq))
              AS t(last_value bigint, is_called boolean);
        EXCEPTION WHEN others THEN
            CONTINUE;   -- sequence absent on the source (local-only): leave it alone
        END;
        CONTINUE WHEN src.last_value IS NULL;

        EXECUTE format('SELECT last_value, is_called FROM %s', r.fq) INTO local_last, local_called;
        -- only ever move FORWARD
        CONTINUE WHEN local_last >= src.last_value;

        PERFORM setval(r.fq::regclass, src.last_value, src.is_called);
        seq := r.fq; was := local_last; now_at := src.last_value;
        RETURN NEXT;
    END LOOP;
END;
$$;
COMMENT ON FUNCTION gfs.resync_sequences() IS
  'Advance local sequences to the source''s positions (never backwards), so local inserts do not collide with fetched rows';

-- SIZE RE-VERIFICATION --------------------------------------------------------
-- source_rows is captured ONCE at clone time and drives the copy-vs-ask decision.
-- A live source keeps growing, so a table that was small when you cloned can be
-- millions of rows later while the router still believes the old number and
-- happily copies the whole thing over the link. Same failure class as #112/#114,
-- reached through drift instead of a bad initial measurement.
--
-- Measure the real size with count(*), which postgres_fdw pushes down to the
-- source (one round trip, no data transfer), and WRITE IT BACK so the next query
-- uses the fresh number instead of measuring again. Bounded by a timeout; on
-- failure return -1 and let the caller keep its existing estimate.
CREATE FUNCTION gfs.verify_source_rows(p_relid regclass) RETURNS bigint
LANGUAGE plpgsql AS $$
DECLARE src text; n bigint;
BEGIN
    SELECT source_ref INTO src FROM gfs.clone_source WHERE relid = p_relid;
    IF src IS NULL OR to_regclass(src) IS NULL THEN RETURN -1; END IF;
    BEGIN
        SET LOCAL statement_timeout = '30s';
        EXECUTE format('SELECT count(*) FROM %s', src) INTO n;
        SET LOCAL statement_timeout = '0';
    EXCEPTION WHEN others THEN
        RETURN -1;   -- unreachable or too slow: keep the estimate we already had
    END;
    UPDATE gfs.clone_source SET source_rows = GREATEST(n, 0) WHERE relid = p_relid;
    RETURN GREATEST(n, 0);
END;
$$;
COMMENT ON FUNCTION gfs.verify_source_rows(regclass) IS
  'Measure the source table''s real row count and store it (guards the copy-vs-ask decision against size drift)';

-- SCHEMA REPAIR ---------------------------------------------------------------
-- The clone's description of each source table (its imported FOREIGN TABLE) is
-- written ONCE at clone time and never refreshed. When the source changes shape,
-- that description goes stale and any federated query using it fails with a raw
-- remote error ("column ... does not exist"), which names SQL the user never wrote.
--
-- DDL moves no row counter, so schema drift is invisible to the row-level signals;
-- it is caught by comparing column digests (see gfs.source_probe).

-- The source table's current columns, straight from its catalog. One round trip.
CREATE FUNCTION gfs.source_columns(p_schema text, p_table text)
RETURNS TABLE(colname text, coltype text) LANGUAGE plpgsql AS $$
BEGIN
    RETURN QUERY SELECT * FROM dblink('gfs_remote_srv', format($q$
        SELECT a.attname::text, format_type(a.atttypid, a.atttypmod)::text
          FROM pg_attribute a
          JOIN pg_class c ON c.oid = a.attrelid
          JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = %L AND c.relname = %L
           AND a.attnum > 0 AND NOT a.attisdropped
         ORDER BY a.attnum
    $q$, p_schema, p_table)) AS t(colname text, coltype text);
END;
$$;
COMMENT ON FUNCTION gfs.source_columns(text, text) IS 'Current columns of a table on the source';

-- Re-import one table's definition so the clone matches the source again.
--
-- Deliberately split by risk, mirroring how a data conflict is handled: an
-- ADDITIVE change (the source gained a column) is applied automatically, while a
-- DESTRUCTIVE one (a column that exists locally is gone from the source) is
-- refused and reported. Dropping that column locally could destroy data the user
-- put there, and the same rule holds no matter what the policy says: never
-- silently discard the user's work.
--
-- Returns 'repaired: ...' or 'conflict: ...'.
CREATE FUNCTION gfs.repair_schema(p_relid regclass) RETURNS text LANGUAGE plpgsql AS $$
DECLARE m record; added text[]; removed text[]; c record; ftname text;
BEGIN
    SELECT * INTO m FROM gfs.source_map WHERE relid = p_relid;
    IF m.relid IS NULL THEN RETURN 'conflict: not a registered clone table'; END IF;

    IF to_regclass('pg_temp.gfs_srccols') IS NULL THEN
        CREATE TEMP TABLE gfs_srccols(colname text, coltype text) ON COMMIT DROP;
    END IF;
    DELETE FROM gfs_srccols;
    BEGIN
        INSERT INTO gfs_srccols SELECT * FROM gfs.source_columns(m.src_schema, m.src_table);
    EXCEPTION WHEN others THEN
        RETURN format('conflict: cannot read %s.%s on the source (%s)', m.src_schema, m.src_table, SQLERRM);
    END;

    IF NOT EXISTS (SELECT 1 FROM gfs_srccols) THEN
        RETURN format('conflict: %s.%s no longer exists on the source; the local copy is orphaned',
                      m.src_schema, m.src_table);
    END IF;

    -- present locally but gone from the source -> destructive, never automatic
    SELECT array_agg(a.attname ORDER BY a.attnum) INTO removed
      FROM pg_attribute a
     WHERE a.attrelid = p_relid AND a.attnum > 0 AND NOT a.attisdropped
       AND NOT EXISTS (SELECT 1 FROM gfs_srccols s WHERE s.colname = a.attname);
    IF removed IS NOT NULL THEN
        RETURN format('conflict: the source no longer has column(s) %s; not dropped locally because that may destroy your data',
                      array_to_string(removed, ', '));
    END IF;

    -- present on the source but not locally -> additive, safe to apply
    SELECT array_agg(s.colname) INTO added
      FROM gfs_srccols s
     WHERE NOT EXISTS (SELECT 1 FROM pg_attribute a
                        WHERE a.attrelid = p_relid AND a.attnum > 0 AND NOT a.attisdropped
                          AND a.attname = s.colname);

    -- refresh the foreign table so federated queries stop asking for stale columns
    ftname := m.ftrel::text;
    BEGIN
        EXECUTE format('DROP FOREIGN TABLE IF EXISTS %s', ftname);
        EXECUTE format('IMPORT FOREIGN SCHEMA %I LIMIT TO (%I) FROM SERVER gfs_remote_srv INTO %I',
                       m.src_schema, m.src_table, split_part(ftname, '.', 1));
    EXCEPTION WHEN others THEN
        RETURN format('conflict: could not re-import %s (%s)', ftname, SQLERRM);
    END;

    -- mirror any new columns locally, so a refetch can populate them
    IF added IS NOT NULL THEN
        FOR c IN SELECT colname, coltype FROM gfs_srccols WHERE colname = ANY(added) LOOP
            BEGIN
                EXECUTE format('ALTER TABLE %s ADD COLUMN %I %s', p_relid::text, c.colname, c.coltype);
            EXCEPTION WHEN others THEN
                RETURN format('conflict: could not add column %I locally (%s)', c.colname, SQLERRM);
            END;
        END LOOP;
    END IF;

    -- Constraints the source has and we do not. Applying one can legitimately fail
    -- when rows already stored locally violate it (a CHECK added upstream after
    -- this clone diverged), which is a conflict for the user to resolve, not
    -- something to force -- so report it rather than dropping their rows.
    FOR c IN
        SELECT d.def FROM dblink('gfs_remote_srv', format($q$
            SELECT pg_get_constraintdef(con.oid)
              FROM pg_constraint con
              JOIN pg_class cl ON cl.oid = con.conrelid
              JOIN pg_namespace nsp ON nsp.oid = cl.relnamespace
             WHERE nsp.nspname = %L AND cl.relname = %L
               AND con.contype IN ('c','u','x')
        $q$, m.src_schema, m.src_table)) AS d(def text)
    LOOP
        CONTINUE WHEN EXISTS (SELECT 1 FROM pg_constraint lc
                               WHERE lc.conrelid = p_relid
                                 AND pg_get_constraintdef(lc.oid) = c.def);
        BEGIN
            EXECUTE format('ALTER TABLE %s ADD %s', p_relid::text, c.def);
        EXCEPTION WHEN others THEN
            RETURN format('conflict: the source added %s but the rows already in this clone do not satisfy it (%s)',
                          c.def, SQLERRM);
        END;
    END LOOP;

    -- the shape changed, so whatever was copied is no longer a faithful copy
    PERFORM gfs.resync_table(p_relid);
    UPDATE gfs.drift_state SET schema_drifted = false, checked_at = clock_timestamp()
     WHERE relid = p_relid;

    RETURN CASE WHEN added IS NULL THEN 'repaired: definition re-imported'
                ELSE format('repaired: definition re-imported, added column(s) %s', array_to_string(added, ', ')) END;
END;
$$;
COMMENT ON FUNCTION gfs.repair_schema(regclass) IS
  'Re-import one table''s definition from the source; additive changes are applied, destructive ones reported as conflicts';

CREATE TABLE gfs.cached (
    relid regclass NOT NULL REFERENCES gfs.clone_source(relid) ON DELETE CASCADE,
    lo    bigint   NOT NULL,
    hi    bigint   NOT NULL
);
CREATE INDEX ON gfs.cached (relid);
COMMENT ON TABLE gfs.cached IS 'Hydrated key ranges per clone table (range-granular completeness for elision)';

CREATE TABLE gfs.cached_predicate (
    relid      regclass NOT NULL REFERENCES gfs.clone_source(relid) ON DELETE CASCADE,
    pred       text     NOT NULL,
    complete   boolean  NOT NULL DEFAULT false,  -- true = matching rows fully hydrated -> serve local
    overflowed boolean  NOT NULL DEFAULT false,  -- true = capped pull overflowed (not selective) -> never partial again
    queued     boolean  NOT NULL DEFAULT false,  -- true = an ASYNC partial copy is pending in the background -> federate meanwhile
    PRIMARY KEY (relid, pred)
);
COMMENT ON TABLE gfs.cached_predicate IS 'Non-key predicates seen by the router: complete=fully hydrated (local), overflowed=too many matches (federate), queued=async copy pending (federate meanwhile). A bare row (all false) is a second-chance "seen once" marker.';
-- The async copy worker scans for queued-but-not-yet-done predicates to drain.
CREATE INDEX cached_predicate_queued_idx ON gfs.cached_predicate (relid)
    WHERE queued AND NOT complete AND NOT overflowed;

-- Async copy queue for the background worker: typed jobs BEYOND the predicate
-- partial (selective predicates stay on gfs.cached_predicate.queued, untouched).
--   kind='whole' -> own the whole table (lo/hi unused);
--   kind='time'  -> capped temporal slice over [lo,hi] (epoch microseconds on the date/timestamp key).
-- The router enqueues here and federates the query for an instant answer; the worker
-- performs the copy off the critical path, exactly like the predicate path. A job is
-- removed once run (completeness is recorded in clone_source.whole_cached / gfs.cached).
CREATE TABLE gfs.copy_queue (
    relid       regclass    NOT NULL REFERENCES gfs.clone_source(relid) ON DELETE CASCADE,
    kind        text        NOT NULL CHECK (kind IN ('whole','time','resync','driftcheck','schemafix')),
    lo          bigint      NOT NULL DEFAULT 0,
    hi          bigint      NOT NULL DEFAULT 0,
    enqueued_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (relid, kind, lo, hi)
);
COMMENT ON TABLE gfs.copy_queue IS 'Pending async work (kind=whole|time|resync|driftcheck|schemafix) the background worker drains off the query critical path.';

-- Copy-on-write DELETE tombstones: a user DELETE on a clone table records the
-- deleted row's PRIMARY KEY (as jsonb) here, so later copy-on-read hydration never
-- re-fetches/resurrects it. Matched by `to_jsonb(source_row) @> pk`.
CREATE TABLE gfs.tombstone (
    relid regclass NOT NULL REFERENCES gfs.clone_source(relid) ON DELETE CASCADE,
    pk    jsonb    NOT NULL,
    PRIMARY KEY (relid, pk)
);
COMMENT ON TABLE gfs.tombstone IS 'PRIMARY KEYs of locally-deleted rows; hydration excludes them so a local DELETE is never resurrected';

CREATE FUNCTION gfs.note_tombstone() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE pkcols text[]; pkjson jsonb;
BEGIN
    SELECT array_agg(a.attname) INTO pkcols
      FROM pg_index i JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
     WHERE i.indrelid = TG_RELID AND i.indisprimary;
    IF pkcols IS NULL THEN RETURN OLD; END IF;            -- keyless table: nothing to tombstone
    SELECT jsonb_object_agg(k, v) INTO pkjson
      FROM jsonb_each(to_jsonb(OLD)) AS j(k, v) WHERE k = ANY(pkcols);
    INSERT INTO gfs.tombstone(relid, pk) VALUES (TG_RELID, pkjson) ON CONFLICT DO NOTHING;
    RETURN OLD;
END $$;
COMMENT ON FUNCTION gfs.note_tombstone() IS 'AFTER DELETE trigger: record the deleted row PK so hydration never resurrects it';

CREATE TABLE gfs.clone_stats (
    relid          regclass PRIMARY KEY REFERENCES gfs.clone_source(relid) ON DELETE CASCADE,
    fetch_calls    bigint NOT NULL DEFAULT 0,
    rows_fetched   bigint NOT NULL DEFAULT 0,
    federate_calls bigint NOT NULL DEFAULT 0,  -- times this table was pushed to the source
    last_fetch     timestamptz
);
COMMENT ON TABLE gfs.clone_stats IS 'Copy-on-read observability per clone table';

-- Insert a hydrated key range, then coalesce overlapping/adjacent ranges for the
-- table into a minimal disjoint set (so coverage checks stay O(1) per query and
-- elision works across spans). Integer key ranges only.
CREATE FUNCTION gfs.note_range(R regclass, p_lo bigint, p_hi bigint) RETURNS void
LANGUAGE plpgsql AS $$
DECLARE los bigint[]; his bigint[];
BEGIN
    INSERT INTO gfs.cached(relid, lo, hi) VALUES (R, p_lo, p_hi);
    -- gaps-and-islands merge (adjacency = +1) into arrays, BEFORE deleting.
    SELECT array_agg(lo ORDER BY lo), array_agg(hi ORDER BY lo)
      INTO los, his
      FROM (
        SELECT min(lo) AS lo, max(hi) AS hi
          FROM (
            SELECT lo, hi, sum(brk) OVER (ORDER BY lo, hi) AS g
              FROM (
                SELECT lo, hi,
                       CASE WHEN lo <= COALESCE(max(hi) OVER (
                              ORDER BY lo, hi ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING), lo) + 1
                            THEN 0 ELSE 1 END AS brk
                  FROM gfs.cached WHERE relid = R
              ) s
          ) g
         GROUP BY g
      ) m;
    DELETE FROM gfs.cached WHERE relid = R;
    INSERT INTO gfs.cached(relid, lo, hi)
        SELECT R, unnest(los), unnest(his);
END;
$$;

CREATE FUNCTION gfs.register_clone(local regclass, source_ref text, key_col text DEFAULT 'id')
RETURNS void LANGUAGE plpgsql AS $$
DECLARE kind text := 'whole'; j json; srows bigint := 0; sbytes int := 100;
BEGIN
    -- range-key strategy: integer keys hydrate key ranges; date/timestamp keys
    -- hydrate capped TIME ranges (epoch-micros coverage); everything else whole.
    SELECT CASE WHEN t.typname IN ('int2','int4','int8') THEN 'int'
                WHEN t.typname IN ('date','timestamp','timestamptz') THEN 'time'
                ELSE 'whole' END
      INTO kind
      FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid
     WHERE a.attrelid = local AND a.attname = key_col;
    kind := COALESCE(kind, 'whole');

    -- Cost-model stats from the SOURCE's planner estimate (reltuples + width) via
    -- postgres_fdw remote estimate -- NO scan, so it stays cheap on a multi-TB
    -- source. We toggle use_remote_estimate just for this EXPLAIN (then reset it so
    -- normal query planning doesn't pay a remote round-trip). Best-effort defaults.
    BEGIN
        BEGIN EXECUTE format('ALTER FOREIGN TABLE %s OPTIONS (ADD use_remote_estimate %L)', source_ref, 'true');
        EXCEPTION WHEN others THEN
            BEGIN EXECUTE format('ALTER FOREIGN TABLE %s OPTIONS (SET use_remote_estimate %L)', source_ref, 'true'); EXCEPTION WHEN others THEN NULL; END;
        END;
        EXECUTE format('EXPLAIN (FORMAT JSON) SELECT * FROM %s', source_ref) INTO j;
        srows  := GREATEST((j->0->'Plan'->>'Plan Rows')::bigint, 0);
        sbytes := GREATEST((j->0->'Plan'->>'Plan Width')::int, 1);
        BEGIN EXECUTE format('ALTER FOREIGN TABLE %s OPTIONS (DROP use_remote_estimate)', source_ref); EXCEPTION WHEN others THEN NULL; END;
    EXCEPTION WHEN others THEN srows := 0; sbytes := 100;
    END;

    -- A remote estimate of <= 1 row almost always means the SOURCE table was never
    -- ANALYZEd (its reltuples is stale), NOT that it is genuinely tiny. Trusting it
    -- makes the router treat a million-row table as negligible and whole-copy it on
    -- first read over a slow link -- the same failure class as issue #112, from stale
    -- stats instead of a bad probe. Get the REAL size with count(*), which postgres_fdw
    -- pushes down to the source (one round-trip, no data transfer). Bound it with a
    -- timeout so a giant un-analyzed table cannot stall the clone; on timeout/error
    -- assume "large" so the router federates rather than eagerly copies.
    IF srows <= 1 THEN
        BEGIN
            SET LOCAL statement_timeout = '30s';
            EXECUTE format('SELECT count(*) FROM %s', source_ref) INTO srows;
            srows := GREATEST(srows, 0);
            SET LOCAL statement_timeout = '0';
        EXCEPTION WHEN others THEN
            srows := 100000000;   -- couldn't measure -> assume large (federate, never copy on first touch)
        END;
    END IF;

    INSERT INTO gfs.clone_source(relid, source_ref, key_col, chunk_kind, source_rows, row_bytes)
         VALUES (local, source_ref, key_col, kind, srows, sbytes)
    ON CONFLICT (relid)
        DO UPDATE SET source_ref = EXCLUDED.source_ref, key_col = EXCLUDED.key_col,
                      chunk_kind = EXCLUDED.chunk_kind, source_rows = EXCLUDED.source_rows,
                      row_bytes = EXCLUDED.row_bytes;
    INSERT INTO gfs.clone_stats(relid) VALUES (local) ON CONFLICT (relid) DO NOTHING;
    -- Record local DELETEs so hydration never resurrects them (copy-on-write).
    EXECUTE format('CREATE OR REPLACE TRIGGER gfs_tombstone AFTER DELETE ON %s
                    FOR EACH ROW EXECUTE FUNCTION gfs.note_tombstone()', local);
END;
$$;
COMMENT ON FUNCTION gfs.register_clone(regclass, text, text) IS
  'Register <local> as a copy-on-read clone of foreign relation <source_ref>';

CREATE FUNCTION gfs.unregister_clone(local regclass)
RETURNS void LANGUAGE sql AS $$
    DELETE FROM gfs.clone_source WHERE relid = local;
$$;

-- Force a clone table fully local (and mark it owned -> future queries never hit
-- the source, even aggregates).
CREATE FUNCTION gfs.warm(local regclass)
RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE src text; cols text; ov text; n bigint; old_srr text; srcfrom text; rqual text; cdef text; oc text; arb text;
BEGIN
    SELECT source_ref INTO src FROM gfs.clone_source WHERE relid = local;
    IF src IS NULL OR to_regclass(src) IS NULL THEN
        RAISE EXCEPTION 'gfs.warm: % is not a registered clone (or its source is gone)', local;
    END IF;
    SELECT string_agg(quote_ident(attname), ', ' ORDER BY attnum) INTO cols
      FROM pg_attribute
     WHERE attrelid = local AND attnum > 0 AND NOT attisdropped AND attgenerated = '';
    -- Inheritance parent (mirrored from the source by the schema replay): warm ONLY
    -- its own rows. postgres_fdw cannot deparse ONLY (the foreign scan would include
    -- child rows, which the clone's inheritance scan then reads AGAIN from the warmed
    -- children), so read through dblink with an explicit FROM ONLY against the real
    -- source-side table behind the foreign table.
    IF EXISTS (SELECT 1 FROM pg_inherits i JOIN pg_class pc ON pc.oid = i.inhparent
                WHERE i.inhparent = local AND pc.relkind = 'r') THEN
        SELECT format('%I.%I',
                      COALESCE((SELECT option_value FROM pg_options_to_table(ft.ftoptions) WHERE option_name = 'schema_name'), n.nspname),
                      COALESCE((SELECT option_value FROM pg_options_to_table(ft.ftoptions) WHERE option_name = 'table_name'), c.relname))
          INTO rqual
          FROM pg_foreign_table ft
          JOIN pg_class c ON c.oid = ft.ftrelid
          JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE ft.ftrelid = to_regclass(src);
        SELECT string_agg(quote_ident(attname) || ' ' || format_type(atttypid, atttypmod), ', ' ORDER BY attnum)
          INTO cdef
          FROM pg_attribute
         WHERE attrelid = local AND attnum > 0 AND NOT attisdropped AND attgenerated = '';
        srcfrom := format('dblink(''gfs_remote_srv'', %L) AS s(%s)',
                          format('SELECT %s FROM ONLY %s', cols, rqual), cdef);
    ELSE
        srcfrom := src || ' s';
    END IF;
    -- A GENERATED ALWAYS AS IDENTITY column rejects an explicit value on a plain
    -- INSERT; OVERRIDING SYSTEM VALUE lets us copy the source's own key faithfully.
    SELECT CASE WHEN EXISTS (SELECT 1 FROM pg_attribute
                              WHERE attrelid = local AND attidentity = 'a')
                THEN 'OVERRIDING SYSTEM VALUE ' ELSE '' END INTO ov;
    -- Target-less ON CONFLICT is refused when the table has a DEFERRABLE unique or
    -- exclusion constraint (they cannot arbitrate) -- warming would error. Name an
    -- explicit arbiter instead: a non-deferrable, non-partial, non-expression unique
    -- index (primary key preferred). Re-pulled source rows match on ANY unique
    -- index, so one arbiter is enough to keep warming idempotent.
    IF EXISTS (SELECT 1 FROM pg_constraint WHERE conrelid = local
                AND contype IN ('p','u','x') AND condeferrable) THEN
        SELECT '(' || string_agg(quote_ident(a.attname), ', ' ORDER BY k.ord) || ')' INTO arb
          FROM pg_index i
          JOIN LATERAL unnest(i.indkey::int[]) WITH ORDINALITY AS k(attnum, ord) ON true
          JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
         WHERE i.indrelid = local AND i.indisunique AND i.indimmediate
           AND i.indpred IS NULL AND 0 <> ALL (i.indkey::int[])
         GROUP BY i.indexrelid, i.indisprimary, i.indnkeyatts
         ORDER BY i.indisprimary DESC, i.indnkeyatts ASC, i.indexrelid LIMIT 1;
        IF arb IS NULL THEN
            RAISE EXCEPTION 'gfs.warm: % has only DEFERRABLE unique constraints -- no usable ON CONFLICT arbiter', local;
        END IF;
        oc := format('ON CONFLICT %s DO NOTHING', arb);
    ELSE
        oc := 'ON CONFLICT DO NOTHING';
    END IF;
    -- Warming replays the SOURCE's already-computed rows, so a replayed INSERT trigger
    -- must NOT fire on them (it would re-mutate the row or re-run side effects, diverging
    -- the clone from the source). Copy in the 'replica' role -- how logical replication
    -- applies rows, skipping user triggers -- then restore the prior role (SET LOCAL, so
    -- it also resets at txn end) so later writes fire their triggers normally.
    old_srr := current_setting('session_replication_role');
    PERFORM set_config('session_replication_role', 'replica', true);
    -- Exclude locally-deleted rows so warming never resurrects a copy-on-write DELETE.
    EXECUTE format('INSERT INTO %s (%s) %sSELECT %s FROM %s
                    WHERE NOT EXISTS (SELECT 1 FROM gfs.tombstone tb
                                       WHERE tb.relid = %L::regclass AND to_jsonb(s) @> tb.pk)
                    %s',
                   local::text, cols, ov, cols, srcfrom, local::text, oc);
    GET DIAGNOSTICS n = ROW_COUNT;
    PERFORM set_config('session_replication_role', old_srr, true);
    EXECUTE format('ANALYZE %s', local::text);
    UPDATE gfs.clone_source SET whole_cached = true WHERE relid = local;
    UPDATE gfs.clone_stats
       SET fetch_calls = fetch_calls + 1, rows_fetched = rows_fetched + n, last_fetch = now()
     WHERE relid = local;
    RETURN n;
END;
$$;
COMMENT ON FUNCTION gfs.warm(regclass) IS
  'Fully materialize + own a clone table (served local thereafter, no source contact)';

CREATE VIEW gfs.clones AS
    SELECT s.relid::text AS clone, s.source_ref, s.key_col, s.chunk_kind, s.whole_cached,
           s.source_rows, s.row_bytes, s.access_count, s.partial_rows, s.no_partial,
           COALESCE(st.fetch_calls, 0)    AS fetch_calls,
           COALESCE(st.rows_fetched, 0)   AS rows_fetched,
           COALESCE(st.federate_calls, 0) AS federate_calls,
           (SELECT count(*) FROM gfs.cached c WHERE c.relid = s.relid) AS cached_ranges,
           (SELECT count(*) FROM gfs.cached_predicate p WHERE p.relid = s.relid AND p.complete) AS cached_preds,
           st.last_fetch
      FROM gfs.clone_source s
      LEFT JOIN gfs.clone_stats st USING (relid)
     ORDER BY s.relid::text;

GRANT USAGE ON SCHEMA gfs TO PUBLIC;
GRANT SELECT ON gfs.clone_source, gfs.cached, gfs.cached_predicate, gfs.copy_queue, gfs.tombstone, gfs.clone_stats, gfs.cost, gfs.budget, gfs.clones TO PUBLIC;
