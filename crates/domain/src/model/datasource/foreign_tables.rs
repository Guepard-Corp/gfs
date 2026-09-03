//! Foreign table metadata. Mirrors the datasource metadata schema.

use serde::{Deserialize, Serialize};

use super::columns::Column;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignTable {
    pub id: i64,
    pub schema: String,
    pub name: String,
    pub comment: Option<String>,
    pub foreign_server_name: String,
    pub foreign_data_wrapper_name: String,
    pub foreign_data_wrapper_handler: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<Column>>,
}
