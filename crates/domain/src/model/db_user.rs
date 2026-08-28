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
    /// The role whose FUTURE objects the preset's `ALTER DEFAULT PRIVILEGES`
    /// should cover — the customer's object-creating role (`owner`) in a deploy,
    /// so a preset user sees the tables the customer creates later. `None`
    /// (single-node, no deploy owner) covers the connecting role's future objects.
    pub default_privileges_owner: Option<String>,
}

/// A role as read back from the engine's catalog (the `list` projection).
/// Never carries a password — the engine keeps only a hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleInfo {
    pub username: String,
    pub can_login: bool,
    pub is_superuser: bool,
    /// The applied preset (`readonly`/`readwrite`/`admin`), recorded in the
    /// role's comment at create/apply time. `None` when no preset was set.
    #[serde(default)]
    pub preset: Option<String>,
}

/// Everything needed to bootstrap a database's deploy environment: a
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

/// A single object-level privilege that can be granted on / revoked from a role.
///
/// Serialises as the lowercase SQL keyword (`select`, `insert`, …); `All` maps
/// to `ALL PRIVILEGES`. The set valid for a given object type is constrained by
/// [`Privilege::is_valid_for`] — the allow-list that keeps untrusted input from
/// being spliced into a `GRANT`/`REVOKE` statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Privilege {
    Select,
    Insert,
    Update,
    Delete,
    Truncate,
    References,
    Trigger,
    Usage,
    Create,
    Connect,
    Temporary,
    /// `ALL PRIVILEGES` — valid on every object type.
    All,
}

impl Privilege {
    /// Parse from a CLI/tool string; `None` if unknown.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "select" => Some(Self::Select),
            "insert" => Some(Self::Insert),
            "update" => Some(Self::Update),
            "delete" => Some(Self::Delete),
            "truncate" => Some(Self::Truncate),
            "references" => Some(Self::References),
            "trigger" => Some(Self::Trigger),
            "usage" => Some(Self::Usage),
            "create" => Some(Self::Create),
            "connect" => Some(Self::Connect),
            "temporary" => Some(Self::Temporary),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// The canonical wire string (lowercase).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Select => "select",
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Truncate => "truncate",
            Self::References => "references",
            Self::Trigger => "trigger",
            Self::Usage => "usage",
            Self::Create => "create",
            Self::Connect => "connect",
            Self::Temporary => "temporary",
            Self::All => "all",
        }
    }

    /// The allow-list matrix: whether `self` may be applied to `object`.
    ///
    /// `All` is valid on every object type; the others follow the PostgreSQL
    /// per-object-class privilege vocabulary. Callers MUST reject an invalid
    /// combination before building SQL — this is the injection-safety property
    /// (an unknown/invalid privilege never reaches a statement).
    pub fn is_valid_for(self, object: &GrantableObject) -> bool {
        use Privilege::*;
        if matches!(self, All) {
            return true;
        }
        match object {
            GrantableObject::Database => matches!(self, Connect | Create | Temporary),
            GrantableObject::Schema { .. } => matches!(self, Usage | Create),
            GrantableObject::Table { .. } | GrantableObject::AllTablesInSchema { .. } => {
                matches!(
                    self,
                    Select | Insert | Update | Delete | Truncate | References | Trigger
                )
            }
            GrantableObject::Sequence { .. } | GrantableObject::AllSequencesInSchema { .. } => {
                matches!(self, Usage | Select | Update)
            }
        }
    }
}

/// A grantable / revocable database object.
///
/// Identifiers (`schema`, `name`) are validated + quoted by the provider
/// implementation. [`GrantableObject::Database`] carries no name — the provider
/// resolves it to the instance's own database, so there is no caller-named
/// cross-database grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GrantableObject {
    /// The instance's own database (resolved provider-side).
    Database,
    /// A schema.
    Schema { schema: String },
    /// A single table — or view (PostgreSQL grants table privileges on views).
    Table { schema: String, name: String },
    /// Every existing table in a schema.
    AllTablesInSchema { schema: String },
    /// A single sequence.
    Sequence { schema: String, name: String },
    /// Every existing sequence in a schema.
    AllSequencesInSchema { schema: String },
}

/// Everything needed to grant privileges on an object to a role.
#[derive(Debug, Clone)]
pub struct GrantSpec {
    /// The grantee role (validated identifier).
    pub role: String,
    /// The object the privileges apply to.
    pub object: GrantableObject,
    /// The privileges to grant (each must be [`Privilege::is_valid_for`] the object).
    pub privileges: Vec<Privilege>,
    /// Append `WITH GRANT OPTION` — lets the grantee re-grant what it holds.
    pub with_grant_option: bool,
    /// When `Some(grantor)`, also emit role-scoped `ALTER DEFAULT PRIVILEGES FOR
    /// ROLE <grantor>` so future objects the grantor creates flow to the role.
    /// Valid only for the all-in-schema object variants.
    pub apply_to_future: Option<String>,
}

