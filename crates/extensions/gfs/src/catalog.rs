//! SPI: catalog lookups + small mutators the router and federation rely on, plus
//! the prod-protection throttle gate.

use core::ffi::c_char;
use std::ffi::{CStr, CString};

use pgrx::pg_sys;

use crate::model::CloneInfo;

pub(crate) unsafe fn spi_text(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        None
    } else {
        Some(CStr::from_ptr(p).to_string_lossy().into_owned())
    }
}

pub(crate) unsafe fn gfs_lookup_clone(relid: pg_sys::Oid) -> Option<CloneInfo> {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return None;
    }
    let q = CString::new(format!(
        "SELECT s.relid::regclass::text, s.source_ref, \
                COALESCE((SELECT string_agg(quote_ident(attname), ', ' ORDER BY attnum) \
                            FROM pg_attribute WHERE attrelid = s.relid AND attnum > 0 \
                              AND NOT attisdropped AND attgenerated = ''), ''), \
                s.chunk_kind, s.whole_cached::int::text, s.key_col, \
                COALESCE((SELECT attnum FROM pg_attribute WHERE attrelid = s.relid \
                            AND attname = s.key_col), 0)::text, \
                s.source_rows::text, s.row_bytes::text, s.access_count::text, \
                x.net::text, x.source::text, x.negligible::text, x.ceiling::text, x.horizon::text, \
                s.partial_rows::text, s.no_partial::int::text, \
                x.partial_max_frac::text, x.promote_frac::text, x.max_partial_preds::text, \
                COALESCE((SELECT t.typname FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
                            WHERE a.attrelid = s.relid AND a.attname = s.key_col), '') \
           FROM gfs.clone_source s, gfs.cost x \
          WHERE s.relid::oid = {} AND to_regclass(s.source_ref) IS NOT NULL",
        u32::from(relid)
    ))
    .unwrap();
    let mut out = None;
    if pg_sys::SPI_execute(q.as_ptr(), true, 1) == pg_sys::SPI_OK_SELECT as i32
        && pg_sys::SPI_processed == 1
    {
        let tt = pg_sys::SPI_tuptable;
        let row = *(*tt).vals;
        let td = (*tt).tupdesc;
        let g = |i| spi_text(pg_sys::SPI_getvalue(row, td, i));
        let num = |i| g(i).and_then(|s| s.trim().parse::<f64>().ok()).unwrap_or(0.0);
        if let (Some(l), Some(s), Some(c), Some(k), Some(w), Some(kc), Some(at)) =
            (g(1), g(2), g(3), g(4), g(5), g(6), g(7))
        {
            out = Some(CloneInfo {
                local_ref: l,
                source_ref: s,
                collist: c,
                chunk_kind: k,
                whole_cached: w == "1",
                key_col: kc,
                key_type: g(21).unwrap_or_default(),
                key_attno: at.trim().parse::<i16>().unwrap_or(0),
                source_rows: num(8) as i64,
                row_bytes: num(9) as i64,
                access_count: num(10) as i64,
                w_net: num(11),
                w_source: num(12),
                w_negligible: num(13),
                w_ceiling: num(14),
                w_horizon: num(15),
                partial_rows: num(16) as i64,
                no_partial: g(17).as_deref() == Some("1"),
                w_partial_max_frac: num(18),
                w_promote_frac: num(19),
                w_max_partial_preds: num(20) as i64,
            });
        }
    }
    pg_sys::SPI_finish();
    out
}

/// Increment the per-table access counter (drives the amortization horizon H).
pub(crate) unsafe fn bump_access(relid: pg_sys::Oid) {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return;
    }
    let q = CString::new(format!(
        "UPDATE gfs.clone_source SET access_count = access_count + 1 WHERE relid::oid = {}",
        u32::from(relid)
    ))
    .unwrap();
    pg_sys::SPI_execute(q.as_ptr(), false, 0);
    pg_sys::SPI_finish();
}

pub(crate) unsafe fn gfs_source_oid(relid: pg_sys::Oid) -> Option<pg_sys::Oid> {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return None;
    }
    let q = CString::new(format!(
        "SELECT to_regclass(source_ref)::oid::int8 FROM gfs.clone_source \
         WHERE relid::oid = {} AND to_regclass(source_ref) IS NOT NULL",
        u32::from(relid)
    ))
    .unwrap();
    let mut out = None;
    if pg_sys::SPI_execute(q.as_ptr(), true, 1) == pg_sys::SPI_OK_SELECT as i32
        && pg_sys::SPI_processed == 1
    {
        let tt = pg_sys::SPI_tuptable;
        let row = *(*tt).vals;
        let td = (*tt).tupdesc;
        if let Some(s) = spi_text(pg_sys::SPI_getvalue(row, td, 1)) {
            if let Ok(v) = s.trim().parse::<u32>() {
                if v != 0 {
                    out = Some(pg_sys::Oid::from(v));
                }
            }
        }
    }
    pg_sys::SPI_finish();
    out
}

pub(crate) unsafe fn gfs_is_covered(relid: pg_sys::Oid, lo: i64, hi: i64) -> bool {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return false;
    }
    let q = CString::new(format!(
        "SELECT EXISTS(SELECT 1 FROM gfs.cached \
           WHERE relid::oid = {} AND lo <= {} AND hi >= {})::int::text",
        u32::from(relid),
        lo,
        hi
    ))
    .unwrap();
    let mut covered = false;
    if pg_sys::SPI_execute(q.as_ptr(), true, 1) == pg_sys::SPI_OK_SELECT as i32
        && pg_sys::SPI_processed == 1
    {
        let tt = pg_sys::SPI_tuptable;
        let row = *(*tt).vals;
        let td = (*tt).tupdesc;
        covered = spi_text(pg_sys::SPI_getvalue(row, td, 1)).as_deref() == Some("1");
    }
    pg_sys::SPI_finish();
    covered
}

