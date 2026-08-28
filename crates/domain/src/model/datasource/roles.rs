//! Role metadata. Mirrors the datasource metadata schema.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub id: i64,
    pub name: String,
    #[serde(rename = "isSuperuser")]
    pub is_superuser: bool,
    #[serde(rename = "canCreateDb")]
    pub can_create_db: bool,
    #[serde(rename = "canCreateRole")]
    pub can_create_role: bool,
    #[serde(rename = "inheritRole")]
    pub inherit_role: bool,
    #[serde(rename = "canLogin")]
    pub can_login: bool,
    #[serde(rename = "isReplicationRole")]
    pub is_replication_role: bool,
    #[serde(rename = "canBypassRls")]
    pub can_bypass_rls: bool,
    #[serde(rename = "activeConnections")]
    pub active_connections: i64,
    #[serde(rename = "connectionLimit")]
    pub connection_limit: i64,
    #[serde(rename = "validUntil")]
    pub valid_until: Option<String>,
    pub config: std::collections::HashMap<String, String>,
}
