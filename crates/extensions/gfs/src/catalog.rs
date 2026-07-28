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

/// True iff this clone table has DIVERGED from the source via any local write:
/// an INSERT/UPDATE (flagged in `gfs.clone_source.has_local_writes`) or a DELETE
/// (recorded in `gfs.tombstone`). A federated swap runs the query at the SOURCE over
/// the foreign tables, which cannot see local writes, so a federated result would
/// MISS a local insert, return the STALE value of a local update, or RESURRECT a
/// local delete. The router therefore must NOT federate a diverged table; it falls
/// back to a whole-own hydration (do_hydrate copies the source with ON CONFLICT DO
/// NOTHING + tombstone exclusion, which preserves the local insert/update/delete) and
/// serves local. Zero-cost lookup; an unmodified table (the norm) returns false.
pub(crate) unsafe fn relation_diverged(relid: pg_sys::Oid) -> bool {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return false;
    }
    let q = CString::new(format!(
        "SELECT (COALESCE((SELECT has_local_writes FROM gfs.clone_source WHERE relid::oid = {0}), false) \
                 OR EXISTS(SELECT 1 FROM gfs.tombstone WHERE relid::oid = {0}))::int::text",
        u32::from(relid)
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
    pg_sys::SPI_finish();
    has
}

/// Flag a clone table as locally written (diverged) so future queries do not federate
/// it. Called from the router when it plans a top-level INSERT/UPDATE/DELETE whose
/// target is a registered clone. A no-op (0 rows) for unregistered relations; runs in
/// the writing transaction, so a rolled-back write does not leave the flag set.
pub(crate) unsafe fn gfs_mark_local_write(relid: pg_sys::Oid) {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return;
    }
    let q = CString::new(format!(
        "UPDATE gfs.clone_source SET has_local_writes = true \
           WHERE relid::oid = {} AND NOT has_local_writes",
        u32::from(relid)
    ))
    .unwrap();
    pg_sys::SPI_execute(q.as_ptr(), false, 0);
    pg_sys::SPI_finish();
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
                            WHERE a.attrelid = s.relid AND a.attname = s.key_col), ''), \
                COALESCE((SELECT d.drifted FROM gfs.drift_state d WHERE d.relid = s.relid), false)::int::text, \
                COALESCE((SELECT d.schema_drifted FROM gfs.drift_state d WHERE d.relid = s.relid), false)::int::text \
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
                // SOURCE DRIFT: does the source no longer match our local copy?
                // Read from a LOCAL table (gfs.drift_state) so the planner hook
                // never does network I/O. false => behaviour identical to before.
                drifted: g(22).as_deref() == Some("1"),
                // The source's SHAPE changed. Distinct from `drifted` (rows): a
                // stale definition makes federated reads fail outright rather
                // than merely return old data.
                schema_drifted: g(23).as_deref() == Some("1"),
            });
        }
    }
    pg_sys::SPI_finish();
    out
}

/// (autopull_on, verdict_is_stale_dated, autoschema_on) for this table, in ONE
/// local query.
/// The planner hook calls this on a scan, so it must never touch the network:
/// both answers come from local catalog tables.
pub(crate) unsafe fn gfs_sync_flags(relid: pg_sys::Oid) -> (bool, bool, bool) {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return (false, false, false);
    }
    let q = CString::new(format!(
        "SELECT p.autopull::int::text, \
                COALESCE((SELECT (d.checked_at < clock_timestamp() - p.check_interval) \
                            FROM gfs.drift_state d WHERE d.relid::oid = {}), true)::int::text, \
                p.autoschema::int::text \
           FROM gfs.sync_policy p LIMIT 1",
        u32::from(relid)
    ))
    .unwrap();
    let mut out = (false, false, false);
    if pg_sys::SPI_execute(q.as_ptr(), true, 1) == pg_sys::SPI_OK_SELECT as i32
        && pg_sys::SPI_processed == 1
    {
        let tt = pg_sys::SPI_tuptable;
        let row = *(*tt).vals;
        let td = (*tt).tupdesc;
        out = (
            spi_text(pg_sys::SPI_getvalue(row, td, 1)).as_deref() == Some("1"),
            spi_text(pg_sys::SPI_getvalue(row, td, 2)).as_deref() == Some("1"),
            spi_text(pg_sys::SPI_getvalue(row, td, 3)).as_deref() == Some("1"),
        );
    }
    pg_sys::SPI_finish();
    out
}

/// Run one unit of async upkeep for the background worker. `resync` puts a single
/// table back on the lazy path (truncate + clear cached state + re-anchor just that
/// table); `driftcheck` recomputes the stale verdicts. Both are plain SQL so the
/// logic lives next to the catalog it mutates.
pub(crate) unsafe fn gfs_run_upkeep(relid: pg_sys::Oid, kind: &str) {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return;
    }
    let sql = match kind {
        "resync" => format!("SELECT gfs.resync_table({}::oid::regclass)", u32::from(relid)),
        "schemafix" => format!("SELECT gfs.repair_schema({}::oid::regclass)", u32::from(relid)),
        _ => "SELECT gfs.refresh_drift_state()".to_string(),
    };
    if let Ok(q) = CString::new(sql) {
        pg_sys::SPI_execute(q.as_ptr(), false, 0);
    }
    pg_sys::SPI_finish();
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

