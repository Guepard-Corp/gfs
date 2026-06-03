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

use core::ffi::{c_char, c_int};
use std::ffi::{CStr, CString};

use pgrx::pg_sys;
use pgrx::prelude::*;
use pgrx::{PgList, PgTryBuilder};

::pgrx::pg_module_magic!();

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

unsafe fn base_plan(
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
    let out = PgTryBuilder::new(|| unsafe { gfs_route(parse, qs, cursor, params) })
        .finally(|| unsafe { GFS_IN_HOOK = false })
        .execute();
    out
}

// ===========================================================================
// Router
// ===========================================================================
struct Hydration {
    local_ref: String,
    source_ref: String,
    collist: String,
    relid: pg_sys::Oid,
    key_col: String,
    lo: i64,
    hi: i64,
    whole: bool,
}

struct Ctx {
    hydrations: Vec<Hydration>,      // range/whole fetches for coverable/small tables
    federate_targets: Vec<Hydration>, // tables to push to the source (whole-fallback if swap fails)
}

unsafe fn gfs_route(
    parse: *mut pg_sys::Query,
    qs: *const c_char,
    cursor: c_int,
    params: pg_sys::ParamListInfo,
) -> *mut pg_sys::PlannedStmt {
    let parse_copy = pg_sys::copyObjectImpl(parse as *const _) as *mut pg_sys::Query;
    let stmt = base_plan(parse, qs, cursor, params); // cold plan, to inspect

    let mut ctx = Ctx { hydrations: Vec::new(), federate_targets: Vec::new() };
    if !stmt.is_null() {
        classify_walk((*stmt).planTree, (*stmt).rtable, &mut ctx);
    }

    // A not-yet-owned table accessed without a range-key bound must reach the
    // source. Preferred: push the whole query to the foreign tables (postgres_fdw
    // computes joins/aggregates remotely). Fallback (if we can't rewrite the RTEs,
    // e.g. an exotic shape): own those tables whole — NEVER serve a local
    // incomplete result.
    if !ctx.federate_targets.is_empty() {
        if !parse_copy.is_null() && swap_clone_rtes_to_foreign(parse_copy) > 0 {
            return base_plan(parse_copy, qs, cursor, params);
        }
        for t in &ctx.federate_targets {
            do_hydrate(t); // whole-table fallback (correct, not lazy)
        }
    }

    // Hydrate the needed ranges/small tables, then re-plan on the populated local
    // tables (fresh stats -> indexes used).
    let did = !ctx.hydrations.is_empty() || !ctx.federate_targets.is_empty();
    for h in &ctx.hydrations {
        do_hydrate(h);
    }
    if did && !parse_copy.is_null() {
        base_plan(parse_copy, qs, cursor, params)
    } else {
        stmt // everything already owned/covered -> local
    }
}

// Classify each base scan on a registered clone table.
unsafe fn classify_walk(plan: *mut pg_sys::Plan, rtable: *mut pg_sys::List, ctx: &mut Ctx) {
    if plan.is_null() {
        return;
    }
    let tag = (*(plan as *mut pg_sys::Node)).type_;
    match tag {
        pg_sys::NodeTag::T_SeqScan
        | pg_sys::NodeTag::T_IndexScan
        | pg_sys::NodeTag::T_IndexOnlyScan
        | pg_sys::NodeTag::T_BitmapHeapScan => {
            let scan = plan as *mut pg_sys::Scan;
            let scanrelid = (*scan).scanrelid;
            if scanrelid >= 1 {
                if let Some(rte) = rte_fetch(rtable, scanrelid) {
                    classify_scan((*rte).relid, scanrelid, plan, tag, ctx);
                }
            }
        }
        pg_sys::NodeTag::T_Append => {
            for p in PgList::<pg_sys::Plan>::from_pg((*(plan as *mut pg_sys::Append)).appendplans)
                .iter_ptr()
            {
                classify_walk(p, rtable, ctx);
            }
        }
        pg_sys::NodeTag::T_MergeAppend => {
            for p in
                PgList::<pg_sys::Plan>::from_pg((*(plan as *mut pg_sys::MergeAppend)).mergeplans)
                    .iter_ptr()
            {
                classify_walk(p, rtable, ctx);
            }
        }
        pg_sys::NodeTag::T_SubqueryScan => {
            classify_walk((*(plan as *mut pg_sys::SubqueryScan)).subplan, rtable, ctx);
        }
        _ => {}
    }
    classify_walk((*plan).lefttree, rtable, ctx);
    classify_walk((*plan).righttree, rtable, ctx);
}

