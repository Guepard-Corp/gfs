//! Query result set types for datasources.
//! Mirrors the datasource metadata schema.

use serde::{Deserialize, Serialize};

/// Normalized column type for frontend visualization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColumnType {
    String,
    Number,
    Integer,
    Boolean,
    Date,
    Datetime,
    Timestamp,
    Time,
    Json,
    Jsonb,
    Array,
    Blob,
    Binary,
    Uuid,
    Decimal,
    Float,
    Null,
    Unknown,
}

/// Statistics about query execution results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DatasourceResultStat {
    #[serde(rename = "rowsAffected")]
    pub rows_affected: u64,
    #[serde(rename = "rowsRead")]
    pub rows_read: Option<u64>,
    #[serde(rename = "rowsWritten")]
    pub rows_written: Option<u64>,
    #[serde(rename = "queryDurationMs")]
    pub query_duration_ms: Option<f64>,
}

/// Column header metadata for query results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnHeader {
    pub name: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "originalType")]
    pub original_type: Option<String>,
    #[serde(rename = "type")]
    pub type_hint: Option<ColumnType>,
    pub schema: Option<String>,
    pub table: Option<String>,
    #[serde(rename = "originalName")]
    pub original_name: Option<String>,
    #[serde(rename = "primaryKey")]
    pub primary_key: Option<bool>,
    #[serde(rename = "columnId")]
    pub column_id: Option<i32>,
    #[serde(rename = "tableId")]
    pub table_id: Option<i32>,
}

/// A single row of data from query results.
pub type DatasourceRow = std::collections::HashMap<String, serde_json::Value>;

/// Complete result set from a datasource query execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasourceResultSet {
    pub rows: Vec<DatasourceRow>,
    pub columns: Vec<ColumnHeader>,
    pub stat: DatasourceResultStat,
}
