//! `gfs user` — manage database users/roles (create, list, drop, set-password).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_db_providers as containers;
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::model::config::GfsConfig;
use gfs_domain::model::db_user::{
    GrantSpec, GrantableObject, Privilege, RevokeSpec, RolePreset, RoleSpec,
};
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::ports::repository::Repository;
use gfs_domain::usecases::repository::manage_users_usecase::ManageUsersUseCase;

use super::compute_support::compute_for_repo;
use crate::cli_utils::get_repo_dir;

/// Build a `ManageUsersUseCase` wired to the repo's compute + provider registry
/// (mirrors `cmd_query`'s composition root).
async fn build_use_case(
    repo_path: &Path,
) -> Result<ManageUsersUseCase<InMemoryDatabaseProviderRegistry>> {
    GfsConfig::load(repo_path).context("not a GFS repository (run gfs init first)")?;
    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute = compute_for_repo(&repository, repo_path).await?;
    let registry = InMemoryDatabaseProviderRegistry::new();
    containers::register_all(&registry).context("failed to register database providers")?;
    Ok(ManageUsersUseCase::new(compute, Arc::new(registry)))
}

fn parse_preset(preset: Option<String>) -> Result<Option<RolePreset>> {
    match preset {
        Some(p) => RolePreset::parse(&p)
            .map(Some)
            .with_context(|| format!("unknown preset '{p}' (expected readonly|readwrite|admin)")),
        None => Ok(None),
    }
}

/// A random password (uuid v4, 122 bits) used when the caller supplies none.
fn generate_password() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn print_credential(username: &str, password: &str, generated: bool, json_output: bool) {
    if json_output {
        // Machine output keeps a stable shape; the caller opts into it and owns
        // redaction of a password they themselves supplied.
        println!(
            "{}",
            serde_json::json!({ "username": username, "password": password })
        );
    } else if generated {
        // Only the server-generated secret is the "shown once" copy worth echoing.
        println!("user '{username}' — password (shown once): {password}");
    } else {
        // A caller-supplied password is not re-echoed to the terminal/logs.
        println!("user '{username}' — password set");
    }
}

pub async fn run_create(
    path: Option<PathBuf>,
    username: String,
    preset: Option<String>,
    password: Option<String>,
    json_output: bool,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let preset = parse_preset(preset)?;
    let generated = password.is_none();
    let password = password.unwrap_or_else(generate_password);
    let use_case = build_use_case(&repo_path).await?;
    // A preset's default privileges must cover the customer's FUTURE tables, which
    // the deploy `owner` role creates — not the connecting admin. Auto-detect the
    // deploy owner so a readonly/readwrite user isn't blind to owner's later tables
    // (None on single-node repos with no `owner` role → connecting-role-scoped).
    let default_privileges_owner = if preset.is_some() {
        use_case.detect_deploy_owner(&repo_path).await
    } else {
        None
    };
    use_case
        .create_role(
            &repo_path,
            &RoleSpec {
                username: username.clone(),
                password: password.clone(),
                preset,
                default_privileges_owner,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    print_credential(&username, &password, generated, json_output);
    Ok(())
}

pub async fn run_list(path: Option<PathBuf>, json_output: bool) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let use_case = build_use_case(&repo_path).await?;
    let roles = use_case
        .list_roles(&repo_path)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if json_output {
        println!("{}", serde_json::to_string(&roles)?);
    } else if roles.is_empty() {
        println!("no database users");
    } else {
        for role in &roles {
            println!(
                "{:<32} login={} superuser={}",
                role.username, role.can_login, role.is_superuser
            );
        }
    }
    Ok(())
}

pub async fn run_drop(path: Option<PathBuf>, username: String, json_output: bool) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let use_case = build_use_case(&repo_path).await?;
    use_case
        .drop_role(&repo_path, &username)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if json_output {
        println!(
            "{}",
            serde_json::json!({ "username": username, "dropped": true })
        );
    } else {
        println!("dropped user '{username}'");
    }
    Ok(())
}

pub async fn run_set_password(
    path: Option<PathBuf>,
    username: String,
    password: Option<String>,
    json_output: bool,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let generated = password.is_none();
    let password = password.unwrap_or_else(generate_password);
    let use_case = build_use_case(&repo_path).await?;
    use_case
        .set_password(&repo_path, &username, &password)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    print_credential(&username, &password, generated, json_output);
    Ok(())
}

