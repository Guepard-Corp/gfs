//! View metadata. Mirrors the datasource metadata schema.

use serde::{Deserialize, Serialize};

use super::columns::Column;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct View {
    pub id: i64,
    pub schema: String,
    pub name: String,
    pub is_updatable: bool,
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<Column>>,
}
