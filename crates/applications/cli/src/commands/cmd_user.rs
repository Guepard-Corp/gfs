//! `gfs user` — manage database users/roles (create, list, drop, set-password).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use gfs_compute_docker::containers;
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::model::config::GfsConfig;
use gfs_domain::model::db_user::{RolePreset, RoleSpec};
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
    use_case
        .create_role(
            &repo_path,
            &RoleSpec {
                username: username.clone(),
                password: password.clone(),
                preset,
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