pub async fn run_apply_preset(
    path: Option<PathBuf>,
    username: String,
    preset: String,
    json_output: bool,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let preset = parse_preset(Some(preset))?
        .ok_or_else(|| anyhow::anyhow!("a preset is required (readonly|readwrite|admin)"))?;
    let use_case = build_use_case(&repo_path).await?;
    // Scope the preset's default privileges to the deploy owner's future objects
    // (same as create) so a re-applied/changed preset stays owner-aware.
    let default_privileges_owner = use_case.detect_deploy_owner(&repo_path).await;
    use_case
        .apply_preset(
            &repo_path,
            &username,
            preset,
            default_privileges_owner.as_deref(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if json_output {
        println!(
            "{}",
            serde_json::json!({ "username": username, "preset_applied": true })
        );
    } else {
        println!("user '{username}' — preset applied");
    }
    Ok(())
}

/// The mutually-exclusive `--on-*` object flags for grant/revoke.
pub struct ObjectFlags {
    pub on_database: bool,
    pub on_schema: Option<String>,
    pub on_table: Option<String>,
    pub on_all_tables_in_schema: Option<String>,
    pub on_sequence: Option<String>,
    pub on_all_sequences_in_schema: Option<String>,
}

/// Split a `schema.name` argument; both parts must be non-empty.
fn split_qualified(value: &str) -> Result<(String, String)> {
    match value.split_once('.') {
        Some((schema, name)) if !schema.is_empty() && !name.is_empty() => {
            Ok((schema.to_string(), name.to_string()))
        }
        _ => anyhow::bail!("expected schema.name (got '{value}')"),
    }
}

/// Resolve exactly one `--on-*` flag into a [`GrantableObject`].
fn parse_object(flags: &ObjectFlags) -> Result<GrantableObject> {
    let mut chosen: Vec<GrantableObject> = Vec::new();
    if flags.on_database {
        chosen.push(GrantableObject::Database);
    }
    if let Some(schema) = &flags.on_schema {
        chosen.push(GrantableObject::Schema {
            schema: schema.clone(),
        });
    }
    if let Some(value) = &flags.on_table {
        let (schema, name) = split_qualified(value)?;
        chosen.push(GrantableObject::Table { schema, name });
    }
    if let Some(schema) = &flags.on_all_tables_in_schema {
        chosen.push(GrantableObject::AllTablesInSchema {
            schema: schema.clone(),
        });
    }
    if let Some(value) = &flags.on_sequence {
        let (schema, name) = split_qualified(value)?;
        chosen.push(GrantableObject::Sequence { schema, name });
    }
    if let Some(schema) = &flags.on_all_sequences_in_schema {
        chosen.push(GrantableObject::AllSequencesInSchema {
            schema: schema.clone(),
        });
    }
    match chosen.len() {
        1 => Ok(chosen.into_iter().next().expect("len checked")),
        0 => anyhow::bail!(
            "specify one object: --on-database | --on-schema <s> | --on-table <s.t> | \
             --on-all-tables-in-schema <s> | --on-sequence <s.q> | --on-all-sequences-in-schema <s>"
        ),
        _ => anyhow::bail!("specify exactly one object flag, not multiple"),
    }
}

/// Parse a comma-separated privilege list (case-insensitive), e.g. `SELECT,INSERT`.
fn parse_privileges(csv: &str) -> Result<Vec<Privilege>> {
    let privileges = csv
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            Privilege::parse(&p.to_lowercase()).with_context(|| format!("unknown privilege '{p}'"))
        })
        .collect::<Result<Vec<_>>>()?;
    if privileges.is_empty() {
        anyhow::bail!("at least one privilege is required (e.g. --privileges SELECT,INSERT)");
    }
    Ok(privileges)
}

