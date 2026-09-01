//! Hydration engine: pull a needed slice/range/whole table into the local heap
//! (single-statement FDW or a parallel dblink fan-out), record coverage, and keep
//! the source untouched on any incomplete pull.

use std::ffi::CString;

use pgrx::pg_sys;

use crate::catalog::{gfs_throttle, spi_text};
use crate::keyrange::{time_recon, TIME_FAR_FUTURE, TIME_FAR_PAST};
use crate::model::Hydration;

/// Hard cap on concurrent dblink scans regardless of the gfs.cost knob (one source
/// gets at most this many parallel readers per backfill, to protect prod).
const PARALLEL_WORKERS_CAP: i64 = 8;

/// Set by the async copy worker (a separate process) so its hydrations skip the
/// non-essential per-table catalog bookkeeping -- `clone_source.partial_rows` and
/// `clone_stats` -- that every query also updates (`bump_access` / `bump_federate`).
/// Touching those shared rows from the worker's separate transaction forms a
/// row-lock CYCLE with concurrent queries on the same table; skipping them makes the
/// worker share no lock with foreground queries (the row copy + cached_predicate
/// completeness, the only correctness-critical writes, still happen). Always false
/// in normal backends (the synchronous path keeps full bookkeeping).
pub(crate) static mut DEFER_BOOKKEEPING: bool = false;

/// Record coverage (whole_cached / coalesced range) and refresh planner stats after
/// a whole/int-range fetch. Shared by the single-statement path and the parallel
/// backfill. Caller holds an open SPI connection.
unsafe fn record_whole_or_range(h: &Hydration, n: i64) {
    let rec = if h.whole {
        format!("UPDATE gfs.clone_source SET whole_cached = true WHERE relid::oid = {}", u32::from(h.relid))
    } else {
        format!("SELECT gfs.note_range({}::oid::regclass, {}, {})", u32::from(h.relid), h.lo, h.hi)
    };
    pg_sys::SPI_execute(CString::new(rec).unwrap().as_ptr(), false, 0);
    // #131: stamp the copy event (where the source was when these rows landed).
    // Only the WHOLE branch stamps here -- gfs.note_range stamps range fetches
    // itself, so adding one for them too would probe the source twice. An
    // explicit call, not a trigger: this runs inside the replica-role window,
    // where ordinary triggers silently do not fire.
    if h.whole {
        let wm = CString::new(format!("SELECT gfs.note_copy({}::oid::regclass)", u32::from(h.relid))).unwrap();
        pg_sys::SPI_execute(wm.as_ptr(), false, 0);
    }
    hydrate_finish(h, n);
}

/// Disconnect dblink backfill connections `0..upto` (best-effort cleanup on bail).
unsafe fn cleanup_backfill_conns(relid: pg_sys::Oid, upto: usize) {
    for k in 0..upto {
        let d = CString::new(format!("SELECT dblink_disconnect('gfs_bf_{}_{}')", u32::from(relid), k)).unwrap();
        pg_sys::SPI_execute(d.as_ptr(), false, 0);
    }
}

/// Read column 1 of the single-row result of the just-run SPI SELECT as text.
unsafe fn spi_cell1() -> Option<String> {
    if pg_sys::SPI_processed != 1 {
        return None;
    }
    let tt = pg_sys::SPI_tuptable;
    let row = *(*tt).vals;
    spi_text(pg_sys::SPI_getvalue(row, (*tt).tupdesc, 1))
}

/// `OVERRIDING SYSTEM VALUE ` when the local clone table has a GENERATED ALWAYS AS
/// IDENTITY column, else empty. Such a column rejects an explicit value on a plain
/// INSERT, so every hydration INSERT must carry this clause to write the SOURCE's
/// own key value (a faithful copy keeps the source key, never a fresh local
/// sequence value). Identity columns are `attidentity = 'a'` (always); `'d'` (by
/// default) already accepts explicit values without the clause, so only `'a'` needs
/// it. Caller holds an open SPI connection.
unsafe fn overriding_clause(local_ref: &str) -> &'static str {
    let q = CString::new(format!(
        "SELECT EXISTS(SELECT 1 FROM pg_attribute WHERE attrelid = '{}'::regclass AND attidentity = 'a')::int::text",
        local_ref.replace('\'', "''")
    ))
    .unwrap();
    if pg_sys::SPI_execute(q.as_ptr(), true, 1) == pg_sys::SPI_OK_SELECT as i32
        && spi_cell1().as_deref() == Some("1")
    {
        return "OVERRIDING SYSTEM VALUE ";
    }
    ""
}

