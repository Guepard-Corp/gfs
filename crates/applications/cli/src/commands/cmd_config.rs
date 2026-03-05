//! `gfs config` — get or set repo-local or global config (user.name, user.email).
//!
//! Local values are stored in `.gfs/config.toml`.
//! Global values are stored in `~/.gfs/config.toml` and used as defaults
//! for every repository (similar to `~/.gitconfig`).
//!
//! Resolution order for commits: CLI flag → local → global → git config.

use std::path::PathBuf;

use anyhow::Result;
use gfs_domain::model::config::{GfsConfig, GlobalSettings};
use gfs_domain::model::errors::RepoError;
use gfs_domain::model::layout::GFS_DIR;
use gfs_domain::repo_utils::repo_layout;

use crate::cli_utils::get_repo_dir;

/// Supported config keys (git-style: user.name, user.email).
const KEY_USER_NAME: &str = "user.name";
const KEY_USER_EMAIL: &str = "user.email";
const KEY_TELEMETRY_ENABLED: &str = "telemetry.enabled";

/// Run `gfs config [--global] [--path <dir>] <key> [<value>]`.
/// - One argument (key): get; print value or nothing, exit 0.
/// - Two arguments (key, value): set; update config file, no output on success.
/// - `--global`: operate on `~/.gfs/config.toml` instead of `.gfs/config.toml`.
pub fn run(path: Option<PathBuf>, key: String, value: Option<String>, global: bool) -> Result<()> {
    if global {
        match value {
            None => get_global(&key),
            Some(v) => set_global(&key, &v),
        }
    } else {
        let repo_path = path.unwrap_or_else(get_repo_dir);
        match value {
            None => get(&repo_path, &key),
            Some(v) => set(&repo_path, &key, &v),
        }
    }
}

// ---------------------------------------------------------------------------
// Repo-local helpers
// ---------------------------------------------------------------------------

fn get(repo_path: &std::path::Path, key: &str) -> Result<()> {
    let config = match GfsConfig::load(repo_path) {
        Ok(c) => c,
        Err(e) => {
            return Err(repo_error_to_anyhow(e, repo_path));
        }
    };

    let out = match key {
        KEY_USER_NAME => config
            .user
            .as_ref()
            .and_then(|u| u.name.as_deref())
            .unwrap_or(""),
        KEY_USER_EMAIL => config
            .user
            .as_ref()
            .and_then(|u| u.email.as_deref())
            .unwrap_or(""),
        KEY_TELEMETRY_ENABLED => {
            anyhow::bail!(
                "'{}' is a global-only setting; use --global to read it",
                key
            );
        }
        _ => {
            anyhow::bail!(
                "unsupported config key '{}'; supported: {}, {}, {}",
                key,
                KEY_USER_NAME,
                KEY_USER_EMAIL,
                KEY_TELEMETRY_ENABLED
            );
        }
    };

    if !out.is_empty() {
        print!("{out}");
    }
    Ok(())
}

fn set(repo_path: &std::path::Path, key: &str, value: &str) -> Result<()> {
    if key == KEY_TELEMETRY_ENABLED {
        anyhow::bail!("'{}' is a global-only setting; use --global to set it", key);
    }
    if key != KEY_USER_NAME && key != KEY_USER_EMAIL {
        anyhow::bail!(
            "unsupported config key '{}'; supported: {}, {}, {}",
            key,
            KEY_USER_NAME,
            KEY_USER_EMAIL,
            KEY_TELEMETRY_ENABLED
        );
    }

    let gfs_dir = repo_path.join(GFS_DIR);
    if !gfs_dir.exists() {
        repo_layout::init_repo_layout(repo_path, None)
            .map_err(|e| repo_error_to_anyhow(e, repo_path))?;
    }

    let mut config = GfsConfig::load(repo_path).map_err(|e| repo_error_to_anyhow(e, repo_path))?;

    let mut user = config.user.clone().unwrap_or_default();
    match key {
        KEY_USER_NAME => user.name = Some(value.to_string()),
        KEY_USER_EMAIL => user.email = Some(value.to_string()),
        _ => unreachable!(),
    }
    config.user = Some(user);
    config
        .save(repo_path)
        .map_err(|e| repo_error_to_anyhow(e, repo_path))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Global (~/.gfs/config.toml) helpers
// ---------------------------------------------------------------------------

fn get_global(key: &str) -> Result<()> {
    let settings = GlobalSettings::load().unwrap_or_default();

    match key {
        KEY_USER_NAME => {
            let out = settings
                .user
                .as_ref()
                .and_then(|u| u.name.as_deref())
                .unwrap_or("");
            if !out.is_empty() {
                print!("{out}");
            }
        }
        KEY_USER_EMAIL => {
            let out = settings
                .user
                .as_ref()
                .and_then(|u| u.email.as_deref())
                .unwrap_or("");
            if !out.is_empty() {
                print!("{out}");
            }
        }
        KEY_TELEMETRY_ENABLED => {
            println!("{}", settings.telemetry);
        }
        _ => {
            anyhow::bail!(
                "unsupported config key '{}'; supported: {}, {}, {}",
                key,
                KEY_USER_NAME,
                KEY_USER_EMAIL,
                KEY_TELEMETRY_ENABLED
            );
        }
    }

    Ok(())
}

fn set_global(key: &str, value: &str) -> Result<()> {
    match key {
        KEY_USER_NAME | KEY_USER_EMAIL => {}
        KEY_TELEMETRY_ENABLED => {
            let enabled = match value {
                "true" => true,
                "false" => false,
                _ => anyhow::bail!(
                    "invalid value '{}' for '{}'; expected 'true' or 'false'",
                    value,
                    key
                ),
            };
            let mut settings = GlobalSettings::load().unwrap_or_default();
            settings.telemetry = enabled;
            settings.save().map_err(|e| anyhow::anyhow!("{}", e))?;
            return Ok(());
        }
        _ => {
            anyhow::bail!(
                "unsupported config key '{}'; supported: {}, {}, {}",
                key,
                KEY_USER_NAME,
                KEY_USER_EMAIL,
                KEY_TELEMETRY_ENABLED
            );
        }
    }

    let mut settings = GlobalSettings::load().unwrap_or_default();
    let mut user = settings.user.clone().unwrap_or_default();
    match key {
        KEY_USER_NAME => user.name = Some(value.to_string()),
        KEY_USER_EMAIL => user.email = Some(value.to_string()),
        _ => unreachable!(),
    }
    settings.user = Some(user);
    settings.save().map_err(|e| anyhow::anyhow!("{}", e))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn repo_error_to_anyhow(e: RepoError, repo_path: &std::path::Path) -> anyhow::Error {
    match &e {
        RepoError::NoRepoFound(_) | RepoError::MissingFile(_) | RepoError::IoError(_) => {
            anyhow::anyhow!("not a gfs repository: {}", repo_path.display())
        }
        _ => anyhow::anyhow!("{}", e),
    }
}
