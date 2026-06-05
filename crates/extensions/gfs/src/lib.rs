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

use core::ffi::{c_char, c_int, c_void};
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
    where_sql: String, // PARTIAL hydration: fetch only rows matching this predicate
    pred_key: String,  // completeness key for the predicate (so repeats serve local)
    partial_cap: i64,  // PARTIAL / time-range: hard row cap (LIMIT = cap+1); overflow -> federate
    time_key: bool,    // lo/hi are epoch MICROSECONDS on a date/timestamp key (capped range hydrate)
    key_type: String,  // typname of the key column (for the temporal literal reconstruction)
}

struct Ctx {
    hydrations: Vec<Hydration>,       // range/whole fetches for coverable/small tables
    federate_targets: Vec<Hydration>, // tables to push to the source (whole-fallback if swap fails)
    partials: Vec<Hydration>,         // selective per-predicate fetches on too-big tables (capped, self-validating)
}

unsafe fn gfs_route(
    parse: *mut pg_sys::Query,
    qs: *const c_char,
    cursor: c_int,
    params: pg_sys::ParamListInfo,
) -> *mut pg_sys::PlannedStmt {
    let parse_copy = pg_sys::copyObjectImpl(parse as *const _) as *mut pg_sys::Query;
    // A WRITE (UPDATE/DELETE/INSERT..SELECT) must NEVER be federated: swapping the
    // query's clone RTEs to foreign would swap the ModifyTable's RESULT relation too,
    // sending the write to the SOURCE (corrupting prod, or erroring on a read-only
    // mapping). For writes we therefore never swap -> the federate fallback below
    // whole-hydrates those tables LOCALLY, so the write applies to complete local
    // data with the source untouched. Only SELECT may federate.
    let is_write = !parse.is_null() && (*parse).commandType != pg_sys::CmdType::CMD_SELECT;
    let stmt = base_plan(parse, qs, cursor, params); // cold plan, to inspect

    let mut ctx =
        Ctx { hydrations: Vec::new(), federate_targets: Vec::new(), partials: Vec::new() };
    if !stmt.is_null() {
        classify_walk((*stmt).planTree, (*stmt).rtable, &mut ctx);
    }

    // PARTIAL pre-pass: pull each selective slice with a HARD cap. The capped pull
    // self-validates selectivity against REALITY (not an estimate): if the source
    // had more than the cap of matching rows, the slice is not actually selective
    // -> we must NOT serve it local (the cap truncated it) -> federate this query.
    // A committed slice records cached_predicate.complete so repeats serve local.
    for p in &ctx.partials {
        if !do_hydrate(p) {
            // overflowed -> incomplete locally -> federate the whole query (correct + bounded)
            ctx.federate_targets.push(whole_of(p));
        }
    }

    // A not-yet-owned table accessed without a range-key bound must reach the
    // source. Preferred: push the whole query to the foreign tables (postgres_fdw
    // computes joins/aggregates remotely). Fallback (if we can't rewrite the RTEs,
    // e.g. an exotic shape): own those tables whole — NEVER serve a local
    // incomplete result.
    if !ctx.federate_targets.is_empty() {
        if !is_write && !parse_copy.is_null() && swap_clone_rtes_to_foreign(parse_copy) > 0 {
            gfs_throttle(); // rate-limit source contact (the federated query hits prod)
            return base_plan(parse_copy, qs, cursor, params);
        }
        // Writes never reach the swap above -> they whole-hydrate locally here, so an
        // UPDATE/DELETE applies to complete local data (source untouched).
        for t in &ctx.federate_targets {
            do_hydrate(t); // whole-table fallback (correct, not lazy)
        }
    }

    // Hydrate the needed ranges/small tables, then re-plan on the populated local
    // tables (fresh stats -> indexes used).
    let did = !ctx.hydrations.is_empty()
        || !ctx.federate_targets.is_empty()
        || !ctx.partials.is_empty();
    for h in &ctx.hydrations {
        do_hydrate(h);
    }
    if did && !parse_copy.is_null() {
        base_plan(parse_copy, qs, cursor, params)
    } else {
        stmt // everything already owned/covered -> local
    }
}