/// State of a predicate in the catalog: None = never seen; Some((complete,
/// overflowed)). complete=true -> matching rows fully hydrated (serve local);
/// overflowed=true -> a prior capped pull found too many matches (not selective ->
/// federate, never partial again); (false,false) -> a "seen once" second-chance
/// marker (the next identical touch may partial-hydrate).
pub(crate) unsafe fn gfs_pred_state(relid: pg_sys::Oid, pred: &str) -> Option<(bool, bool)> {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return None;
    }
    let q = CString::new(format!(
        "SELECT complete::int::text, overflowed::int::text FROM gfs.cached_predicate \
           WHERE relid::oid = {} AND pred = '{}'",
        u32::from(relid),
        pred.replace('\'', "''")
    ))
    .unwrap();
    let mut out = None;
    if pg_sys::SPI_execute(q.as_ptr(), true, 1) == pg_sys::SPI_OK_SELECT as i32
        && pg_sys::SPI_processed == 1
    {
        let tt = pg_sys::SPI_tuptable;
        let row = *(*tt).vals;
        let td = (*tt).tupdesc;
        let c = spi_text(pg_sys::SPI_getvalue(row, td, 1)).as_deref() == Some("1");
        let o = spi_text(pg_sys::SPI_getvalue(row, td, 2)).as_deref() == Some("1");
        out = Some((c, o));
    }
    pg_sys::SPI_finish();
    out
}

/// Record a predicate as SEEN (second-chance marker, complete=false) without
/// contacting the source -- so its NEXT identical touch is eligible for partial.
pub(crate) unsafe fn gfs_note_pred_seen(relid: pg_sys::Oid, pred: &str) {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return;
    }
    let q = CString::new(format!(
        "INSERT INTO gfs.cached_predicate(relid, pred) VALUES ({}::oid::regclass, '{}') \
         ON CONFLICT DO NOTHING",
        u32::from(relid),
        pred.replace('\'', "''")
    ))
    .unwrap();
    pg_sys::SPI_execute(q.as_ptr(), false, 0);
    pg_sys::SPI_finish();
}

/// Count of fully-hydrated (complete) partial predicates for this table -- the
/// CONTACT-cap input for the promote guard.
pub(crate) unsafe fn gfs_pred_count(relid: pg_sys::Oid) -> i64 {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return 0;
    }
    let q = CString::new(format!(
        "SELECT count(*)::int8::text FROM gfs.cached_predicate \
           WHERE relid::oid = {} AND complete",
        u32::from(relid)
    ))
    .unwrap();
    let mut n = 0i64;
    if pg_sys::SPI_execute(q.as_ptr(), true, 1) == pg_sys::SPI_OK_SELECT as i32
        && pg_sys::SPI_processed == 1
    {
        let tt = pg_sys::SPI_tuptable;
        let row = *(*tt).vals;
        let td = (*tt).tupdesc;
        n = spi_text(pg_sys::SPI_getvalue(row, td, 1))
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0);
    }
    pg_sys::SPI_finish();
    n
}

/// Mark a table terminally un-ownable: federate every call, stop probing/partialing.
pub(crate) unsafe fn gfs_set_no_partial(relid: pg_sys::Oid) {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return;
    }
    let q = CString::new(format!(
        "UPDATE gfs.clone_source SET no_partial = true WHERE relid::oid = {}",
        u32::from(relid)
    ))
    .unwrap();
    pg_sys::SPI_execute(q.as_ptr(), false, 0);
    pg_sys::SPI_finish();
}

/// Count a query that was pushed to the source for this clone table (so the demo
/// can label it "federated" rather than "local" — it returned 0 hydrated rows but
/// did contact the source).
pub(crate) unsafe fn bump_federate(relid: pg_sys::Oid) {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return;
    }
    let q = CString::new(format!(
        "UPDATE gfs.clone_stats SET federate_calls = federate_calls + 1 WHERE relid::oid = {}",
        u32::from(relid)
    ))
    .unwrap();
    pg_sys::SPI_execute(q.as_ptr(), false, 0);
    pg_sys::SPI_finish();
}

/// Prod-protection gate: before contacting the source, consume a rate-limit token;
/// if the per-clone budget is exhausted, wait (back-pressure, bounded) so 100s of
/// clones can't overwhelm the prod source. No-op when max_rate = 0 (unlimited).
pub(crate) unsafe fn gfs_throttle() {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return;
    }
    let q = CString::new("SELECT gfs.take_token()").unwrap();
    let mut wait = 0.0f64;
    if pg_sys::SPI_execute(q.as_ptr(), false, 1) == pg_sys::SPI_OK_SELECT as i32
        && pg_sys::SPI_processed == 1
    {
        let tt = pg_sys::SPI_tuptable;
        let row = *(*tt).vals;
        let td = (*tt).tupdesc;
        if let Some(s) = spi_text(pg_sys::SPI_getvalue(row, td, 1)) {
            wait = s.trim().parse::<f64>().unwrap_or(0.0);
        }
    }
    pg_sys::SPI_finish();
    if wait > 0.0 {
        // bounded per call so we never block a backend pathologically long
        let us = (wait.min(1.0) * 1_000_000.0) as core::ffi::c_long;
        pg_sys::pg_usleep(us);
    }
}
