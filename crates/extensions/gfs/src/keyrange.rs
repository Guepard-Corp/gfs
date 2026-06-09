//! Range extraction: find [lo,hi] bounds on the table's range key in a scan, plus
//! the const-decoding / operator helpers it (and the deparser) rely on.

use std::ffi::CStr;

use pgrx::pg_sys;
use pgrx::PgList;

// Temporal sentinels (epoch microseconds, UTC) used as the "unbounded" range ends
// for time keys: reconstructable by to_timestamp and safe under note_range's +1.
pub(crate) const TIME_FAR_PAST: i64 = -62_135_596_800_000_000; // 0001-01-01
pub(crate) const TIME_FAR_FUTURE: i64 = 253_402_300_799_000_000; // 9999-12-31 23:59:59

pub(crate) unsafe fn extract_key_range(
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
pub(crate) fn time_recon(epoch_us: i64, key_type: &str) -> String {
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

pub(crate) unsafe fn push_list(out: &mut Vec<*mut pg_sys::Node>, list: *mut pg_sys::List) {
    if list.is_null() {
        return;
    }
    for n in PgList::<pg_sys::Node>::from_pg(list).iter_ptr() {
        if !n.is_null() {
            out.push(n);
        }
    }
}
