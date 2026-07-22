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
                                    || ':' || a.attnotnull::text, ',' ORDER BY a.attnum)) AS fp
                FROM pg_class c
                JOIN pg_namespace n ON n.oid = c.relnamespace
                JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
               WHERE c.relkind IN ('r','p')
                 AND n.nspname NOT IN ('pg_catalog','information_schema')
               GROUP BY 1,2) y), '[]'::json)
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
    INSERT INTO gfs.source_table_baseline(relid, writes, live_tup, src_fp, ft_fp, loc_fp)
    SELECT m.relid,
           COALESCE(st.w, 0), COALESCE(st.l, 0), sc.fp,
           gfs.relation_fp(m.ftrel), gfs.relation_fp(m.relid)
      FROM gfs.source_map m
      LEFT JOIN LATERAL (
            SELECT (e->>'w')::bigint AS w, (e->>'l')::bigint AS l
              FROM jsonb_array_elements(p->'tables') e
             WHERE e->>'s' = m.src_schema AND e->>'r' = m.src_table) st ON true
      LEFT JOIN LATERAL (
            SELECT e->>'fp' AS fp
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

    CREATE TEMP TABLE IF NOT EXISTS gfs_drift_now (
        kind text, src_table text, detail text) ON COMMIT DROP;
    DELETE FROM gfs_drift_now;

    INSERT INTO gfs_drift_now(kind, src_table, detail)
    SELECT d.kind, q.tbl, d.detail FROM (
        SELECT m.src_schema || '.' || m.src_table AS tbl,
               b.writes, b.live_tup, b.src_fp,
               COALESCE(st.w, 0) AS w_now, COALESCE(st.l, 0) AS l_now,
               sc.fp AS fp_now,
               gfs.relation_fp(m.ftrel) AS ft_fp_now
          FROM gfs.source_table_baseline b
          JOIN gfs.source_map m ON m.relid = b.relid
          LEFT JOIN LATERAL (
                SELECT (e->>'w')::bigint AS w, (e->>'l')::bigint AS l
                  FROM jsonb_array_elements(p->'tables') e
                 WHERE e->>'s' = m.src_schema AND e->>'r' = m.src_table) st ON true
          LEFT JOIN LATERAL (
                SELECT e->>'fp' AS fp
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
              THEN 'the source table''s columns changed since clone time' END),
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
    SELECT count(*) INTO attributed FROM gfs_drift_now g WHERE g.kind IN ('data','schema');

    IF attributed = 0 THEN
        -- WAL moved but nothing accounts for it. TRUNCATE and table rewrites
        -- move no row counter, so this is NOT "nothing changed": treat every
        -- materialized table as suspect. Failing safe is the whole point.
        INSERT INTO gfs_drift_now(kind, src_table, detail)
        VALUES ('unattributed', '(all materialized tables)',
                format('source moved (WAL %s -> %s) but no table accounts for it; treat every copied table as suspect', base, cur));
    END IF;

    RAISE WARNING 'gfs: the source changed since this clone was created (WAL % -> %). This clone may mix data from different points in time.', base, cur;
    RETURN QUERY SELECT g.kind, g.src_table, g.detail FROM gfs_drift_now g ORDER BY g.kind, g.src_table;
END;
$$;
COMMENT ON FUNCTION gfs.source_drift() IS
  'What changed on the source since clone time (empty = untouched); kinds: data, schema, unfederatable, unattributed';

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
    kind        text        NOT NULL CHECK (kind IN ('whole','time')),
    lo          bigint      NOT NULL DEFAULT 0,
    hi          bigint      NOT NULL DEFAULT 0,
    enqueued_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (relid, kind, lo, hi)
);
COMMENT ON TABLE gfs.copy_queue IS 'Pending async copies (kind=whole|time) the background worker drains off the query critical path.';

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
