//! Column privilege metadata. Mirrors the datasource metadata schema.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ColumnPrivilegeType {
    Select,
    Insert,
    Update,
    References,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnPrivilegeGrant {
    pub grantor: String,
    pub grantee: String,
    pub privilege_type: ColumnPrivilegeType,
    pub is_grantable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnPrivileges {
    pub column_id: String,
    pub relation_schema: String,
    pub relation_name: String,
    pub column_name: String,
    pub privileges: Vec<ColumnPrivilegeGrant>,
}
