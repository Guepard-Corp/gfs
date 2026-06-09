//! Deparse a scan's pushable restriction into a remote WHERE (for PARTIAL
//! hydration: fetch only the matching rows, not the whole table).

use core::ffi::{c_char, c_void};
use std::ffi::CStr;

use pgrx::pg_sys;
use pgrx::prelude::*;

use crate::keyrange::push_list;

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
pub(crate) unsafe fn deparse_restriction(
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
