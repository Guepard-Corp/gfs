//! Extension metadata (e.g. Postgres extensions). Mirrors the datasource metadata schema.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasourceExtension {
    pub name: String,
    pub schema: Option<String>,
    pub default_version: String,
    pub installed_version: Option<String>,
    pub comment: String,
}