pub async fn run_grant(
    path: Option<PathBuf>,
    username: String,
    object: ObjectFlags,
    privileges: String,
    with_grant_option: bool,
    apply_to_future: Option<String>,
    json_output: bool,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let object = parse_object(&object)?;
    let privileges = parse_privileges(&privileges)?;
    let use_case = build_use_case(&repo_path).await?;
    use_case
        .grant(
            &repo_path,
            &GrantSpec {
                role: username.clone(),
                object,
                privileges,
                with_grant_option,
                apply_to_future,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if json_output {
        println!(
            "{}",
            serde_json::json!({ "username": username, "granted": true })
        );
    } else {
        println!("granted privileges to '{username}'");
    }
    Ok(())
}

pub async fn run_revoke(
    path: Option<PathBuf>,
    username: String,
    object: ObjectFlags,
    privileges: String,
    cascade: bool,
    json_output: bool,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let object = parse_object(&object)?;
    let privileges = parse_privileges(&privileges)?;
    let use_case = build_use_case(&repo_path).await?;
    use_case
        .revoke(
            &repo_path,
            &RevokeSpec {
                role: username.clone(),
                object,
                privileges,
                cascade,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if json_output {
        println!(
            "{}",
            serde_json::json!({ "username": username, "revoked": true })
        );
    } else {
        println!("revoked privileges from '{username}'");
    }
    Ok(())
}

pub async fn run_list_privs(
    path: Option<PathBuf>,
    username: String,
    json_output: bool,
) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let use_case = build_use_case(&repo_path).await?;
    let privileges = use_case
        .list_privileges(&repo_path, &username)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if json_output {
        println!("{}", serde_json::to_string(&privileges)?);
    } else if privileges.is_empty() {
        println!("no privileges for '{username}'");
    } else {
        for p in &privileges {
            println!(
                "{:<10} {:<40} {:<12} grantable={}",
                p.object_type, p.object_name, p.privilege, p.grantable
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags() -> ObjectFlags {
        ObjectFlags {
            on_database: false,
            on_schema: None,
            on_table: None,
            on_all_tables_in_schema: None,
            on_sequence: None,
            on_all_sequences_in_schema: None,
        }
    }

    #[test]
    fn parse_object_each_variant() {
        assert_eq!(
            parse_object(&ObjectFlags {
                on_database: true,
                ..flags()
            })
            .unwrap(),
            GrantableObject::Database
        );
        assert_eq!(
            parse_object(&ObjectFlags {
                on_schema: Some("public".into()),
                ..flags()
            })
            .unwrap(),
            GrantableObject::Schema {
                schema: "public".into()
            }
        );
        assert_eq!(
            parse_object(&ObjectFlags {
                on_table: Some("public.orders".into()),
                ..flags()
            })
            .unwrap(),
            GrantableObject::Table {
                schema: "public".into(),
                name: "orders".into()
            }
        );
        assert_eq!(
            parse_object(&ObjectFlags {
                on_all_tables_in_schema: Some("public".into()),
                ..flags()
            })
            .unwrap(),
            GrantableObject::AllTablesInSchema {
                schema: "public".into()
            }
        );
        assert_eq!(
            parse_object(&ObjectFlags {
                on_sequence: Some("public.seq".into()),
                ..flags()
            })
            .unwrap(),
            GrantableObject::Sequence {
                schema: "public".into(),
                name: "seq".into()
            }
        );
        assert_eq!(
            parse_object(&ObjectFlags {
                on_all_sequences_in_schema: Some("public".into()),
                ..flags()
            })
            .unwrap(),
            GrantableObject::AllSequencesInSchema {
                schema: "public".into()
            }
        );
    }

    #[test]
    fn parse_object_requires_exactly_one() {
        assert!(parse_object(&flags()).is_err(), "none set must error");
        assert!(
            parse_object(&ObjectFlags {
                on_database: true,
                on_schema: Some("public".into()),
                ..flags()
            })
            .is_err(),
            "multiple set must error"
        );
    }

    #[test]
    fn parse_object_rejects_unqualified_table() {
        assert!(
            parse_object(&ObjectFlags {
                on_table: Some("orders".into()),
                ..flags()
            })
            .is_err(),
            "table without schema. prefix must error"
        );
    }

    #[test]
    fn parse_privileges_case_insensitive_and_rejects_unknown() {
        assert_eq!(
            parse_privileges("SELECT, insert ,Update").unwrap(),
            vec![Privilege::Select, Privilege::Insert, Privilege::Update]
        );
        assert_eq!(parse_privileges("ALL").unwrap(), vec![Privilege::All]);
        assert!(parse_privileges("bogus").is_err());
        assert!(parse_privileges("").is_err(), "empty must error");
    }
}
