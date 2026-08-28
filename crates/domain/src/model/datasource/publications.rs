//! Publication metadata. Mirrors the datasource metadata schema.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicationTable {
    pub id: Option<i64>,
    pub name: String,
    pub schema: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Publication {
    pub id: i64,
    pub name: String,
    pub owner: String,
    pub publish_insert: bool,
    pub publish_update: bool,
    pub publish_delete: bool,
    pub publish_truncate: bool,
    pub tables: Option<Vec<PublicationTable>>,
}