/// True when `relid` is a plain-table inheritance parent (pg_inherits, relkind 'r').
/// The clone's schema replay mirrors the source's INHERITS hierarchy, so this equals
/// "the source table has INHERITS children". Own SPI connection (planner-hook safe).
pub(crate) unsafe fn gfs_has_children(relid: pg_sys::Oid) -> bool {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return false;
    }
    let q = CString::new(format!(
        "SELECT EXISTS(SELECT 1 FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhparent \
          WHERE i.inhparent = {} AND c.relkind = 'r')::int::text",
        u32::from(relid)
    ))
    .unwrap();
    let mut has = false;
    if pg_sys::SPI_execute(q.as_ptr(), true, 1) == pg_sys::SPI_OK_SELECT as i32
        && pg_sys::SPI_processed == 1
    {
        let tt = pg_sys::SPI_tuptable;
        let row = *(*tt).vals;
        has = spi_text(pg_sys::SPI_getvalue(row, (*tt).tupdesc, 1)).as_deref() == Some("1");
    }
    pg_sys::SPI_finish();
    has
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
/// overflowed, queued)). complete=true -> matching rows fully hydrated (serve
/// local); overflowed=true -> a prior capped pull found too many matches (not
/// selective -> federate, never partial again); queued=true -> an async partial
/// copy is pending in the background (federate meanwhile); (false,false,false) ->
/// a "seen once" second-chance marker (the next identical touch may partial-hydrate).
pub(crate) unsafe fn gfs_pred_state(relid: pg_sys::Oid, pred: &str) -> Option<(bool, bool, bool)> {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return None;
    }
    let q = CString::new(format!(
        "SELECT complete::int::text, overflowed::int::text, queued::int::text \
           FROM gfs.cached_predicate WHERE relid::oid = {} AND pred = '{}'",
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
        let q_ = spi_text(pg_sys::SPI_getvalue(row, td, 3)).as_deref() == Some("1");
        out = Some((c, o, q_));
    }
    pg_sys::SPI_finish();
    out
}

/// Mark a (seen) predicate as QUEUED for an asynchronous partial copy. Idempotent:
/// re-enqueueing a queued predicate is a no-op. The background worker will perform
/// the capped, self-validating hydration and flip it to complete/overflowed.
pub(crate) unsafe fn gfs_enqueue_partial(relid: pg_sys::Oid, pred: &str) {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return;
    }
    let q = CString::new(format!(
        "INSERT INTO gfs.cached_predicate(relid, pred, queued) VALUES ({}::oid::regclass, '{}', true) \
         ON CONFLICT (relid, pred) DO UPDATE SET queued = true \
           WHERE NOT gfs.cached_predicate.complete AND NOT gfs.cached_predicate.overflowed",
        u32::from(relid),
        pred.replace('\'', "''")
    ))
    .unwrap();
    pg_sys::SPI_execute(q.as_ptr(), false, 0);
    pg_sys::SPI_finish();
}

/// Pick ONE pending async partial copy for the background worker. A plain snapshot
/// read with NO row lock: the worker's single-drainer advisory lock already
/// guarantees no other drainer races for the same job, so a `FOR UPDATE` here would
/// only add a long-held lock (the copy takes seconds) that deadlocks with regular
/// query backends touching gfs.clone_source / gfs.cached_predicate in the opposite
/// order. The job stays `queued` until the copy commits and clears it, so an
/// aborted attempt is simply re-picked next poll. Returns (relid, predicate), or
/// None if the queue is empty.
pub(crate) unsafe fn gfs_claim_copy() -> Option<(pg_sys::Oid, String)> {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return None;
    }
    let q = CString::new(
        "SELECT relid::oid::int8::text, pred FROM gfs.cached_predicate \
           WHERE queued AND NOT complete AND NOT overflowed LIMIT 1",
    )
    .unwrap();
    let mut out = None;
    if pg_sys::SPI_execute(q.as_ptr(), true, 1) == pg_sys::SPI_OK_SELECT as i32
        && pg_sys::SPI_processed == 1
    {
        let tt = pg_sys::SPI_tuptable;
        let row = *(*tt).vals;
        let td = (*tt).tupdesc;
        let oid = spi_text(pg_sys::SPI_getvalue(row, td, 1))
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|v| *v != 0)
            .map(pg_sys::Oid::from);
        let pred = spi_text(pg_sys::SPI_getvalue(row, td, 2));
        if let (Some(o), Some(p)) = (oid, pred) {
            out = Some((o, p));
        }
    }
    pg_sys::SPI_finish();
    out
}

/// Clear the queued flag for a predicate after its async copy has run (the copy
/// itself set complete or overflowed). Runs in the worker's job transaction.
pub(crate) unsafe fn gfs_clear_queued(relid: pg_sys::Oid, pred: &str) {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return;
    }
    let q = CString::new(format!(
        "UPDATE gfs.cached_predicate SET queued = false \
           WHERE relid::oid = {} AND pred = '{}'",
        u32::from(relid),
        pred.replace('\'', "''")
    ))
    .unwrap();
    pg_sys::SPI_execute(q.as_ptr(), false, 0);
    pg_sys::SPI_finish();
}