/// True when the local clone table is a plain-table inheritance parent (pg_inherits,
/// relkind 'r'). The clone replays the source schema, so this equals "the SOURCE
/// table has INHERITS children". Declarative-partition parents (relkind 'p') are
/// excluded: they hold no rows of their own and route inserts to partitions, so they
/// keep the existing paths. Caller holds an open SPI connection.
unsafe fn local_has_children(relid: pg_sys::Oid) -> bool {
    let q = CString::new(format!(
        "SELECT EXISTS(SELECT 1 FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhparent \
          WHERE i.inhparent = {} AND c.relkind = 'r')::int::text",
        u32::from(relid)
    ))
    .unwrap();
    pg_sys::SPI_execute(q.as_ptr(), true, 1) == pg_sys::SPI_OK_SELECT as i32
        && spi_cell1().as_deref() == Some("1")
}

/// Real source-side `schema.table` (quoted) behind the foreign table `fref`; None
/// when `fref` is not a foreign table. Caller holds an open SPI connection.
unsafe fn remote_qualified(fref: &str) -> Option<String> {
    let fq = CString::new(format!(
        "SELECT quote_ident(COALESCE((SELECT option_value FROM pg_options_to_table(ft.ftoptions) WHERE option_name = 'schema_name'), n.nspname)), \
                quote_ident(COALESCE((SELECT option_value FROM pg_options_to_table(ft.ftoptions) WHERE option_name = 'table_name'), c.relname)) \
           FROM pg_foreign_table ft JOIN pg_class c ON c.oid = ft.ftrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
          WHERE ft.ftrelid = '{}'::regclass",
        fref.replace('\'', "''")
    )).unwrap();
    if pg_sys::SPI_execute(fq.as_ptr(), true, 1) != pg_sys::SPI_OK_SELECT as i32 || pg_sys::SPI_processed != 1 {
        return None;
    }
    let tt = pg_sys::SPI_tuptable;
    let row = *(*tt).vals;
    let td = (*tt).tupdesc;
    let sch = spi_text(pg_sys::SPI_getvalue(row, td, 1))?;
    let tbl = spi_text(pg_sys::SPI_getvalue(row, td, 2))?;
    Some(format!("{}.{}", sch, tbl))
}

/// Typed column list of `local_ref` (non-dropped, non-generated, attnum order), for
/// use as a dblink result-set definition. Caller holds an open SPI connection.
unsafe fn local_coldef(local_ref: &str) -> Option<String> {
    let cq = CString::new(format!(
        "SELECT string_agg(quote_ident(attname) || ' ' || format_type(atttypid, atttypmod), ', ' ORDER BY attnum) \
           FROM pg_attribute WHERE attrelid = '{}'::regclass AND attnum > 0 AND NOT attisdropped AND attgenerated = ''",
        local_ref.replace('\'', "''")
    )).unwrap();
    if pg_sys::SPI_execute(cq.as_ptr(), true, 1) != pg_sys::SPI_OK_SELECT as i32 {
        return None;
    }
    spi_cell1().filter(|s| !s.is_empty())
}

