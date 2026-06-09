//! Async PARTIAL copy worker.
//!
//! On a selective non-key predicate the router does NOT block on the (potentially
//! slow) capped hydration any more. Instead it marks the predicate `queued`
//! (`gfs.cached_predicate.queued`), federates the query for an immediate answer,
//! and kicks this worker. The worker performs the same capped, self-validating
//! copy OFF the query's critical path, then flips the predicate to `complete`
//! (or `overflowed`) so future queries serve local. No query ever waits on the copy.
//!
//! The clone is loaded via `session_preload_libraries` (no `shared_preload`, no
//! restart), so a static worker can't be registered in `_PG_init`. We therefore
//! launch a DYNAMIC worker on demand ([`spawn`]). A single drainer at a time is
//! guaranteed by a session-level advisory lock: a redundantly-spawned worker that
//! can't take the lock exits immediately. The active worker drains, then idles for
//! a short grace window (polling) before exiting -- the grace window also covers
//! the case where the enqueuing transaction commits the `queued` flag just after
//! the worker started.

use core::time::Duration;
use std::ffi::CString;

use pgrx::bgworkers::{BackgroundWorker, BackgroundWorkerBuilder, SignalWakeFlags};
use pgrx::pg_sys;
use pgrx::prelude::*;
use pgrx::PgTryBuilder;

use crate::catalog::{gfs_claim_copy, gfs_clear_queued, gfs_lookup_clone, spi_text};
use crate::hydrate::do_hydrate;
use crate::model::Hydration;

/// Cluster-wide advisory-lock key ensuring a single copy drainer at a time.
const GFS_COPY_LOCK_KEY: i64 = 0x6766_7363_6f70_79; // "gfscopy"
/// Idle ticks (1s each) the active worker keeps polling before it exits.
const GRACE_TICKS: u32 = 5;

/// Launch the dynamic copy worker (best-effort). Called from the router when it
/// enqueues an async partial copy. Redundant launches are harmless: the advisory
/// lock makes a second worker exit at once. Failure (e.g. no free worker slot) is
/// ignored -- the job stays queued and a later enqueue re-launches a worker.
pub(crate) fn spawn() {
    match BackgroundWorkerBuilder::new("gfs copy worker")
        .set_library("gfs")
        .set_function("gfs_copy_worker_main")
        .set_restart_time(None) // ephemeral: drains then exits, re-launched on demand
        .enable_spi_access()
        .set_notify_pid(0)
        .load_dynamic()
    {
        Ok(_) => debug1!("gfs: copy worker registered (dynamic)"),
        Err(e) => warning!("gfs: copy worker registration failed: {e:?}"),
    }
}

#[pg_guard]
#[no_mangle]
pub extern "C-unwind" fn gfs_copy_worker_main(_arg: pg_sys::Datum) {
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);
    BackgroundWorker::connect_worker_to_spi(Some("postgres"), None);
    // This process's hydrations skip the contended per-table catalog bookkeeping so
    // the worker shares no lock with foreground queries (no deadlock cycle).
    unsafe { crate::hydrate::DEFER_BOOKKEEPING = true };
    debug1!("gfs: copy worker started");

    // Single-drainer dedup. A worker that can't take the lock exits immediately.
    if !BackgroundWorker::transaction(|| unsafe { try_drain_lock() }) {
        debug1!("gfs: copy worker exiting (another drainer holds the lock)");
        return;
    }

    let mut idle: u32 = 0;
    let mut done: u32 = 0;
    loop {
        if BackgroundWorker::sigterm_received() {
            break;
        }
        // One job per transaction so each finished copy commits (and becomes
        // local-serveable) immediately. A failing job is caught so it can't kill
        // the worker; it stays queued and is retried.
        let did = PgTryBuilder::new(|| BackgroundWorker::transaction(|| unsafe { drain_one() }))
            .catch_others(|e| {
                // The job's transaction errored (e.g. a transient deadlock). pgrx's
                // transaction() skips its commit on the longjmp, leaving the xact
                // open -- abort it here so the worker recovers instead of wedging on
                // "unexpected state STARTED". The job stays `queued` (its writes
                // rolled back) and is retried on the next poll.
                debug1!("gfs: copy job failed, will retry: {e:?}");
                unsafe { pg_sys::AbortCurrentTransaction() };
                false
            })
            .execute();
        if did {
            idle = 0;
            done += 1;
            continue; // drain promptly; more may be queued
        }
        idle += 1;
        if idle > GRACE_TICKS {
            break; // idle long enough -> exit; a later enqueue re-launches a worker
        }
        if !BackgroundWorker::wait_latch(Some(Duration::from_secs(1))) {
            break; // false == SIGTERM
        }
    }
    debug1!("gfs: copy worker exiting; drained {done} job(s), {idle} idle poll(s)");
    // The session-level advisory lock releases automatically when the worker exits.
}