unsafe fn classify_scan(
    relid: pg_sys::Oid,
    scanrelid: pg_sys::Index,
    plan: *mut pg_sys::Plan,
    tag: pg_sys::NodeTag,
    ctx: &mut Ctx,
) {
    if u32::from(relid) == 0 {
        return;
    }
    let Some(info) = gfs_lookup_clone(relid) else {
        return;
    };
    if info.collist.is_empty() {
        return;
    }
    if info.whole_cached {
        return; // owned -> serve local
    }
    if info.chunk_kind != "int" || info.key_attno == 0 {
        // non-rangeable (uuid) / small table -> own it whole on first touch.
        ctx.hydrations.push(Hydration {
            local_ref: info.local_ref,
            source_ref: info.source_ref,
            collist: info.collist,
            relid,
            key_col: info.key_col,
            lo: 0,
            hi: 0,
            whole: true,
        });
        return;
    }
    // Range-keyed table: does the query bound the key?
    match extract_key_range(plan, scanrelid, info.key_attno, tag) {
        Some((lo, hi)) => {
            if gfs_is_covered(relid, lo, hi) {
                // range already owned -> local
            } else {
                ctx.hydrations.push(Hydration {
                    local_ref: info.local_ref,
                    source_ref: info.source_ref,
                    collist: info.collist,
                    relid,
                    key_col: info.key_col,
                    lo,
                    hi,
                    whole: false,
                });
            }
        }
        None => {
            // No range bound, not owned -> must reach the source. Record as a
            // whole-table target (used to federate, or to own as a fallback).
            ctx.federate_targets.push(Hydration {
                local_ref: info.local_ref,
                source_ref: info.source_ref,
                collist: info.collist,
                relid,
                key_col: info.key_col,
                lo: 0,
                hi: 0,
                whole: true,
            });
        }
    }
}

unsafe fn rte_fetch(
    rtable: *mut pg_sys::List,
    scanrelid: pg_sys::Index,
) -> Option<*mut pg_sys::RangeTblEntry> {
    if scanrelid == 0 {
        return None;
    }
    PgList::<pg_sys::RangeTblEntry>::from_pg(rtable).get_ptr((scanrelid - 1) as usize)
}

// ===========================================================================
// Range extraction: find [lo,hi] bounds on the table's range key in a scan.
// ===========================================================================
unsafe fn extract_key_range(
    plan: *mut pg_sys::Plan,
    scanrelid: pg_sys::Index,
    key_attno: i16,
    tag: pg_sys::NodeTag,
) -> Option<(i64, i64)> {
    let mut conds: Vec<*mut pg_sys::Node> = Vec::new();
    push_list(&mut conds, (*plan).qual);
    match tag {
        pg_sys::NodeTag::T_IndexScan => {
            push_list(&mut conds, (*(plan as *mut pg_sys::IndexScan)).indexqualorig)
        }
        pg_sys::NodeTag::T_BitmapHeapScan => {
            push_list(&mut conds, (*(plan as *mut pg_sys::BitmapHeapScan)).bitmapqualorig)
        }
        pg_sys::NodeTag::T_IndexOnlyScan => {
            push_list(&mut conds, (*(plan as *mut pg_sys::IndexOnlyScan)).recheckqual)
        }
        _ => {}
    }

    let mut lo = i64::MIN;
    let mut hi = i64::MAX;
    let mut bounded = false;
    for node in conds {
        if node.is_null() || (*node).type_ != pg_sys::NodeTag::T_OpExpr {
            continue;
        }
        let op = node as *mut pg_sys::OpExpr;
        let args = PgList::<pg_sys::Node>::from_pg((*op).args);
        if args.len() != 2 {
            continue;
        }
        let a = args.get_ptr(0).unwrap();
        let b = args.get_ptr(1).unwrap();
        // Identify (Var on the key column, Const integer); handle either order.
        let (cst, var_left) = if is_key_var(a, scanrelid, key_attno) {
            (const_int(b), true)
        } else if is_key_var(b, scanrelid, key_attno) {
            (const_int(a), false)
        } else {
            continue;
        };
        let Some(v) = cst else { continue };
        let name = opname((*op).opno);
        let sym = name.as_deref().unwrap_or("");
        // If the Var is on the right, the comparison reads reversed (c op var).
        let eff = if var_left { sym } else { flip(sym) };
        match eff {
            ">=" => { lo = lo.max(v); bounded = true; }
            ">" => { lo = lo.max(v.saturating_add(1)); bounded = true; }
            "<=" => { hi = hi.min(v); bounded = true; }
            "<" => { hi = hi.min(v.saturating_sub(1)); bounded = true; }
            "=" => { lo = lo.max(v); hi = hi.min(v); bounded = true; }
            _ => {}
        }
    }
    if bounded && lo <= hi {
        Some((lo, hi))
    } else {
        None
    }
}