/// `ON CONFLICT ... DO NOTHING` clause for hydration inserts into `h.local_ref`.
/// The target-less form asks Postgres to consider EVERY unique/exclusion constraint
/// as a potential arbiter, and it refuses outright when any of them is DEFERRABLE
/// ("ON CONFLICT does not support deferrable unique constraints/exclusion
/// constraints as arbiters") -- the hydration insert would error and every read of
/// the table would fail. For such tables, name an explicit arbiter: the columns of
/// a non-deferrable (indimmediate), non-partial, non-expression unique index,
/// preferring the primary key (registration guarantees one exists -- the bootstrap
/// keycol query requires indimmediate). Hydration only needs to dedupe re-pulls of
/// already-hydrated keys, and a re-pulled source row matches on ANY unique index,
/// so a single arbiter suffices. Caller holds an open SPI connection.
unsafe fn conflict_clause(h: &Hydration) -> String {
    let lref = h.local_ref.replace('\'', "''");
    let q = CString::new(format!(
        "SELECT EXISTS(SELECT 1 FROM pg_constraint WHERE conrelid = '{}'::regclass \
           AND contype IN ('p','u','x') AND condeferrable)::int::text",
        lref
    ))
    .unwrap();
    if pg_sys::SPI_execute(q.as_ptr(), true, 1) != pg_sys::SPI_OK_SELECT as i32
        || spi_cell1().as_deref() != Some("1")
    {
        return "ON CONFLICT DO NOTHING".into(); // common case: nothing deferrable
    }
    let a = CString::new(format!(
        "SELECT '(' || string_agg(quote_ident(a.attname), ', ' ORDER BY k.ord) || ')' \
           FROM pg_index i \
           JOIN LATERAL unnest(i.indkey::int[]) WITH ORDINALITY AS k(attnum, ord) ON true \
           JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
          WHERE i.indrelid = '{}'::regclass AND i.indisunique AND i.indimmediate \
            AND i.indpred IS NULL AND 0 <> ALL (i.indkey::int[]) \
          GROUP BY i.indexrelid, i.indisprimary, i.indnkeyatts \
          ORDER BY i.indisprimary DESC, i.indnkeyatts ASC, i.indexrelid LIMIT 1",
        lref
    ))
    .unwrap();
    if pg_sys::SPI_execute(a.as_ptr(), true, 1) == pg_sys::SPI_OK_SELECT as i32 {
        if let Some(cols) = spi_cell1() {
            return format!("ON CONFLICT {} DO NOTHING", cols);
        }
    }
    pgrx::error!(
        "gfs: {} has only DEFERRABLE unique constraints -- no usable ON CONFLICT arbiter for copy-on-read",
        h.local_ref
    );
}

/// FROM clause (aliased `src`) reading the source rows for `h`. Normal case: the
/// registered foreign table (postgres_fdw pushes the caller's WHERE down). For an
/// inheritance parent (`only`) postgres_fdw is unusable -- it cannot deparse ONLY, so
/// its remote scan would include child rows, which the clone's own inheritance scan
/// then reads AGAIN from the hydrated children (every child row served twice). Read
/// through dblink with an explicit `FROM ONLY` instead, embedding the caller's
/// predicate and cap (a dblink rowset gets no pushdown; without them a partial
/// hydration would transfer the parent's whole own heap). The caller's local
/// WHERE/LIMIT re-apply over the rowset, harmlessly idempotent.
unsafe fn src_from(h: &Hydration, only: bool, remote_where: &str, remote_limit: i64) -> String {
    if !only {
        return format!("{} src", h.source_ref);
    }
    let Some(qual) = remote_qualified(&h.source_ref) else {
        pgrx::error!("gfs: no foreign table behind {} (inheritance parent needs an ONLY-fetch)", h.local_ref);
    };
    let Some(coldef) = local_coldef(&h.local_ref) else {
        pgrx::error!("gfs: no usable columns on {} (inheritance parent needs an ONLY-fetch)", h.local_ref);
    };
    let w = if remote_where.is_empty() { "true" } else { remote_where };
    let lim = if remote_limit > 0 { format!(" LIMIT {}", remote_limit) } else { String::new() };
    format!(
        "dblink('gfs_remote_srv', $gfsq$SELECT {} FROM ONLY {} WHERE {}{}$gfsq$) AS src({})",
        h.collist, qual, w, lim, coldef
    )
}