/// A whole-table federate/own descriptor for the same relation as `h` (used when a
/// partial slice overflows: the table must reach the source as a whole).
fn whole_of(h: &Hydration) -> Hydration {
    Hydration {
        local_ref: h.local_ref.clone(),
        source_ref: h.source_ref.clone(),
        collist: h.collist.clone(),
        relid: h.relid,
        key_col: h.key_col.clone(),
        lo: 0,
        hi: 0,
        whole: true,
        where_sql: String::new(),
        pred_key: String::new(),
        partial_cap: 0,
        time_key: false,
        key_type: String::new(),
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
    bump_access(relid);
    if info.whole_cached {
        return; // owned -> serve local
    }

    let b = info.row_bytes.max(1) as f64; // bytes/row
    let tr = info.source_rows.max(0) as f64; // source table size
    let h = (info.access_count as f64).min(info.w_horizon); // amortization horizon
    let s = String::new;
    let mk = |lo: i64, hi: i64, whole: bool, w: String, p: String, cap: i64| Hydration {
        local_ref: info.local_ref.clone(),
        source_ref: info.source_ref.clone(),
        collist: info.collist.clone(),
        relid,
        key_col: info.key_col.clone(),
        lo,
        hi,
        whole,
        where_sql: w,
        pred_key: p,
        partial_cap: cap,
        time_key: false,
        key_type: info.key_type.clone(),
    };

    // 1. RANGE-key bound (id BETWEEN / placed_at BETWEEN) -> range model: covered ->
    //    local (elision), else fetch the missing key span. INTEGER keys size the
    //    span in rows; TEMPORAL keys (date/timestamp) map the bound to epoch micros
    //    and fetch a CAPPED slice (we can't size micros in rows) that self-validates
    //    (overflow -> federate) -- both record coalesced coverage in gfs.cached.
    let is_time = info.chunk_kind == "time";
    if (info.chunk_kind == "int" || is_time) && info.key_attno != 0 {
        if let Some((lo, hi)) = extract_key_range(plan, scanrelid, info.key_attno, tag, is_time) {
            if gfs_is_covered(relid, lo, hi) {
                return; // already owned (range covered) -> serve local
            }
            if is_time {
                let cap = (info.w_partial_max_frac * tr).floor().max(1.0) as i64;
                ctx.partials.push(Hydration {
                    local_ref: info.local_ref.clone(),
                    source_ref: info.source_ref.clone(),
                    collist: info.collist.clone(),
                    relid,
                    key_col: info.key_col.clone(),
                    lo,
                    hi,
                    whole: false,
                    where_sql: String::new(),
                    pred_key: String::new(),
                    partial_cap: cap,
                    time_key: true,
                    key_type: info.key_type.clone(),
                });
                return;
            }
            let span = ((hi - lo).saturating_add(1)).max(0) as f64;
            let own_rows = span.min(tr.max(1.0));
            push_by_cost(ctx, own_rows, b, tr, h, &info, mk(lo, hi, false, s(), s(), 0));
            return;
        }
    }

    // 2. WHOLE-OWN affordability gate. Identical arithmetic to push_by_cost at
    //    own_rows=Tr, hoisted here BEFORE any predicate work. If the table is
    //    affordable to own WHOLE (or terminal no_partial), the partial branch is
    //    skipped entirely -- whole-own (1 contact, then all-local) dominates partial
    //    and this gate is independent of selectivity, so no slice can flip an
    //    ownable table into partial. This is the line that keeps the benchmark at
    //    source_ops=12 and makes the cost-v3 ordering regression structurally
    //    impossible.
    let whole_own_cost = info.w_net * b * tr;
    let fed_call = info.w_source * tr.max(1.0);
    let whole_ownable = whole_own_cost <= info.w_negligible
        || (whole_own_cost <= info.w_ceiling && whole_own_cost <= (h + 1.0) * fed_call);
    if whole_ownable || info.no_partial {
        // push_by_cost(Tr) owns iff whole_ownable, else federates -- never partial.
        push_by_cost(ctx, tr, b, tr, h, &info, mk(0, 0, true, s(), s(), 0));
        return;
    }

    // 3. NOT whole-ownable: a SELECTIVE single-relation predicate can be PARTIAL-
    //    owned (its matching slice only), keeping a too-big clone partial.
    if let Some(pred) = deparse_restriction(relid, plan, scanrelid, tag) {
        match gfs_pred_state(relid, &pred) {
            Some((true, _)) => return, // complete -> serve local (0 contact)
            Some((_, true)) => {
                // known not-selective (a prior capped pull overflowed) -> federate, no re-probe
                push_by_cost(ctx, tr, b, tr, h, &info, mk(0, 0, true, s(), s(), 0));
                return;
            }
            None => {
                // SECOND-CHANCE: a predicate's FIRST sighting federates (== cost-v2)
                // and is only recorded as "seen" (no contact, no estimate); partial
                // is paid only once reuse is demonstrated (the 2nd identical touch).
                gfs_note_pred_seen(relid, &pred);
                push_by_cost(ctx, tr, b, tr, h, &info, mk(0, 0, true, s(), s(), 0));
                return;
            }
            Some((false, false)) => {} // seen before -> consider partial below
        }

        // CONTACT cap / ROW cap / headroom -> PROMOTE: collapse the piecemeal slices
        //   into ONE whole-own (if capacity allows) or mark terminal no_partial and
        //   federate per call. The contact cap bounds tiny-slice floods the row cap
        //   is blind to; the scarce resource is CONTACTS.
        let cap_rows = (info.w_partial_max_frac * tr).floor().max(1.0);
        let contact_cap_hit = gfs_pred_count(relid) >= info.w_max_partial_preds;
        let row_cap_hit = (info.partial_rows as f64) >= info.w_promote_frac * tr;
        let headroom_ok = (info.partial_rows as f64) + cap_rows <= info.w_promote_frac * tr;
        if contact_cap_hit || row_cap_hit || !headroom_ok {
            if whole_own_cost <= info.w_ceiling {
                ctx.hydrations.push(mk(0, 0, true, s(), s(), 0)); // forced whole-own (one final contact)
            } else {
                gfs_set_no_partial(relid);
                ctx.federate_targets.push(mk(0, 0, true, s(), s(), 0));
            }
            return;
        }

        // CAPACITY: even the maximum allowed slice (partial_max_frac*Tr) must fit
        //   under the ceiling -- this uses the CAP, not an estimate, so no mis-
        //   estimate can sneak an over-budget slice through. The real pull is then
        //   hard-capped and self-validated in do_hydrate (overflow -> federate).
        if info.w_net * b * cap_rows <= info.w_ceiling {
            ctx.partials.push(mk(0, 0, false, pred.clone(), pred, cap_rows as i64));
        } else {
            push_by_cost(ctx, tr, b, tr, h, &info, mk(0, 0, true, s(), s(), 0)); // slice too big -> federate
        }
        return;
    }

    // 4. No usable restriction (join-derived / aggregate input) -> cost decides
    //    own-whole vs federate (not whole-ownable here -> federate).
    push_by_cost(ctx, tr, b, tr, h, &info, mk(0, 0, true, s(), s(), 0));
}

/// Cost/energy gate: OWN (push `hyd`) when the pull is negligible, or -- below the
/// capacity ceiling -- amortized over H calls; else FEDERATE (push the source to
/// it). E(own)=net*B*own_rows (one-time); E(feder)=source*Tr per call (the source
/// re-scans every time, incl. prod-load penalty).
unsafe fn push_by_cost(
    ctx: &mut Ctx,
    own_rows: f64,
    b: f64,
    tr: f64,
    h: f64,
    info: &CloneInfo,
    hyd: Hydration,
) {
    let own_cost = info.w_net * b * own_rows;
    let fed_call = info.w_source * tr.max(1.0);
    let own = own_cost <= info.w_negligible
        || (own_cost <= info.w_ceiling && own_cost <= (h + 1.0) * fed_call);
    if own {
        ctx.hydrations.push(hyd);
    } else {
        ctx.federate_targets.push(Hydration {
            local_ref: hyd.local_ref,
            source_ref: hyd.source_ref,
            collist: hyd.collist,
            relid: hyd.relid,
            key_col: hyd.key_col,
            lo: 0,
            hi: 0,
            whole: true,
            where_sql: String::new(),
            pred_key: String::new(),
            partial_cap: 0,
            time_key: false,
            key_type: String::new(),
        });
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
// Temporal sentinels (epoch microseconds, UTC) used as the "unbounded" range ends
// for time keys: reconstructable by to_timestamp and safe under note_range's +1.
const TIME_FAR_PAST: i64 = -62_135_596_800_000_000; // 0001-01-01
const TIME_FAR_FUTURE: i64 = 253_402_300_799_000_000; // 9999-12-31 23:59:59

unsafe fn extract_key_range(
    plan: *mut pg_sys::Plan,
    scanrelid: pg_sys::Index,
    key_attno: i16,
    tag: pg_sys::NodeTag,
    is_time: bool,
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

    let (mut lo, mut hi) = if is_time { (TIME_FAR_PAST, TIME_FAR_FUTURE) } else { (i64::MIN, i64::MAX) };
    let decode = |n: *mut pg_sys::Node| if is_time { const_time(n) } else { const_int(n) };
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
        // Identify (Var on the key column, Const int/temporal); handle either order.
        let (cst, var_left) = if is_key_var(a, scanrelid, key_attno) {
            (decode(b), true)
        } else if is_key_var(b, scanrelid, key_attno) {
            (decode(a), false)
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

/// Decode a DATE / TIMESTAMP / TIMESTAMPTZ Const to epoch MICROSECONDS (UTC), so a
/// temporal range key maps onto the same integer gfs.cached coverage as integers.
/// PG stores these relative to 2000-01-01; we shift to the 1970 Unix epoch (the
/// offset is 946_684_800 s = 10_957 days) and treat the value as UTC -- matched by
/// the `to_timestamp(...) AT TIME ZONE 'UTC'` reconstruction in do_hydrate.
unsafe fn const_time(node: *mut pg_sys::Node) -> Option<i64> {
    if node.is_null() || (*node).type_ != pg_sys::NodeTag::T_Const {
        return None;
    }
    let c = node as *mut pg_sys::Const;
    if (*c).constisnull {
        return None;
    }
    const PG_EPOCH_US: i64 = 946_684_800_000_000; // 2000-01-01 in epoch microseconds
    let raw = (*c).constvalue.value() as i64;
    match u32::from((*c).consttype) {
        1082 => Some((raw as i32 as i64) * 86_400_000_000 + PG_EPOCH_US), // date: int4 days since 2000
        1114 | 1184 => Some(raw + PG_EPOCH_US),                            // timestamp(tz): int8 micros since 2000
        _ => None,
    }
}

/// Rebuild a temporal literal from epoch microseconds for the hydration WHERE,
/// keyed to the column type and pinned to UTC so it round-trips const_time exactly
/// (timestamptz compares by absolute instant; timestamp/date are wall-clock-as-UTC).
fn time_recon(epoch_us: i64, key_type: &str) -> String {
    let base = format!("to_timestamp({}::float8 / 1000000.0)", epoch_us);
    match key_type {
        "timestamp" => format!("({} AT TIME ZONE 'UTC')", base),
        "date" => format!("({} AT TIME ZONE 'UTC')::date", base),
        _ => base, // timestamptz (absolute instant) + safe fallback
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
// Deparse a scan's pushable restriction into a remote WHERE (for PARTIAL
// hydration: fetch only the matching rows, not the whole table).
// ===========================================================================
struct PushCtx {
    ok: bool,
    scanrelid: pg_sys::Index,
}

#[pg_guard]
unsafe extern "C-unwind" fn push_walker(node: *mut pg_sys::Node, ctx: *mut c_void) -> bool {
    if node.is_null() {
        return false;
    }
    let c = &mut *(ctx as *mut PushCtx);
    match (*node).type_ {
        pg_sys::NodeTag::T_Var => {
            let v = node as *mut pg_sys::Var;
            if (*v).varno < 1 || (*v).varno as u32 != c.scanrelid || (*v).varlevelsup != 0 {
                c.ok = false;
                return true;
            }
        }
        pg_sys::NodeTag::T_Const
        | pg_sys::NodeTag::T_BoolExpr
        | pg_sys::NodeTag::T_RelabelType
        | pg_sys::NodeTag::T_NullTest => {}
        pg_sys::NodeTag::T_OpExpr => {
            if u32::from((*(node as *mut pg_sys::OpExpr)).opno) >= pg_sys::FirstNormalObjectId {
                c.ok = false;
                return true;
            }
        }
        pg_sys::NodeTag::T_ScalarArrayOpExpr => {
            if u32::from((*(node as *mut pg_sys::ScalarArrayOpExpr)).opno)
                >= pg_sys::FirstNormalObjectId
            {
                c.ok = false;
                return true;
            }
        }
        _ => {
            c.ok = false;
            return true;
        }
    }
    pg_sys::expression_tree_walker_impl(node, Some(push_walker), ctx)
}

unsafe fn node_is_pushable(node: *mut pg_sys::Node, scanrelid: pg_sys::Index) -> bool {
    if node.is_null() || pg_sys::contain_volatile_functions(node) {
        return false;
    }
    let mut c = PushCtx { ok: true, scanrelid };
    push_walker(node, &mut c as *mut _ as *mut c_void);
    c.ok
}

#[pg_guard]
unsafe extern "C-unwind" fn rewrite_walker(node: *mut pg_sys::Node, ctx: *mut c_void) -> bool {
    if node.is_null() {
        return false;
    }
    if (*node).type_ == pg_sys::NodeTag::T_Var {
        let v = node as *mut pg_sys::Var;
        let scanrelid = *(ctx as *mut pg_sys::Index);
        if (*v).varno as u32 == scanrelid {
            (*v).varno = 1;
            (*v).varnosyn = 1;
        }
    }
    pg_sys::expression_tree_walker_impl(node, Some(rewrite_walker), ctx)
}

unsafe fn deparse_one(
    relid: pg_sys::Oid,
    relname: *mut c_char,
    node: *mut pg_sys::Node,
    scanrelid: pg_sys::Index,
) -> Option<String> {
    let copy = pg_sys::copyObjectImpl(node as *const _) as *mut pg_sys::Node;
    if copy.is_null() {
        return None;
    }
    let mut sr = scanrelid;
    rewrite_walker(copy, &mut sr as *mut _ as *mut c_void);
    let ctx = pg_sys::deparse_context_for(relname as *const c_char, relid);
    let s = pg_sys::deparse_expression(copy, ctx, false, false);
    if s.is_null() {
        return None;
    }
    Some(CStr::from_ptr(s).to_string_lossy().into_owned())
}

/// AND of all pushable single-relation restriction conditions on this scan,
/// deparsed to bare-column SQL (a WHERE for fetching just the matching rows).
/// None if the scan has no usable restriction (join-derived / aggregate input).
unsafe fn deparse_restriction(
    relid: pg_sys::Oid,
    plan: *mut pg_sys::Plan,
    scanrelid: pg_sys::Index,
    tag: pg_sys::NodeTag,
) -> Option<String> {
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
    let relname = pg_sys::get_rel_name(relid);
    if relname.is_null() {
        return None;
    }
    let mut frags: Vec<String> = Vec::new();
    for node in conds {
        if !node.is_null() && node_is_pushable(node, scanrelid) {
            if let Some(s) = deparse_one(relid, relname, node, scanrelid) {
                if !frags.contains(&s) {
                    frags.push(s);
                }
            }
        }
    }
    if frags.is_empty() {
        None
    } else {
        Some(frags.iter().map(|f| format!("({})", f)).collect::<Vec<_>>().join(" AND "))
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
    key_type: String,  // typname of the key column ('date'/'timestamp'/'timestamptz' for chunk_kind='time')
    key_attno: i16,
    source_rows: i64,  // Tr: source table size (reltuples, captured at register)
    row_bytes: i64,    // B: avg bytes/row
    access_count: i64, // H: times this table has been queried (amortization)
    partial_rows: i64, // cumulative rows pulled by committed partial hydrations
    no_partial: bool,  // terminal: too big to own -> federate per call, no more probes
    w_net: f64,        // cost weights (gfs.cost)
    w_source: f64,
    w_negligible: f64,
    w_ceiling: f64,
    w_horizon: f64,
    w_partial_max_frac: f64,  // max slice fraction + hard pull cap
    w_promote_frac: f64,      // cumulative-pull fraction that auto-promotes to whole-own
    w_max_partial_preds: i64, // max distinct partial predicates (contacts) before promote
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
unsafe fn bump_access(relid: pg_sys::Oid) {
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

/// State of a predicate in the catalog: None = never seen; Some((complete,
/// overflowed)). complete=true -> matching rows fully hydrated (serve local);
/// overflowed=true -> a prior capped pull found too many matches (not selective ->
/// federate, never partial again); (false,false) -> a "seen once" second-chance
/// marker (the next identical touch may partial-hydrate).
unsafe fn gfs_pred_state(relid: pg_sys::Oid, pred: &str) -> Option<(bool, bool)> {
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
unsafe fn gfs_note_pred_seen(relid: pg_sys::Oid, pred: &str) {
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
unsafe fn gfs_pred_count(relid: pg_sys::Oid) -> i64 {
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
unsafe fn gfs_set_no_partial(relid: pg_sys::Oid) {
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

/// Prod-protection gate: before contacting the source, consume a rate-limit token;
/// if the per-clone budget is exhausted, wait (back-pressure, bounded) so 100s of
/// clones can't overwhelm the prod source. No-op when max_rate = 0 (unlimited).
unsafe fn gfs_throttle() {
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

/// Fetch a hydration into the local table. Returns true when the slice/table is
/// COMPLETE (safe to serve local); returns false ONLY for a PARTIAL pull that
/// overflowed its cap (too many matches -> not selective -> caller must federate,
/// the local rows are an incomplete subset and are never claimed complete).
unsafe fn do_hydrate(h: &Hydration) -> bool {
    gfs_throttle(); // rate-limit source contact
    if pg_sys::SPI_connect() != pg_sys::SPI_OK_CONNECT as i32 {
        // Couldn't hydrate. A capped pull (partial / time-range) would be incomplete
        // -> federate (false). A whole/int-range fetch never claims completeness on
        // failure -> safe (true).
        return h.where_sql.is_empty() && !h.time_key;
    }

    // PARTIAL: pull the matching slice with a HARD cap and self-validate against
    // REALITY (not an estimate). One source contact. `matched` (LIMIT cap+1) tells
    // us whether the source had MORE than the cap of matching rows: if so the slice
    // is not actually selective -> mark it overflowed (never partial again) and the
    // caller federates this query; the <=cap+1 rows already inserted are a genuine
    // subset (no completeness is claimed for them), so they are harmless.
    if !h.where_sql.is_empty() {
        let cap = h.partial_cap.max(0);
        let sql = format!(
            "WITH picked AS (SELECT {c} FROM {s} WHERE {w} LIMIT {lim}), \
                  ins AS (INSERT INTO {l} ({c}) SELECT {c} FROM picked ON CONFLICT DO NOTHING RETURNING 1) \
             SELECT (SELECT count(*) FROM picked)::int8::text, (SELECT count(*) FROM ins)::int8::text",
            c = h.collist, s = h.source_ref, w = h.where_sql, l = h.local_ref, lim = cap + 1
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
            "WITH picked AS (SELECT {c} FROM {s} WHERE {w} LIMIT {lim}), \
                  ins AS (INSERT INTO {l} ({c}) SELECT {c} FROM picked ON CONFLICT DO NOTHING RETURNING 1) \
             SELECT (SELECT count(*) FROM picked)::int8::text, (SELECT count(*) FROM ins)::int8::text",
            c = h.collist, s = h.source_ref, w = where_clause, l = h.local_ref, lim = cap + 1
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

    // WHOLE / RANGE.
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

    let rec = if h.whole {
        format!("UPDATE gfs.clone_source SET whole_cached = true WHERE relid::oid = {}", u32::from(h.relid))
    } else {
        format!("SELECT gfs.note_range({}::oid::regclass, {}, {})", u32::from(h.relid), h.lo, h.hi)
    };
    pg_sys::SPI_execute(CString::new(rec).unwrap().as_ptr(), false, 0);
    hydrate_finish(h, n);
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
    max_partial_preds int    NOT NULL DEFAULT 10    -- POLICY: max distinct partial predicates (CONTACTS) before
                                                    --   promote; bounds tiny-slice floods the row cap can't see.
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
    PRIMARY KEY (relid, pred)
);
COMMENT ON TABLE gfs.cached_predicate IS 'Non-key predicates seen by the router: complete=fully hydrated (local), overflowed=too many matches (federate). A bare row (both false) is a second-chance "seen once" marker.';

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
GRANT SELECT ON gfs.clone_source, gfs.cached, gfs.cached_predicate, gfs.clone_stats, gfs.cost, gfs.budget, gfs.clones TO PUBLIC;
"#,
    name = "gfs_catalog",
);
