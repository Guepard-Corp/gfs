//! Table privilege metadata. Mirrors the datasource metadata schema.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TablePrivilegeType {
    Select,
    Insert,
    Update,
    Delete,
    Truncate,
    References,
    Trigger,
    Maintain,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TablePrivilegeGrant {
    pub grantor: String,
    pub grantee: String,
    pub privilege_type: TablePrivilegeType,
    pub is_grantable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TablePrivilegesKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
    PartitionedTable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TablePrivileges {
    pub relation_id: i64,
    pub schema: String,
    pub name: String,
    pub kind: TablePrivilegesKind,
    pub privileges: Vec<TablePrivilegeGrant>,
}