/// Fan a large whole/int-range backfill over N concurrent dblink scans against the
/// source -- CTID-block partitioning for a whole table (no usable key -> heap scan),
/// key-range split for an int range (indexed key) -- instead of one FDW cursor. The
/// N scans run concurrently on the source; we drain + insert locally. Returns
/// Some(rows_inserted) on success, or None to fall back to the single-statement path
/// (parallelism disabled, table too small, range not large enough, or source
/// metadata unavailable). Caller holds SPI open. Every per-worker insert is
/// ON CONFLICT DO NOTHING, so a fallback after a partial fan is idempotent/harmless.
/// Read-only on the source; no replication slot. dblink reuses the existing FDW
/// server `gfs_remote_srv` (+ its PUBLIC user mapping) -- no new connstr/secret.
unsafe fn try_parallel_backfill(h: &Hydration, has_tomb: bool, overriding: &str, only: bool, conflict: &str) -> Option<i64> {
    // --- knobs + source size estimate + dblink availability (one row) ---
    let q = CString::new(format!(
        "SELECT x.parallel_workers::text, x.parallel_min_pages::text, x.parallel_min_frac::text, \
                s.source_rows::text, s.row_bytes::text, \
                (to_regprocedure('dblink_send_query(text,text)') IS NOT NULL)::int::text \
           FROM gfs.cost x, gfs.clone_source s WHERE s.relid::oid = {}",
        u32::from(h.relid)
    )).unwrap();
    if pg_sys::SPI_execute(q.as_ptr(), true, 1) != pg_sys::SPI_OK_SELECT as i32 || pg_sys::SPI_processed != 1 {
        return None;
    }
    let tt = pg_sys::SPI_tuptable;
    let row = *(*tt).vals;
    let td = (*tt).tupdesc;
    let num = |i| spi_text(pg_sys::SPI_getvalue(row, td, i)).and_then(|s| s.trim().parse::<f64>().ok());
    let workers = num(1).unwrap_or(0.0) as i64;
    let min_pages = num(2).unwrap_or(f64::INFINITY);
    let min_frac = num(3).unwrap_or(1.0);
    let source_rows = num(4).unwrap_or(0.0);
    let row_bytes = num(5).unwrap_or(1.0).max(1.0);
    let has_dblink = num(6).unwrap_or(0.0) as i64 == 1;

    if workers <= 1 || !has_dblink {
        return None; // disabled (kill-switch), or dblink not installed -> single-statement path
    }
    let n = workers.clamp(1, PARALLEL_WORKERS_CAP) as usize;
    let est_pages = (source_rows.max(0.0) * row_bytes / 8192.0).ceil();
    if est_pages <= min_pages {
        return None; // too small to be worth fanning out
    }
    if !h.whole {
        let span = (h.hi.saturating_sub(h.lo)).saturating_add(1).max(0) as f64;
        if span < min_frac * source_rows.max(1.0) {
            return None; // a narrow range stays on the indexed single-statement path
        }
    }

    // --- real source-side schema.table behind the foreign table (quoted). An
    // inheritance parent scans ONLY its own heap: its children backfill themselves,
    // and both split modes stay valid (ctid ranges address the parent's own heap;
    // key ranges filter the parent's own rows). ---
    let src_qual = format!("{}{}", if only { "ONLY " } else { "" }, remote_qualified(&h.source_ref)?);

    // --- typed column list for dblink_get_result (same types as the local table) ---
    let coldef = local_coldef(&h.local_ref)?;

    // --- partition predicates ---
    let preds: Vec<String> = if h.whole {
        // CTID-block: [0, est_pages] split into n page ranges; last worker open-ended
        // (captures rows beyond the estimate). ctid is pushed verbatim by dblink.
        let per = (est_pages / n as f64).ceil().max(1.0) as i64;
        (0..n)
            .map(|k| {
                let lo = k as i64 * per;
                if k == n - 1 {
                    format!("ctid >= '({},0)'::tid", lo)
                } else {
                    format!("ctid >= '({},0)'::tid AND ctid < '({},0)'::tid", lo, (k as i64 + 1) * per)
                }
            })
            .collect()
    } else {
        // key-range split of [lo, hi] over the indexed int key
        let span = (h.hi - h.lo).saturating_add(1).max(1);
        let step = (span as f64 / n as f64).ceil().max(1.0) as i64;
        (0..n)
            .filter_map(|k| {
                let wlo = h.lo.saturating_add(k as i64 * step);
                if wlo > h.hi {
                    return None;
                }
                let whi = if k == n - 1 { h.hi } else { wlo.saturating_add(step - 1).min(h.hi) };
                Some(format!("{} BETWEEN {} AND {}", h.key_col, wlo, whi))
            })
            .collect()
    };
    if preds.is_empty() {
        return None;
    }
    let m = preds.len();

    // Tombstone exclusion re-aliased to the local result set `t` (the source query
    // can't see the local gfs.tombstone table; we filter after the fetch instead).
    let excl_t = if has_tomb {
        format!(" AND NOT EXISTS (SELECT 1 FROM gfs.tombstone tb WHERE tb.relid::oid = {} AND to_jsonb(t) @> tb.pk)", u32::from(h.relid))
    } else {
        String::new()
    };

    // Open all connections + dispatch all scans: the N source scans now run
    // concurrently. A connect/dispatch failure bails to the single-statement path.
    for (k, pred) in preds.iter().enumerate() {
        let conn = format!("gfs_bf_{}_{}", u32::from(h.relid), k);
        let c = CString::new(format!("SELECT dblink_connect('{}', 'gfs_remote_srv')", conn)).unwrap();
        if pg_sys::SPI_execute(c.as_ptr(), false, 0) != pg_sys::SPI_OK_SELECT as i32 {
            cleanup_backfill_conns(h.relid, k);
            return None;
        }
        // dollar-quote the remote SQL so the ctid literals need no escaping.
        let remote = format!("SELECT {} FROM {} WHERE {}", h.collist, src_qual, pred);
        let s = CString::new(format!("SELECT dblink_send_query('{}', $gfsq${}$gfsq$)", conn, remote)).unwrap();
        if pg_sys::SPI_execute(s.as_ptr(), false, 0) != pg_sys::SPI_OK_SELECT as i32 {
            cleanup_backfill_conns(h.relid, k + 1);
            return None;
        }
    }

    // Drain each result and insert locally (sequential locally; the slow source
    // scan + network already overlapped across workers).
    let mut total: i64 = 0;
    for k in 0..m {
        let conn = format!("gfs_bf_{}_{}", u32::from(h.relid), k);
        let ins = CString::new(format!(
            "INSERT INTO {l} ({c}) {ov}SELECT {c} FROM dblink_get_result('{conn}') AS t({cd}) WHERE true{excl} {oc}",
            l = h.local_ref, c = h.collist, conn = conn, cd = coldef, excl = excl_t, ov = overriding, oc = conflict
        )).unwrap();
        if pg_sys::SPI_execute(ins.as_ptr(), false, 0) == pg_sys::SPI_OK_INSERT as i32 {
            total += pg_sys::SPI_processed as i64;
        }
        let d = CString::new(format!("SELECT dblink_disconnect('{}')", conn)).unwrap();
        pg_sys::SPI_execute(d.as_ptr(), false, 0);
    }
    Some(total)
}

