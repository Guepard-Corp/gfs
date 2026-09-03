//! Table metadata for datasources.
//! Mirrors the datasource metadata schema.

use serde::{Deserialize, Serialize};

use super::columns::Column;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TablePrimaryKey {
    pub table_id: i64,
    pub name: String,
    pub schema: String,
    pub table_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableRelationship {
    pub id: i64,
    pub constraint_name: String,
    pub source_schema: String,
    pub source_table_name: String,
    pub source_column_name: String,
    pub target_table_schema: String,
    pub target_table_name: String,
    pub target_column_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Table {
    pub id: i64,
    pub schema: String,
    pub name: String,
    pub rls_enabled: bool,
    pub rls_forced: bool,
    pub bytes: i64,
    pub size: String,
    pub live_rows_estimate: i64,
    pub dead_rows_estimate: i64,
    pub comment: Option<String>,
    pub primary_keys: Vec<TablePrimaryKey>,
    pub relationships: Vec<TableRelationship>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<Column>>,
}
