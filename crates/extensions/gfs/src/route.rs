//! Router: classify every base scan on a registered clone table, then decide per
//! table — serve local (covered/owned), hydrate a range/whole/partial slice, or
//! federate the query to the source.

use core::ffi::{c_char, c_int};

use pgrx::pg_sys;
use pgrx::PgList;

use crate::base_plan;
use crate::catalog::{
    bump_access, gfs_enqueue_partial, gfs_is_covered, gfs_lookup_clone, gfs_note_pred_seen,
    gfs_pred_count, gfs_pred_state, gfs_set_no_partial, gfs_throttle,
};
use crate::federate::swap_clone_rtes_to_foreign;
use crate::hydrate::do_hydrate;
use crate::keyrange::extract_key_range;
use crate::model::{CloneInfo, Hydration};
use crate::pushdown::deparse_restriction;
use crate::worker;

struct Ctx {
    hydrations: Vec<Hydration>,       // range/whole fetches for coverable/small tables
    federate_targets: Vec<Hydration>, // tables to push to the source (whole-fallback if swap fails)
    partials: Vec<Hydration>,         // selective per-predicate fetches on too-big tables (capped, self-validating)
}

pub(crate) unsafe fn gfs_route(
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
            Some((true, _, _)) => return, // complete -> serve local (0 contact)
            Some((_, true, _)) => {
                // known not-selective (a prior capped pull overflowed) -> federate, no re-probe
                push_by_cost(ctx, tr, b, tr, h, &info, mk(0, 0, true, s(), s(), 0));
                return;
            }
            Some((_, _, true)) => {
                // an ASYNC partial copy is already pending in the background -> federate
                // for an immediate answer; the worker flips it to complete (local) once
                // the copy commits. Re-kick a drainer (deduped by an advisory lock, so
                // it's a no-op if one is already running): this self-heals a job that an
                // earlier worker missed (e.g. spawned before the enqueue committed).
                worker::spawn();
                ctx.federate_targets.push(mk(0, 0, true, s(), s(), 0));
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
            Some((false, false, false)) => {} // seen before -> consider partial below
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
            // ASYNC partial (previously a synchronous, blocking pull): mark the
            // predicate queued, kick the background copy worker, and federate THIS
            // query now for an immediate answer. The worker performs the capped,
            // self-validating copy off the critical path and flips the predicate to
            // complete -> future queries serve local. No query ever blocks on it.
            gfs_enqueue_partial(relid, &pred);
            worker::spawn();
            ctx.federate_targets.push(mk(0, 0, true, s(), s(), 0));
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