/// Read the current `session_replication_role` (origin | replica | local). Caller
/// holds an open SPI connection.
unsafe fn spi_repl_role() -> String {
    let q = CString::new("SELECT current_setting('session_replication_role')").unwrap();
    if pg_sys::SPI_execute(q.as_ptr(), true, 1) == pg_sys::SPI_OK_SELECT as i32
        && pg_sys::SPI_processed == 1
    {
        let tt = pg_sys::SPI_tuptable;
        let row = *(*tt).vals;
        spi_text(pg_sys::SPI_getvalue(row, (*tt).tupdesc, 1)).unwrap_or_else(|| "origin".into())
    } else {
        "origin".into()
    }
}

/// Set `session_replication_role`. Only the three legal enum values are accepted;
/// anything else falls back to 'origin' (defensive). Caller holds an open SPI connection.
unsafe fn spi_set_repl_role(v: &str) {
    let val = match v {
        "replica" | "local" | "origin" => v,
        _ => "origin",
    };
    let q = CString::new(format!("SET session_replication_role = {}", val)).unwrap();
    pg_sys::SPI_execute(q.as_ptr(), false, 0);
}

/// Fetch a hydration into the local table. Returns true when the slice/table is
/// COMPLETE (safe to serve local); returns false ONLY for a PARTIAL pull that
/// overflowed its cap (too many matches -> not selective -> caller must federate,
/// the local rows are an incomplete subset and are never claimed complete).
pub(crate) unsafe fn do_hydrate(h: &Hydration) -> bool {
    gfs_throttle(); // rate-limit source contact
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        // Couldn't hydrate. A capped pull (partial / time-range) would be incomplete
        // -> federate (false). A whole/int-range fetch never claims completeness on
        // failure -> safe (true).
        return h.where_sql.is_empty() && !h.time_key;
    }

    // Copy-on-read replays the SOURCE's already-computed rows, so a replayed INSERT
    // trigger must NOT fire on them again -- it would re-mutate the row (e.g. append or
    // re-stamp) or re-run side effects (audit/cascade), diverging the clone from the
    // source. Apply the rows in the 'replica' role -- exactly how logical replication
    // loads data, skipping user triggers -- then restore the prior role before every
    // return so the caller's OWN writes in the same transaction still fire their
    // triggers normally.
    let prior_srr = spi_repl_role();
    spi_set_repl_role("replica");

    // Inheritance parent (the schema replay mirrors the source's INHERITS
    // hierarchy): every fetch below must read ONLY the parent's own rows -- the
    // children hydrate themselves through their own registrations, so a fetch that
    // included them would land child rows in the parent's heap and the clone's
    // inheritance scan would serve every child row TWICE.
    let only = local_has_children(h.relid);

    // Dedup clause for every hydration insert below (explicit arbiter when the
    // table has a DEFERRABLE unique/exclusion constraint -- see conflict_clause).
    let conflict = conflict_clause(h);

    // Exclude copy-on-write DELETE tombstones so hydration never resurrects a local
    // DELETE -- only when this table has tombstones (the no-deletes case stays
    // zero-overhead). `src` aliases the source so `to_jsonb(src)` builds the row.
    let excl = {
        let q = CString::new(format!(
            "SELECT EXISTS(SELECT 1 FROM gfs.tombstone WHERE relid::oid = {})::int::text",
            u32::from(h.relid)
        ))
        .unwrap();
        let mut has = false;
        if pg_sys::SPI_execute(q.as_ptr(), true, 1) == pg_sys::SPI_OK_SELECT as i32
            && pg_sys::SPI_processed == 1
        {
            let tt = pg_sys::SPI_tuptable;
            let row = *(*tt).vals;
            let td = (*tt).tupdesc;
            has = spi_text(pg_sys::SPI_getvalue(row, td, 1)).as_deref() == Some("1");
        }
        if has {
            format!(
                " AND NOT EXISTS (SELECT 1 FROM gfs.tombstone tb WHERE tb.relid::oid = {} AND to_jsonb(src) @> tb.pk)",
                u32::from(h.relid)
            )
        } else {
            String::new()
        }
    };

    // A GENERATED ALWAYS AS IDENTITY column rejects an explicit value on a plain
    // INSERT; every materialization INSERT below carries this clause (when present)
    // so the source's own key value is copied faithfully, not regenerated locally.
    let overriding = overriding_clause(&h.local_ref);

    // PARTIAL: pull the matching slice with a HARD cap and self-validate against
    // REALITY (not an estimate). One source contact. `matched` (LIMIT cap+1) tells
    // us whether the source had MORE than the cap of matching rows: if so the slice
    // is not actually selective -> mark it overflowed (never partial again) and the
    // caller federates this query; the <=cap+1 rows already inserted are a genuine
    // subset (no completeness is claimed for them), so they are harmless.
    if !h.where_sql.is_empty() {
        let cap = h.partial_cap.max(0);
        let src = src_from(h, only, &h.where_sql, cap + 1);
        let sql = format!(
            "WITH picked AS (SELECT {c} FROM {s} WHERE {w}{excl} LIMIT {lim}), \
                  ins AS (INSERT INTO {l} ({c}) {ov}SELECT {c} FROM picked {oc} RETURNING 1) \
             SELECT (SELECT count(*) FROM picked)::int8::text, (SELECT count(*) FROM ins)::int8::text",
            c = h.collist, s = src, w = h.where_sql, excl = excl, l = h.local_ref, lim = cap + 1, ov = overriding, oc = conflict
        );
        let q = CString::new(sql).unwrap();
        let (mut matched, mut inserted) = (0i64, 0i64);
        if pg_sys::SPI_execute(q.as_ptr(), false, 0) == pg_sys::SPI_OK_SELECT as i32
            && pg_sys::SPI_processed == 1
        {
            let tt = pg_sys::SPI_tuptable;
            let row = *(*tt).vals;
            let td = (*tt).tupdesc;
            matched = spi_text(pg_sys::SPI_getvalue(row, td, 1))
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            inserted = spi_text(pg_sys::SPI_getvalue(row, td, 2))
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
        }
        let overflow = matched > cap; // strictly more than the cap matched -> not selective
        let p = h.pred_key.replace('\'', "''");
        let rec = if overflow {
            format!(
                "INSERT INTO gfs.cached_predicate(relid, pred, overflowed) VALUES ({r}::oid::regclass, '{p}', true) \
                 ON CONFLICT (relid, pred) DO UPDATE SET overflowed = true",
                r = u32::from(h.relid), p = p
            )
        } else {
            format!(
                "INSERT INTO gfs.cached_predicate(relid, pred, complete) VALUES ({r}::oid::regclass, '{p}', true) \
                 ON CONFLICT (relid, pred) DO UPDATE SET complete = true",
                r = u32::from(h.relid), p = p
            )
        };
        pg_sys::SPI_execute(CString::new(rec).unwrap().as_ptr(), false, 0);
        // #131: a partial fetch is a copy event too -- overflow included: its
        // <=cap+1 inserted rows stay in the heap under ON CONFLICT DO NOTHING,
        // so they date the table exactly like a completed slice does.
        let wm = CString::new(format!("SELECT gfs.note_copy({}::oid::regclass)", u32::from(h.relid))).unwrap();
        pg_sys::SPI_execute(wm.as_ptr(), false, 0);
        if !overflow && !DEFER_BOOKKEEPING {
            let pr = CString::new(format!(
                "UPDATE gfs.clone_source SET partial_rows = partial_rows + {} WHERE relid::oid = {}",
                inserted, u32::from(h.relid)
            ))
            .unwrap();
            pg_sys::SPI_execute(pr.as_ptr(), false, 0);
        }
        hydrate_finish(h, inserted);
        spi_set_repl_role(&prior_srr);
        pg_sys::SPI_finish();
        return !overflow;
    }

    // TIME-RANGE: a date/timestamp key bound mapped to epoch micros. We can't size
    // micros in rows, so fetch a CAPPED slice of the temporal window and self-
    // validate: if it overflows the cap the window is too big -> federate (no
    // coverage recorded); else record the [lo,hi] range (coalesced) for elision.
    if h.time_key {
        let cap = h.partial_cap.max(0);
        let mut conds: Vec<String> = Vec::new();
        if h.lo != TIME_FAR_PAST {
            conds.push(format!("{} >= {}", h.key_col, time_recon(h.lo, &h.key_type)));
        }
        if h.hi != TIME_FAR_FUTURE {
            conds.push(format!("{} <= {}", h.key_col, time_recon(h.hi, &h.key_type)));
        }
        let where_clause = if conds.is_empty() { "true".to_string() } else { conds.join(" AND ") };
        let src = src_from(h, only, &where_clause, cap + 1);
        let sql = format!(
            "WITH picked AS (SELECT {c} FROM {s} WHERE {w}{excl} LIMIT {lim}), \
                  ins AS (INSERT INTO {l} ({c}) {ov}SELECT {c} FROM picked {oc} RETURNING 1) \
             SELECT (SELECT count(*) FROM picked)::int8::text, (SELECT count(*) FROM ins)::int8::text",
            c = h.collist, s = src, w = where_clause, excl = excl, l = h.local_ref, lim = cap + 1, ov = overriding, oc = conflict
        );
        let q = CString::new(sql).unwrap();
        let (mut matched, mut inserted) = (0i64, 0i64);
        if pg_sys::SPI_execute(q.as_ptr(), false, 0) == pg_sys::SPI_OK_SELECT as i32
            && pg_sys::SPI_processed == 1
        {
            let tt = pg_sys::SPI_tuptable;
            let row = *(*tt).vals;
            let td = (*tt).tupdesc;
            matched = spi_text(pg_sys::SPI_getvalue(row, td, 1)).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            inserted = spi_text(pg_sys::SPI_getvalue(row, td, 2)).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        }
        let overflow = matched > cap;
        if !overflow {
            let nr = CString::new(format!("SELECT gfs.note_range({}::oid::regclass, {}, {})", u32::from(h.relid), h.lo, h.hi)).unwrap();
            pg_sys::SPI_execute(nr.as_ptr(), false, 0);
        } else {
            // #131: the overflowed slice still inserted up to cap+1 real rows
            // (kept forever under ON CONFLICT DO NOTHING) -- a copy event, even
            // though no coverage is claimed. note_range stamps the other branch.
            let wm = CString::new(format!("SELECT gfs.note_copy({}::oid::regclass)", u32::from(h.relid))).unwrap();
            pg_sys::SPI_execute(wm.as_ptr(), false, 0);
        }
        hydrate_finish(h, inserted);
        spi_set_repl_role(&prior_srr);
        pg_sys::SPI_finish();
        return !overflow;
    }

    // WHOLE / RANGE. Try a parallel fan over the source first (CTID-block / key-range
    // split via concurrent dblink scans); fall back to one FDW statement on any
    // ineligibility or setup failure. ON CONFLICT DO NOTHING keeps both paths
    // idempotent, so a fallback after a partial fan is safe.
    if let Some(n) = try_parallel_backfill(h, !excl.is_empty(), overriding, only, &conflict) {
        record_whole_or_range(h, n);
        spi_set_repl_role(&prior_srr);
        pg_sys::SPI_finish();
        return true;
    }
    let sql = if h.whole {
        format!(
            "INSERT INTO {l} ({c}) {ov}SELECT {c} FROM {s} WHERE true{excl} {oc}",
            l = h.local_ref, c = h.collist, s = src_from(h, only, "", 0), excl = excl, ov = overriding, oc = conflict
        )
    } else {
        let range_w = format!("{} BETWEEN {} AND {}", h.key_col, h.lo, h.hi);
        format!(
            "INSERT INTO {l} ({c}) {ov}SELECT {c} FROM {s} WHERE {w}{excl} {oc}",
            l = h.local_ref, c = h.collist, s = src_from(h, only, &range_w, 0), w = range_w, excl = excl, ov = overriding, oc = conflict
        )
    };
    let q = CString::new(sql).unwrap();
    let rc = pg_sys::SPI_execute(q.as_ptr(), false, 0);
    let n = if rc == pg_sys::SPI_OK_INSERT as i32 { pg_sys::SPI_processed as i64 } else { 0 };
    record_whole_or_range(h, n);
    spi_set_repl_role(&prior_srr);
    pg_sys::SPI_finish();
    true
}

/// Post-fetch: refresh planner stats (so fresh rows use indexes) + bump activity.
/// Caller holds an open SPI connection.
unsafe fn hydrate_finish(h: &Hydration, n: i64) {
    let an = CString::new(format!("ANALYZE {}", h.local_ref)).unwrap();
    pg_sys::SPI_execute(an.as_ptr(), false, 0);
    if DEFER_BOOKKEEPING {
        return; // async worker: skip the contended clone_stats write (see DEFER_BOOKKEEPING)
    }
    let stat = CString::new(format!(
        "UPDATE gfs.clone_stats SET fetch_calls = fetch_calls + 1, \
         rows_fetched = rows_fetched + {}, last_fetch = now() WHERE relid::oid = {}",
        n,
        u32::from(h.relid)
    ))
    .unwrap();
    pg_sys::SPI_execute(stat.as_ptr(), false, 0);
}
