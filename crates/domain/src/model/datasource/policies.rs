//! RLS policy metadata. Mirrors the datasource metadata schema.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PolicyAction {
    Permissive,
    Restrictive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PolicyCommand {
    Select,
    Insert,
    Update,
    Delete,
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub id: i64,
    pub schema: String,
    pub table: String,
    pub table_id: i64,
    pub name: String,
    pub action: PolicyAction,
    pub roles: Vec<String>,
    pub command: PolicyCommand,
    pub definition: Option<String>,
    pub check: Option<String>,
}
