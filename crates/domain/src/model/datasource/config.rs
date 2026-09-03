//! Config/settings metadata (e.g. Postgres pg_settings). Mirrors the datasource metadata schema.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub name: String,
    pub setting: String,
    pub category: String,
    pub group: String,
    pub subgroup: String,
    pub unit: Option<String>,
    pub short_desc: String,
    pub extra_desc: Option<String>,
    pub context: String,
    pub vartype: String,
    pub source: String,
    pub min_val: Option<String>,
    pub max_val: Option<String>,
    pub enumvals: Option<Vec<String>>,
    pub boot_val: Option<String>,
    pub reset_val: Option<String>,
    pub sourcefile: Option<String>,
    pub sourceline: Option<i64>,
    pub pending_restart: bool,
}