/// Everything needed to revoke privileges on an object from a role.
#[derive(Debug, Clone)]
pub struct RevokeSpec {
    /// The role to revoke from (validated identifier).
    pub role: String,
    /// The object the privileges apply to.
    pub object: GrantableObject,
    /// The privileges to revoke.
    pub privileges: Vec<Privilege>,
    /// Use `CASCADE` (also revoke dependent grants) instead of the default `RESTRICT`.
    pub cascade: bool,
}

/// A single effective object privilege, as read back from the engine catalog
/// (the `list-privs` projection). Never carries a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectPrivilege {
    /// The object class (`table`, `sequence`, `schema`, `database`).
    pub object_type: String,
    /// The (schema-qualified where applicable) object name.
    pub object_name: String,
    /// The granted privilege (lowercase).
    pub privilege: String,
    /// Whether the grantee may re-grant it (`WITH GRANT OPTION`).
    pub grantable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_objects() -> Vec<GrantableObject> {
        vec![
            GrantableObject::Database,
            GrantableObject::Schema { schema: "s".into() },
            GrantableObject::Table {
                schema: "s".into(),
                name: "t".into(),
            },
            GrantableObject::AllTablesInSchema { schema: "s".into() },
            GrantableObject::Sequence {
                schema: "s".into(),
                name: "q".into(),
            },
            GrantableObject::AllSequencesInSchema { schema: "s".into() },
        ]
    }

    #[test]
    fn all_privilege_is_valid_for_every_object() {
        for object in all_objects() {
            assert!(
                Privilege::All.is_valid_for(&object),
                "ALL must be valid for {object:?}"
            );
        }
    }

    #[test]
    fn database_privilege_matrix() {
        let db = GrantableObject::Database;
        for p in [Privilege::Connect, Privilege::Create, Privilege::Temporary] {
            assert!(p.is_valid_for(&db), "{p:?} valid on database");
        }
        for p in [
            Privilege::Select,
            Privilege::Insert,
            Privilege::Usage,
            Privilege::Trigger,
        ] {
            assert!(!p.is_valid_for(&db), "{p:?} invalid on database");
        }
    }

    #[test]
    fn schema_privilege_matrix() {
        let s = GrantableObject::Schema {
            schema: "public".into(),
        };
        for p in [Privilege::Usage, Privilege::Create] {
            assert!(p.is_valid_for(&s), "{p:?} valid on schema");
        }
        for p in [Privilege::Select, Privilege::Connect, Privilege::Insert] {
            assert!(!p.is_valid_for(&s), "{p:?} invalid on schema");
        }
    }

    #[test]
    fn table_privilege_matrix() {
        for t in [
            GrantableObject::Table {
                schema: "s".into(),
                name: "t".into(),
            },
            GrantableObject::AllTablesInSchema { schema: "s".into() },
        ] {
            for p in [
                Privilege::Select,
                Privilege::Insert,
                Privilege::Update,
                Privilege::Delete,
                Privilege::Truncate,
                Privilege::References,
                Privilege::Trigger,
            ] {
                assert!(p.is_valid_for(&t), "{p:?} valid on {t:?}");
            }
            for p in [
                Privilege::Usage,
                Privilege::Connect,
                Privilege::Create,
                Privilege::Temporary,
            ] {
                assert!(!p.is_valid_for(&t), "{p:?} invalid on {t:?}");
            }
        }
    }

    #[test]
    fn sequence_privilege_matrix() {
        for q in [
            GrantableObject::Sequence {
                schema: "s".into(),
                name: "q".into(),
            },
            GrantableObject::AllSequencesInSchema { schema: "s".into() },
        ] {
            for p in [Privilege::Usage, Privilege::Select, Privilege::Update] {
                assert!(p.is_valid_for(&q), "{p:?} valid on {q:?}");
            }
            for p in [
                Privilege::Insert,
                Privilege::Delete,
                Privilege::Create,
                Privilege::Trigger,
            ] {
                assert!(!p.is_valid_for(&q), "{p:?} invalid on {q:?}");
            }
        }
    }

    #[test]
    fn privilege_parse_as_str_roundtrip() {
        for p in [
            Privilege::Select,
            Privilege::Insert,
            Privilege::Update,
            Privilege::Delete,
            Privilege::Truncate,
            Privilege::References,
            Privilege::Trigger,
            Privilege::Usage,
            Privilege::Create,
            Privilege::Connect,
            Privilege::Temporary,
            Privilege::All,
        ] {
            assert_eq!(Privilege::parse(p.as_str()), Some(p));
        }
        assert_eq!(Privilege::parse("bogus"), None);
    }

    #[test]
    fn privilege_serde_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&Privilege::Select).unwrap(),
            "\"select\""
        );
        assert_eq!(
            serde_json::from_str::<Privilege>("\"all\"").unwrap(),
            Privilege::All
        );
    }

    #[test]
    fn grantable_object_serde_is_tagged() {
        let object = GrantableObject::Table {
            schema: "public".into(),
            name: "orders".into(),
        };
        let json = serde_json::to_string(&object).unwrap();
        assert_eq!(
            json,
            r#"{"type":"table","schema":"public","name":"orders"}"#
        );
        assert_eq!(
            serde_json::from_str::<GrantableObject>(&json).unwrap(),
            object
        );
    }
}
