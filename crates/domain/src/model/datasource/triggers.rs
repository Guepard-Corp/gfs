//! Trigger metadata. Mirrors the datasource metadata schema.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trigger {
    pub id: i64,
    pub table_id: i64,
    pub name: String,
    pub table: String,
    pub schema: String,
    pub events: Vec<String>,
    pub function_name: String,
    pub function_schema: Option<String>,
    pub condition: Option<String>,
    pub timing: Option<String>,
    pub orientation: Option<String>,
    pub function_args: Option<Vec<String>>,
}
