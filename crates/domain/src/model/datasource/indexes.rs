//! Index metadata. Mirrors the datasource metadata schema.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexAttribute {
    pub attribute_number: Option<i32>,
    pub attribute_name: String,
    pub data_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub id: i64,
    pub table_id: i64,
    pub schema: String,
    pub name: Option<String>,
    pub is_unique: bool,
    pub is_primary: bool,
    pub index_definition: String,
    pub access_method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_attributes: Option<Vec<IndexAttribute>>,
}
