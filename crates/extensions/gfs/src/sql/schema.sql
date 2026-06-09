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
    no_partial   boolean  NOT NULL DEFAULT false     -- terminal: too big to own; federate per call, no more probes
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

    t0 := clock_timestamp();
    EXECUTE format('SELECT 1 FROM %s LIMIT 1', fref);
    t1 := clock_timestamp();
    lat := GREATEST(extract(epoch FROM t1 - t0), 1e-6);

    t0 := clock_timestamp();                                   -- pull `sample` rows
    EXECUTE format('SELECT count(*) FROM (SELECT * FROM %s LIMIT %s) s', fref, sample);
    t1 := clock_timestamp();
    net_s := GREATEST(extract(epoch FROM t1 - t0) - lat, 1e-9) / GREATEST(sample * b, 1);

    t0 := clock_timestamp();                                   -- source scans up to `sample` rows
    EXECUTE format('SELECT count(*) FROM (SELECT 1 FROM %s LIMIT %s) s', fref, sample);
    t1 := clock_timestamp();
    scanned := LEAST(sample::bigint, tr);
    src_s := GREATEST(extract(epoch FROM t1 - t0) - lat, 1e-9) / GREATEST(scanned, 1);

    UPDATE gfs.cost SET net = net_s, source = src_s * pl, negligible = lat
      RETURNING * INTO r;
    RETURN r;
END;
$$;
COMMENT ON FUNCTION gfs.calibrate(int) IS
  'Probe the source (network throughput, scan rate, latency) and set the cost weights accordingly';

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
DECLARE src text; cols text; n bigint;
BEGIN
    SELECT source_ref INTO src FROM gfs.clone_source WHERE relid = local;
    IF src IS NULL OR to_regclass(src) IS NULL THEN
        RAISE EXCEPTION 'gfs.warm: % is not a registered clone (or its source is gone)', local;
    END IF;
    SELECT string_agg(quote_ident(attname), ', ' ORDER BY attnum) INTO cols
      FROM pg_attribute
     WHERE attrelid = local AND attnum > 0 AND NOT attisdropped AND attgenerated = '';
    -- Exclude locally-deleted rows so warming never resurrects a copy-on-write DELETE.
    EXECUTE format('INSERT INTO %s (%s) SELECT %s FROM %s s
                    WHERE NOT EXISTS (SELECT 1 FROM gfs.tombstone tb
                                       WHERE tb.relid = %L::regclass AND to_jsonb(s) @> tb.pk)
                    ON CONFLICT DO NOTHING',
                   local::text, cols, cols, src, local::text);
    GET DIAGNOSTICS n = ROW_COUNT;
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
GRANT SELECT ON gfs.clone_source, gfs.cached, gfs.cached_predicate, gfs.tombstone, gfs.clone_stats, gfs.cost, gfs.budget, gfs.clones TO PUBLIC;
