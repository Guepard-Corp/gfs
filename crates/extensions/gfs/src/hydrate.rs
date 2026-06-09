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
unsafe fn try_parallel_backfill(h: &Hydration, has_tomb: bool) -> Option<i64> {
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

    // --- real source-side schema.table behind the foreign table (quoted) ---
    let fq = CString::new(format!(
        "SELECT quote_ident(COALESCE((SELECT option_value FROM pg_options_to_table(ft.ftoptions) WHERE option_name = 'schema_name'), n.nspname)), \
                quote_ident(COALESCE((SELECT option_value FROM pg_options_to_table(ft.ftoptions) WHERE option_name = 'table_name'), c.relname)) \
           FROM pg_foreign_table ft JOIN pg_class c ON c.oid = ft.ftrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
          WHERE ft.ftrelid = '{}'::regclass",
        h.source_ref.replace('\'', "''")
    )).unwrap();
    if pg_sys::SPI_execute(fq.as_ptr(), true, 1) != pg_sys::SPI_OK_SELECT as i32 || pg_sys::SPI_processed != 1 {
        return None;
    }
    let tt = pg_sys::SPI_tuptable;
    let row = *(*tt).vals;
    let td = (*tt).tupdesc;
    let sch = spi_text(pg_sys::SPI_getvalue(row, td, 1))?;
    let tbl = spi_text(pg_sys::SPI_getvalue(row, td, 2))?;
    let src_qual = format!("{}.{}", sch, tbl);

    // --- typed column list for dblink_get_result (same types as the local table) ---
    let cq = CString::new(format!(
        "SELECT string_agg(quote_ident(attname) || ' ' || format_type(atttypid, atttypmod), ', ' ORDER BY attnum) \
           FROM pg_attribute WHERE attrelid = '{}'::regclass AND attnum > 0 AND NOT attisdropped AND attgenerated = ''",
        h.local_ref.replace('\'', "''")
    )).unwrap();
    if pg_sys::SPI_execute(cq.as_ptr(), true, 1) != pg_sys::SPI_OK_SELECT as i32 {
        return None;
    }
    let coldef = spi_cell1()?;
    if coldef.is_empty() {
        return None;
    }

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
            "INSERT INTO {l} ({c}) SELECT {c} FROM dblink_get_result('{conn}') AS t({cd}) WHERE true{excl} ON CONFLICT DO NOTHING",
            l = h.local_ref, c = h.collist, conn = conn, cd = coldef, excl = excl_t
        )).unwrap();
        if pg_sys::SPI_execute(ins.as_ptr(), false, 0) == pg_sys::SPI_OK_INSERT as i32 {
            total += pg_sys::SPI_processed as i64;
        }
        let d = CString::new(format!("SELECT dblink_disconnect('{}')", conn)).unwrap();
        pg_sys::SPI_execute(d.as_ptr(), false, 0);
    }
    Some(total)
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

    // Exclude copy-on-write DELETE tombstones so hydration never resurrects a local
    // DELETE -- only when this table has tombstones (the no-deletes case stays
    // zero-overhead). `src` aliases the source so `to_jsonb(src)` builds the row.
    let src = format!("{} src", h.source_ref);
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

    // PARTIAL: pull the matching slice with a HARD cap and self-validate against
    // REALITY (not an estimate). One source contact. `matched` (LIMIT cap+1) tells
    // us whether the source had MORE than the cap of matching rows: if so the slice
    // is not actually selective -> mark it overflowed (never partial again) and the
    // caller federates this query; the <=cap+1 rows already inserted are a genuine
    // subset (no completeness is claimed for them), so they are harmless.
    if !h.where_sql.is_empty() {
        let cap = h.partial_cap.max(0);
        let sql = format!(
            "WITH picked AS (SELECT {c} FROM {s} WHERE {w}{excl} LIMIT {lim}), \
                  ins AS (INSERT INTO {l} ({c}) SELECT {c} FROM picked ON CONFLICT DO NOTHING RETURNING 1) \
             SELECT (SELECT count(*) FROM picked)::int8::text, (SELECT count(*) FROM ins)::int8::text",
            c = h.collist, s = src, w = h.where_sql, excl = excl, l = h.local_ref, lim = cap + 1
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
        if !overflow {
            let pr = CString::new(format!(
                "UPDATE gfs.clone_source SET partial_rows = partial_rows + {} WHERE relid::oid = {}",
                inserted, u32::from(h.relid)
            ))
            .unwrap();
            pg_sys::SPI_execute(pr.as_ptr(), false, 0);
        }
        hydrate_finish(h, inserted);
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
        let sql = format!(
            "WITH picked AS (SELECT {c} FROM {s} WHERE {w}{excl} LIMIT {lim}), \
                  ins AS (INSERT INTO {l} ({c}) SELECT {c} FROM picked ON CONFLICT DO NOTHING RETURNING 1) \
             SELECT (SELECT count(*) FROM picked)::int8::text, (SELECT count(*) FROM ins)::int8::text",
            c = h.collist, s = src, w = where_clause, excl = excl, l = h.local_ref, lim = cap + 1
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
        }
        hydrate_finish(h, inserted);
        pg_sys::SPI_finish();
        return !overflow;
    }

    // WHOLE / RANGE. Try a parallel fan over the source first (CTID-block / key-range
    // split via concurrent dblink scans); fall back to one FDW statement on any
    // ineligibility or setup failure. ON CONFLICT DO NOTHING keeps both paths
    // idempotent, so a fallback after a partial fan is safe.
    if let Some(n) = try_parallel_backfill(h, !excl.is_empty()) {
        record_whole_or_range(h, n);
        pg_sys::SPI_finish();
        return true;
    }
    let sql = if h.whole {
        format!(
            "INSERT INTO {l} ({c}) SELECT {c} FROM {s} WHERE true{excl} ON CONFLICT DO NOTHING",
            l = h.local_ref, c = h.collist, s = src, excl = excl
        )
    } else {
        format!(
            "INSERT INTO {l} ({c}) SELECT {c} FROM {s} WHERE {k} BETWEEN {lo} AND {hi}{excl} ON CONFLICT DO NOTHING",
            l = h.local_ref, c = h.collist, s = src, k = h.key_col, lo = h.lo, hi = h.hi, excl = excl
        )
    };
    let q = CString::new(sql).unwrap();
    let rc = pg_sys::SPI_execute(q.as_ptr(), false, 0);
    let n = if rc == pg_sys::SPI_OK_INSERT as i32 { pg_sys::SPI_processed as i64 } else { 0 };
    record_whole_or_range(h, n);
    pg_sys::SPI_finish();
    true
}

/// Post-fetch: refresh planner stats (so fresh rows use indexes) + bump activity.
/// Caller holds an open SPI connection.
unsafe fn hydrate_finish(h: &Hydration, n: i64) {
    let an = CString::new(format!("ANALYZE {}", h.local_ref)).unwrap();
    pg_sys::SPI_execute(an.as_ptr(), false, 0);
    let stat = CString::new(format!(
        "UPDATE gfs.clone_stats SET fetch_calls = fetch_calls + 1, \
         rows_fetched = rows_fetched + {}, last_fetch = now() WHERE relid::oid = {}",
        n,
        u32::from(h.relid)
    ))
    .unwrap();
    pg_sys::SPI_execute(stat.as_ptr(), false, 0);
}