fn flip(sym: &str) -> &str {
    match sym {
        ">=" => "<=",
        ">" => "<",
        "<=" => ">=",
        "<" => ">",
        other => other,
    }
}

unsafe fn is_key_var(node: *mut pg_sys::Node, scanrelid: pg_sys::Index, key_attno: i16) -> bool {
    if node.is_null() || (*node).type_ != pg_sys::NodeTag::T_Var {
        return false;
    }
    let v = node as *mut pg_sys::Var;
    (*v).varno as u32 == scanrelid && (*v).varattno == key_attno && (*v).varlevelsup == 0
}

unsafe fn const_int(node: *mut pg_sys::Node) -> Option<i64> {
    if node.is_null() || (*node).type_ != pg_sys::NodeTag::T_Const {
        return None;
    }
    let c = node as *mut pg_sys::Const;
    if (*c).constisnull {
        return None;
    }
    let d = (*c).constvalue.value() as i64;
    match u32::from((*c).consttype) {
        20 => Some(d),               // int8
        23 => Some(d as i32 as i64), // int4
        21 => Some(d as i16 as i64), // int2
        _ => None,
    }
}

unsafe fn opname(opno: pg_sys::Oid) -> Option<String> {
    let p = pg_sys::get_opname(opno);
    if p.is_null() {
        None
    } else {
        Some(CStr::from_ptr(p).to_string_lossy().into_owned())
    }
}

unsafe fn push_list(out: &mut Vec<*mut pg_sys::Node>, list: *mut pg_sys::List) {
    if list.is_null() {
        return;
    }
    for n in PgList::<pg_sys::Node>::from_pg(list).iter_ptr() {
        if !n.is_null() {
            out.push(n);
        }
    }
}

// ===========================================================================
// Federate: rewrite clone RTEs -> foreign tables so postgres_fdw pushes down.
// ===========================================================================
unsafe fn swap_clone_rtes_to_foreign(query: *mut pg_sys::Query) -> i32 {
    swap_query(query)
}

// Recursively rewrite clone-table RTEs -> foreign across the Query and its nested
// subqueries / CTEs (a clone table inside a subquery must be swapped too, else we
// would fall through to a local — incomplete — plan).
unsafe fn swap_query(query: *mut pg_sys::Query) -> i32 {
    if query.is_null() {
        return 0;
    }
    let mut n = 0i32;
    for rte in PgList::<pg_sys::RangeTblEntry>::from_pg((*query).rtable).iter_ptr() {
        if rte.is_null() {
            continue;
        }
        match (*rte).rtekind {
            pg_sys::RTEKind::RTE_RELATION => {
                let original = (*rte).relid;
                if let Some(foreign) = gfs_source_oid(original) {
                    bump_federate(original);
                    pg_sys::LockRelationOid(foreign, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
                    (*rte).relid = foreign;
                    (*rte).relkind = pg_sys::RELKIND_FOREIGN_TABLE as c_char;
                    (*rte).inh = false;
                    if (*rte).perminfoindex > 0 {
                        if let Some(pi) =
                            PgList::<pg_sys::RTEPermissionInfo>::from_pg((*query).rteperminfos)
                                .get_ptr(((*rte).perminfoindex - 1) as usize)
                        {
                            (*pi).relid = foreign;
                        }
                    }
                    n += 1;
                }
            }
            pg_sys::RTEKind::RTE_SUBQUERY => n += swap_query((*rte).subquery),
            _ => {}
        }
    }
    for cte in PgList::<pg_sys::CommonTableExpr>::from_pg((*query).cteList).iter_ptr() {
        if !cte.is_null() {
            n += swap_query((*cte).ctequery as *mut pg_sys::Query);
        }
    }
    n
}

// ===========================================================================
// SPI: catalog lookups + hydration.
// ===========================================================================
struct CloneInfo {
    local_ref: String,
    source_ref: String,
    collist: String,
    chunk_kind: String,
    whole_cached: bool,
    key_col: String,
    key_attno: i16,
}

unsafe fn spi_text(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        None
    } else {
        Some(CStr::from_ptr(p).to_string_lossy().into_owned())
    }
}

unsafe fn gfs_lookup_clone(relid: pg_sys::Oid) -> Option<CloneInfo> {
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
                            AND attname = s.key_col), 0)::text \
           FROM gfs.clone_source s \
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
                key_attno: at.trim().parse::<i16>().unwrap_or(0),
            });
        }
    }
    pg_sys::SPI_finish();
    out
}