/// Take the session-level advisory lock (non-blocking). Returns true iff acquired
/// (i.e. this is the sole drainer). Held until the worker process exits.
unsafe fn try_drain_lock() -> bool {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return false;
    }
    // Yield locks fast (below the 1s default deadlock_timeout) so contention with a
    // user query times the WORKER out -- the user's query is never the deadlock
    // victim. A timed-out job stays queued and is retried. Session-scoped (commits
    // with this setup transaction, persists for the worker's life).
    let to = CString::new("SET lock_timeout = '750ms'").unwrap();
    pg_sys::SPI_execute(to.as_ptr(), false, 0);
    let q = CString::new(format!(
        "SELECT pg_try_advisory_lock({})::int::text",
        GFS_COPY_LOCK_KEY
    ))
    .unwrap();
    let mut got = false;
    if pg_sys::SPI_execute(q.as_ptr(), false, 1) == pg_sys::SPI_OK_SELECT as i32
        && pg_sys::SPI_processed == 1
    {
        let tt = pg_sys::SPI_tuptable;
        let row = *(*tt).vals;
        let td = (*tt).tupdesc;
        got = spi_text(pg_sys::SPI_getvalue(row, td, 1)).as_deref() == Some("1");
    }
    pg_sys::SPI_finish();
    got
}

/// Claim and run ONE queued partial copy. Returns true if a job was processed,
/// false if the queue is empty. Runs inside the caller's transaction: picking the
/// job, the copy, and clearing `queued` commit together. Dedup against other
/// drainers is the single-drainer advisory lock (not a row lock), so this shares no
/// long-held lock with foreground queries.
unsafe fn drain_one() -> bool {
    let Some((relid, pred)) = gfs_claim_copy() else {
        return false; // queue empty
    };
    debug1!("gfs: claimed copy job: {} pred {}", relid_text(relid), pred);
    if let Some(info) = gfs_lookup_clone(relid) {
        if !info.whole_cached {
            // Same capped slice the synchronous path used: ceil(partial_max_frac * Tr).
            let cap = (info.w_partial_max_frac * info.source_rows.max(0) as f64)
                .floor()
                .max(1.0) as i64;
            let hyd = Hydration {
                local_ref: info.local_ref,
                source_ref: info.source_ref,
                collist: info.collist,
                relid,
                key_col: info.key_col,
                lo: 0,
                hi: 0,
                whole: false,
                where_sql: pred.clone(),
                pred_key: pred.clone(),
                partial_cap: cap,
                time_key: false,
                key_type: info.key_type,
            };
            // The capped, self-validating pull. It records cached_predicate.complete
            // on success, or .overflowed if more than `cap` rows matched (not
            // selective -> federate forever). Throttle is honored inside do_hydrate.
            do_hydrate(&hyd);
            log!("gfs: async copy done for {} pred {}", relid_text(relid), pred);
        }
    }
    gfs_clear_queued(relid, &pred); // leave only complete/overflowed set
    true
}

/// Best-effort relid -> text for log lines.
unsafe fn relid_text(relid: pg_sys::Oid) -> String {
    let n = pg_sys::get_rel_name(relid);
    if n.is_null() {
        format!("oid {}", u32::from(relid))
    } else {
        std::ffi::CStr::from_ptr(n).to_string_lossy().into_owned()
    }
}
