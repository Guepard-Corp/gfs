//! Custom type metadata (database types, not Rust types).
//! Mirrors the datasource metadata schema. Module named db_types to avoid Rust keyword.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeAttribute {
    pub name: String,
    pub type_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbType {
    pub id: i64,
    pub name: String,
    pub schema: String,
    pub format: String,
    pub enums: Vec<String>,
    pub attributes: Vec<TypeAttribute>,
    pub comment: Option<String>,
}
