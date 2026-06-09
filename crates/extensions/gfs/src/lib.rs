#![allow(static_mut_refs)]
#![allow(non_snake_case)]
//! gfs — lazy copy-on-read clone of an external PostgreSQL (RFC 008), in Rust/pgrx.
//!
//! The source is reachable only over SQL (a `postgres_fdw` foreign table), so the
//! clone is logical, not physical. Each table is a real local heap table (indexes,
//! ownership, writes) PLUS a foreign table `gfs_remote.T`. A `planner_hook` routes
//! every query (A+B):
//!
//!   • HYDRATE — a query that bounds the table's range key (`id BETWEEN`, `id >`)
//!     fetches the missing key-RANGE into the local table (range granularity),
//!     records it in `gfs.cached`, then runs local (indexes). Re-asking a cached
//!     range hits no source (elision). Small / non-rangeable tables (uuid) hydrate
//!     whole on first touch.
//!   • FEDERATE — a query with no range-key bound on a not-yet-owned table (fuzzy,
//!     non-key join, aggregate) is pushed whole to the source via the foreign
//!     tables; postgres_fdw computes it remotely and returns the result — nothing
//!     is materialized locally.
//!   • Once a table is fully owned (`whole_cached`), it is served locally even for
//!     federate-class queries (no source contact) — the clone converges to a
//!     self-sufficient local copy.
//!
//! Correctness: a scan is served local only when its needed range is covered (or
//! the table is whole_cached); otherwise it federates (reads the source) — never a
//! partial local result.
//!
//! Module map:
//!   • [`route`]    — classify each base scan, decide local / hydrate / federate
//!   • [`keyrange`] — extract [lo,hi] range-key bounds + const/operator decoding
//!   • [`pushdown`] — deparse a scan's pushable restriction into a remote WHERE
//!   • [`federate`] — swap clone RTEs to their foreign tables (postgres_fdw pushdown)
//!   • [`catalog`]  — SPI catalog lookups / mutators + the prod-protection throttle
//!   • [`hydrate`]  — the hydration engine (single-statement + parallel dblink fan)
//!   • [`model`]    — descriptors shared across the above
//!   • `sql/schema.sql` — the catalog + API DDL (loaded via `extension_sql_file!`)

use core::ffi::{c_char, c_int};

use pgrx::pg_sys;
use pgrx::prelude::*;
use pgrx::PgTryBuilder;

::pgrx::pg_module_magic!();

mod catalog;
mod federate;
mod hydrate;
mod keyrange;
mod model;
mod pushdown;
mod route;

// ===========================================================================
// Planner hook
// ===========================================================================
static mut PREV_PLANNER: pg_sys::planner_hook_type = None;
static mut GFS_IN_HOOK: bool = false;

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    unsafe {
        PREV_PLANNER = pg_sys::planner_hook;
        pg_sys::planner_hook = Some(gfs_planner);
    }
}

/// Plan without GFS routing: the previous planner hook if one is installed, else
/// the stock planner. Used by the router to produce the cold plan it inspects and
/// the final re-plan on the hydrated/federated query.
pub(crate) unsafe fn base_plan(
    parse: *mut pg_sys::Query,
    qs: *const c_char,
    cursor: c_int,
    params: pg_sys::ParamListInfo,
) -> *mut pg_sys::PlannedStmt {
    if let Some(prev) = PREV_PLANNER {
        prev(parse, qs, cursor, params)
    } else {
        pg_sys::standard_planner(parse, qs, cursor, params)
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn gfs_planner(
    parse: *mut pg_sys::Query,
    qs: *const c_char,
    cursor: c_int,
    params: pg_sys::ParamListInfo,
) -> *mut pg_sys::PlannedStmt {
    if GFS_IN_HOOK || pg_sys::get_namespace_oid(c"gfs".as_ptr(), true) == pg_sys::InvalidOid {
        return base_plan(parse, qs, cursor, params);
    }
    GFS_IN_HOOK = true;
    let out = PgTryBuilder::new(|| unsafe { route::gfs_route(parse, qs, cursor, params) })
        .finally(|| unsafe { GFS_IN_HOOK = false })
        .execute();
    out
}

// ===========================================================================
// Catalog + API (DDL). The schema/tables/functions/view the planner hook reads
// and the user-facing register/warm/calibrate API. Kept in a sibling sql/ file
// (loaded verbatim) so it stays editable and syntax-highlighted on its own.
// ===========================================================================
extension_sql_file!("sql/schema.sql", name = "gfs_catalog");
