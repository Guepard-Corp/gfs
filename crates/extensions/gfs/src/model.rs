//! Shared descriptors passed between the router (classification + cost gate) and
//! the hydration engine. Kept in one place because they cross module boundaries.

use pgrx::pg_sys;

pub(crate) struct Hydration {
    pub(crate) local_ref: String,
    pub(crate) source_ref: String,
    pub(crate) collist: String,
    pub(crate) relid: pg_sys::Oid,
    pub(crate) key_col: String,
    pub(crate) lo: i64,
    pub(crate) hi: i64,
    pub(crate) whole: bool,
    pub(crate) where_sql: String, // PARTIAL hydration: fetch only rows matching this predicate
    pub(crate) pred_key: String,  // completeness key for the predicate (so repeats serve local)
    pub(crate) partial_cap: i64,  // PARTIAL / time-range: hard row cap (LIMIT = cap+1); overflow -> federate
    pub(crate) time_key: bool,    // lo/hi are epoch MICROSECONDS on a date/timestamp key (capped range hydrate)
    pub(crate) key_type: String,  // typname of the key column (for the temporal literal reconstruction)
}

pub(crate) struct CloneInfo {
    pub(crate) local_ref: String,
    pub(crate) source_ref: String,
    pub(crate) collist: String,
    pub(crate) chunk_kind: String,
    pub(crate) whole_cached: bool,
    pub(crate) key_col: String,
    pub(crate) key_type: String,  // typname of the key column ('date'/'timestamp'/'timestamptz' for chunk_kind='time')
    pub(crate) key_attno: i16,
    pub(crate) source_rows: i64,  // Tr: source table size (reltuples, captured at register)
    pub(crate) row_bytes: i64,    // B: avg bytes/row
    pub(crate) access_count: i64, // H: times this table has been queried (amortization)
    pub(crate) partial_rows: i64, // cumulative rows pulled by committed partial hydrations
    pub(crate) no_partial: bool,  // terminal: too big to own -> federate per call, no more probes
    pub(crate) w_net: f64,        // cost weights (gfs.cost)
    pub(crate) w_source: f64,
    pub(crate) w_negligible: f64,
    pub(crate) w_ceiling: f64,
    pub(crate) w_horizon: f64,
    pub(crate) w_partial_max_frac: f64,  // max slice fraction + hard pull cap
    pub(crate) w_promote_frac: f64,      // cumulative-pull fraction that auto-promotes to whole-own
    pub(crate) w_max_partial_preds: i64, // max distinct partial predicates (contacts) before promote
}
