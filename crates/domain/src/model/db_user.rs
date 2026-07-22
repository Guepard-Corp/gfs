//! Database-user / role model for the `gfs user` capability.
//!
//! A managed database user is a login role inside the running database. These
//! are pure value types; the SQL that creates/alters them is provider-specific
//! (see [`crate::ports::database_provider::DatabaseProvider`]).

use serde::{Deserialize, Serialize};

/// A curated, allow-listed privilege bundle applied to a role.
///
/// Serialises as the lowercase strings `readonly` / `readwrite` / `admin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolePreset {
    /// `CONNECT` + schema `USAGE` + `SELECT` on tables.
    Readonly,
    /// `readonly` plus `INSERT` / `UPDATE` / `DELETE` and sequence usage.
    Readwrite,
    /// Owner-grade on the application schema (DDL + full DML). Never a superuser.
    Admin,
}

impl RolePreset {
    /// Parse from a CLI/tool string; `None` if unknown.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "readonly" => Some(Self::Readonly),
            "readwrite" => Some(Self::Readwrite),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    /// The canonical wire string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Readonly => "readonly",
            Self::Readwrite => "readwrite",
            Self::Admin => "admin",
        }
    }
}

/// Everything needed to create a login role.
#[derive(Debug, Clone)]
pub struct RoleSpec {
    pub username: String,
    pub password: String,
    /// Optional preset to apply at create time.
    pub preset: Option<RolePreset>,
}

/// A role as read back from the engine's catalog (the `list` projection).
/// Never carries a password — the engine keeps only a hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleInfo {
    pub username: String,
    pub can_login: bool,
    pub is_superuser: bool,
}

/// Everything needed to bootstrap a database's deploy environment (RFC 009): a
/// `NOLOGIN` group carrying the shared CRUD baseline, an `owner` login (the
/// least-privileged customer role that keeps `public`), the owner's membership
/// in the group, and role-scoped default privileges so future owner objects
/// flow to the group. Tenancy-free — the caller supplies validated names.
#[derive(Debug, Clone)]
pub struct DeployEnvSpec {
    /// The customer's default login role (`LOGIN NOSUPERUSER`, never the DB owner).
    pub owner: String,
    /// The owner's password (SCRAM-hashed by the engine; never logged).
    pub owner_password: String,
    /// The `NOLOGIN` group role carrying the shared CRUD baseline (e.g. `developers`).
    pub group: String,
    /// The database the owner is granted `CONNECT` on.
    pub database: String,
}
