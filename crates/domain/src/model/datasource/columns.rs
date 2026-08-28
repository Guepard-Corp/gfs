//! Column metadata for datasource tables.
//! Mirrors the datasource metadata schema.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub id: String,
    pub table_id: i64,
    pub schema: String,
    pub table: String,
    pub name: String,
    pub ordinal_position: i32,
    pub data_type: String,
    pub format: String,
    pub is_identity: bool,
    pub identity_generation: Option<String>,
    pub is_generated: bool,
    pub is_nullable: bool,
    pub is_updatable: bool,
    pub is_unique: bool,
    pub check: Option<String>,
    pub default_value: Option<serde_json::Value>,
    pub enums: Vec<String>,
    pub comment: Option<String>,
}
