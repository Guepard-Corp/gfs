//! Federate: rewrite clone RTEs -> foreign tables so postgres_fdw pushes down.

use core::ffi::c_char;

use pgrx::pg_sys;
use pgrx::PgList;

use crate::catalog::{bump_federate, gfs_source_oid};

pub(crate) unsafe fn swap_clone_rtes_to_foreign(query: *mut pg_sys::Query) -> i32 {
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
