use std::path::Path;

use anyhow::{Context, Result};
use gfs_console_remote::{auth_from_env, block_direct_kubernetes_env, resolve_console_url, ConsoleClient};
use gfs_domain::model::config::{GfsConfig, RemoteConfig};

pub fn is_remote_repo(repo_path: &Path) -> Result<bool> {
    match GfsConfig::load(repo_path) {
        Ok(cfg) => Ok(cfg.is_guepard_remote()),
        Err(_) => Ok(false),
    }
}

pub fn require_remote_config(repo_path: &Path) -> Result<(GfsConfig, RemoteConfig)> {
    let cfg = GfsConfig::load(repo_path).context("load .gfs/config.toml")?;
    let remote = cfg
        .remote
        .clone()
        .context("repository is not a guepard remote repo (missing [remote] in config.toml)")?;
    block_direct_kubernetes_env()?;
    Ok((cfg, remote))
}

pub fn console_client_for_repo(repo_path: &Path) -> Result<ConsoleClient> {
    let (_cfg, remote) = require_remote_config(repo_path)?;
    let auth = auth_from_env()?;
    let base = resolve_console_url().unwrap_or(remote.console_url);
    ConsoleClient::new(base, auth)
}