unsafe fn gfs_source_oid(relid: pg_sys::Oid) -> Option<pg_sys::Oid> {
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

unsafe fn gfs_is_covered(relid: pg_sys::Oid, lo: i64, hi: i64) -> bool {
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

/// Count a query that was pushed to the source for this clone table (so the demo
/// can label it "federated" rather than "local" — it returned 0 hydrated rows but
/// did contact the source).
unsafe fn bump_federate(relid: pg_sys::Oid) {
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

unsafe fn do_hydrate(h: &Hydration) {
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        return;
    }
    let sql = if h.whole {
        format!(
            "INSERT INTO {l} ({c}) SELECT {c} FROM {s} ON CONFLICT DO NOTHING",
            l = h.local_ref, c = h.collist, s = h.source_ref
        )
    } else {
        format!(
            "INSERT INTO {l} ({c}) SELECT {c} FROM {s} WHERE {k} BETWEEN {lo} AND {hi} ON CONFLICT DO NOTHING",
            l = h.local_ref, c = h.collist, s = h.source_ref, k = h.key_col, lo = h.lo, hi = h.hi
        )
    };
    let q = CString::new(sql).unwrap();
    let rc = pg_sys::SPI_execute(q.as_ptr(), false, 0);
    let n = if rc == pg_sys::SPI_OK_INSERT as i32 { pg_sys::SPI_processed as i64 } else { 0 };

    // Record completeness so future queries elide the source.
    let rec = if h.whole {
        format!("UPDATE gfs.clone_source SET whole_cached = true WHERE relid::oid = {}", u32::from(h.relid))
    } else {
        format!("SELECT gfs.note_range({}::oid::regclass, {}, {})", u32::from(h.relid), h.lo, h.hi)
    };
    pg_sys::SPI_execute(CString::new(rec).unwrap().as_ptr(), false, 0);

    // Refresh planner stats + activity counter.
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
    pg_sys::SPI_finish();
}

// ===========================================================================
// Catalog + API.
// ===========================================================================
extension_sql!(
    r#"
CREATE SCHEMA gfs;
COMMENT ON SCHEMA gfs IS 'GFS clone catalog + API (the planner hook reads clone_source/cached; stats in clone_stats)';

CREATE TABLE gfs.clone_source (
    relid        regclass PRIMARY KEY,
    source_ref   text     NOT NULL,
    key_col      text     NOT NULL DEFAULT 'id',
    chunk_kind   text     NOT NULL DEFAULT 'whole',  -- 'int' (range key) | 'whole'
    whole_cached boolean  NOT NULL DEFAULT false
);
COMMENT ON TABLE gfs.clone_source IS 'Per clone table: source (foreign) ref, range key, and whether it is fully owned';

CREATE TABLE gfs.cached (
    relid regclass NOT NULL REFERENCES gfs.clone_source(relid) ON DELETE CASCADE,
    lo    bigint   NOT NULL,
    hi    bigint   NOT NULL
);
CREATE INDEX ON gfs.cached (relid);
COMMENT ON TABLE gfs.cached IS 'Hydrated key ranges per clone table (range-granular completeness for elision)';

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
DECLARE kind text := 'whole';
BEGIN
    -- range-key strategy only for integer keys; everything else hydrates whole.
    SELECT CASE WHEN t.typname IN ('int2','int4','int8') THEN 'int' ELSE 'whole' END
      INTO kind
      FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid
     WHERE a.attrelid = local AND a.attname = key_col;
    kind := COALESCE(kind, 'whole');

    INSERT INTO gfs.clone_source(relid, source_ref, key_col, chunk_kind)
         VALUES (local, source_ref, key_col, kind)
    ON CONFLICT (relid)
        DO UPDATE SET source_ref = EXCLUDED.source_ref, key_col = EXCLUDED.key_col,
                      chunk_kind = EXCLUDED.chunk_kind;
    INSERT INTO gfs.clone_stats(relid) VALUES (local) ON CONFLICT (relid) DO NOTHING;
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
    EXECUTE format('INSERT INTO %s (%s) SELECT %s FROM %s ON CONFLICT DO NOTHING',
                   local::text, cols, cols, src);
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
           COALESCE(st.fetch_calls, 0)    AS fetch_calls,
           COALESCE(st.rows_fetched, 0)   AS rows_fetched,
           COALESCE(st.federate_calls, 0) AS federate_calls,
           (SELECT count(*) FROM gfs.cached c WHERE c.relid = s.relid) AS cached_ranges,
           st.last_fetch
      FROM gfs.clone_source s
      LEFT JOIN gfs.clone_stats st USING (relid)
     ORDER BY s.relid::text;

GRANT USAGE ON SCHEMA gfs TO PUBLIC;
GRANT SELECT ON gfs.clone_source, gfs.cached, gfs.clone_stats, gfs.clones TO PUBLIC;
"#,
    name = "gfs_catalog",
);
