use std::path::PathBuf;

use anyhow::{Context, Result};
use gfs_console_remote::{auth_from_env, block_direct_kubernetes_env, ConsoleClient};
use gfs_domain::model::config::{
    EnvironmentConfig, GfsConfig, RemoteConfig, RuntimeConfig, UserConfig,
};
use gfs_domain::model::layout::GFS_DIR;
use gfs_domain::usecases::repository::init_repo_usecase::DatabaseCredentials;
use serde_json::json;

use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, dimmed, green};

pub async fn init_remote(
    path: Option<PathBuf>,
    database_provider: Option<String>,
    database_version: Option<String>,
    engine_node_id: Option<String>,
    name: Option<String>,
    credentials: DatabaseCredentials,
    json_output: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    block_direct_kubernetes_env().map_err(|e| std::io::Error::other(e.to_string()))?;

    let provider = database_provider
        .as_deref()
        .context("remote init requires --database-provider")?;
    let version = database_version
        .as_deref()
        .context("remote init requires --database-version")?;
    let node_id = engine_node_id
        .or_else(|| std::env::var("GUEPARD_ENGINE_NODE_ID").ok())
        .filter(|s| !s.trim().is_empty())
        .context("set --remote-node or GUEPARD_ENGINE_NODE_ID")?;

    let console_url = std::env::var("GUEPARD_CONSOLE_URL")
        .context("GUEPARD_CONSOLE_URL is required for remote init")?;
    let auth = auth_from_env().map_err(|e| std::io::Error::other(e.to_string()))?;
    let client = ConsoleClient::new(console_url.clone(), auth)
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    let deploy = client
        .deploy_database(&node_id, provider, version, name.as_deref(), "default")
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    let database_id = deploy
        .deployment
        .get("id")
        .and_then(|v| v.as_str())
        .context("deployment id missing in console response")
        .map_err(|e| std::io::Error::other(e.to_string()))?
        .to_string();

    let target_path = path.unwrap_or_else(get_repo_dir);
    let gfs_dir = target_path.join(GFS_DIR);
    std::fs::create_dir_all(gfs_dir.join("refs/heads"))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::create_dir_all(gfs_dir.join("commits"))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::write(gfs_dir.join("HEAD"), "ref: refs/heads/main\n")
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    let config = GfsConfig {
        mount_point: None,
        version: "1".into(),
        description: "guepard remote".into(),
        user: Some(UserConfig {
            name: credentials.user,
            email: None,
        }),
        environment: Some(EnvironmentConfig {
            database_provider: provider.to_string(),
            database_version: version.to_string(),
            database_port: None,
        }),
        runtime: Some(RuntimeConfig {
            runtime_provider: "guepard".into(),
            runtime_version: "1".into(),
            container_name: database_id.clone(),
        }),
        storage: None,
        remote: Some(RemoteConfig {
            console_url,
            node_id,
            database_id: database_id.clone(),
            project: "default".into(),
        }),
    };
    config
        .save(&target_path)
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "path": target_path.display().to_string(),
                "database_id": database_id,
                "engine": deploy.engine,
                "remote": true,
            }))?
        );
    } else {
        println!(
            "  {} Remote GFS repo at {} (database {})",
            green("✓"),
            cyan(target_path.display().to_string()),
            cyan(&database_id)
        );
        println!("    {:<16} {}", dimmed("Console"), config.remote.as_ref().map(|r| r.console_url.as_str()).unwrap_or(""));
        println!("    {:<16} {}", dimmed("Node"), config.remote.as_ref().map(|r| r.node_id.as_str()).unwrap_or(""));
    }
    Ok(())
}