// ---------------------------------------------------------------------------
// gfs.copy_queue: typed async copies (kind='whole' | 'time') for the background
// worker. The predicate partial stays on gfs.cached_predicate.queued (above),
// untouched. enqueue/pending-checks are used by the router (Phase B/C); claim/clear
// are used by the worker.
// ---------------------------------------------------------------------------

/// Enqueue a typed async copy job. Idempotent on (relid, kind, lo, hi).
#[allow(dead_code)]
pub(crate) unsafe fn gfs_enqueue_copy(relid: pg_sys::Oid, kind: &str, lo: i64, hi: i64) {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return;
    }
    let q = CString::new(format!(
        "INSERT INTO gfs.copy_queue(relid, kind, lo, hi) \
         VALUES ({}::oid::regclass, '{}', {}, {}) ON CONFLICT DO NOTHING",
        u32::from(relid),
        kind.replace('\'', "''"),
        lo,
        hi
    ))
    .unwrap();
    pg_sys::SPI_execute(q.as_ptr(), false, 0);
    pg_sys::SPI_finish();
}

#[allow(dead_code)]
unsafe fn copy_queue_exists(sql: &str) -> bool {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return false;
    }
    let q = CString::new(sql).unwrap();
    let mut yes = false;
    if pg_sys::SPI_execute(q.as_ptr(), true, 1) == pg_sys::SPI_OK_SELECT as i32
        && pg_sys::SPI_processed == 1
    {
        let tt = pg_sys::SPI_tuptable;
        let row = *(*tt).vals;
        let td = (*tt).tupdesc;
        yes = spi_text(pg_sys::SPI_getvalue(row, td, 1)).as_deref() == Some("1");
    }
    pg_sys::SPI_finish();
    yes
}

/// True if a whole-table async copy is already queued (router federates, no dup).
#[allow(dead_code)]
pub(crate) unsafe fn gfs_whole_queued(relid: pg_sys::Oid) -> bool {
    copy_queue_exists(&format!(
        "SELECT EXISTS(SELECT 1 FROM gfs.copy_queue WHERE relid::oid = {} AND kind = 'whole')::int::text",
        u32::from(relid)
    ))
}

/// True if a queued temporal copy already COVERS [lo,hi] (router federates, no dup).
#[allow(dead_code)]
pub(crate) unsafe fn gfs_time_queued(relid: pg_sys::Oid, lo: i64, hi: i64) -> bool {
    copy_queue_exists(&format!(
        "SELECT EXISTS(SELECT 1 FROM gfs.copy_queue WHERE relid::oid = {} AND kind = 'time' \
           AND lo <= {} AND hi >= {})::int::text",
        u32::from(relid),
        lo,
        hi
    ))
}

/// Pick ONE queued async copy job for the worker (plain snapshot read; the worker's
/// single-drainer advisory lock means no row lock is needed -- same rationale as
/// gfs_claim_copy). Returns (relid, kind, lo, hi), or None if the queue is empty.
pub(crate) unsafe fn gfs_claim_copy_job() -> Option<(pg_sys::Oid, String, i64, i64)> {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return None;
    }
    let q = CString::new(
        "SELECT relid::oid::int8::text, kind, lo::text, hi::text FROM gfs.copy_queue \
           ORDER BY enqueued_at LIMIT 1",
    )
    .unwrap();
    let mut out = None;
    if pg_sys::SPI_execute(q.as_ptr(), true, 1) == pg_sys::SPI_OK_SELECT as i32
        && pg_sys::SPI_processed == 1
    {
        let tt = pg_sys::SPI_tuptable;
        let row = *(*tt).vals;
        let td = (*tt).tupdesc;
        let g = |i| spi_text(pg_sys::SPI_getvalue(row, td, i));
        let oid = g(1)
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|v| *v != 0)
            .map(pg_sys::Oid::from);
        let kind = g(2);
        let lo = g(3).and_then(|s| s.trim().parse::<i64>().ok());
        let hi = g(4).and_then(|s| s.trim().parse::<i64>().ok());
        if let (Some(o), Some(k), Some(l), Some(hh)) = (oid, kind, lo, hi) {
            out = Some((o, k, l, hh));
        }
    }
    pg_sys::SPI_finish();
    out
}

/// Remove a finished/aborted async copy job. Runs in the worker's job transaction.
pub(crate) unsafe fn gfs_clear_copy_job(relid: pg_sys::Oid, kind: &str, lo: i64, hi: i64) {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return;
    }
    let q = CString::new(format!(
        "DELETE FROM gfs.copy_queue WHERE relid::oid = {} AND kind = '{}' AND lo = {} AND hi = {}",
        u32::from(relid),
        kind.replace('\'', "''"),
        lo,
        hi
    ))
    .unwrap();
    pg_sys::SPI_execute(q.as_ptr(), false, 0);
    pg_sys::SPI_finish();
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
