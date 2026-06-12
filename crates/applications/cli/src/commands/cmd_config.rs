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

/// Supported config keys (git-style: user.name, user.email, storage.compression, storage.reflink).
const KEY_USER_NAME: &str = "user.name";
const KEY_USER_EMAIL: &str = "user.email";
const KEY_STORAGE_COMPRESSION: &str = "storage.compression";
const KEY_STORAGE_REFLINK: &str = "storage.reflink";
const KEY_TELEMETRY_ENABLED: &str = "telemetry.enabled";
const KEY_REMOTE_CONSOLE_URL: &str = "remote.console_url";
const KEY_REMOTE_SUPABASE_URL: &str = "remote.supabase_url";
const KEY_REMOTE_SUPABASE_ANON_KEY: &str = "remote.supabase_anon_key";

const SUPPORTED_KEYS: &[&str] = &[
    KEY_USER_NAME,
    KEY_USER_EMAIL,
    KEY_STORAGE_COMPRESSION,
    KEY_STORAGE_REFLINK,
    KEY_TELEMETRY_ENABLED,
    KEY_REMOTE_CONSOLE_URL,
    KEY_REMOTE_SUPABASE_URL,
    KEY_REMOTE_SUPABASE_ANON_KEY,
];

const REMOTE_GLOBAL_KEYS: &[&str] = &[
    KEY_REMOTE_CONSOLE_URL,
    KEY_REMOTE_SUPABASE_URL,
    KEY_REMOTE_SUPABASE_ANON_KEY,
];

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

/// Prefix for open-ended database container parameter overrides, stored under
/// `[compute.params]` and rendered to provider-native flags at provision time.
const COMPUTE_PARAM_PREFIX: &str = "compute.params.";

fn get(repo_path: &std::path::Path, key: &str) -> Result<()> {
    let config = match GfsConfig::load(repo_path) {
        Ok(c) => c,
        Err(e) => {
            return Err(repo_error_to_anyhow(e, repo_path));
        }
    };

    if let Some(name) = key.strip_prefix(COMPUTE_PARAM_PREFIX) {
        if let Some(v) = config.compute.as_ref().and_then(|c| c.params.get(name)) {
            print!("{v}");
        }
        return Ok(());
    }

    let out: String = match key {
        KEY_USER_NAME => config
            .user
            .as_ref()
            .and_then(|u| u.name.as_deref())
            .map(|s| s.to_string())
            .unwrap_or_default(),
        KEY_USER_EMAIL => config
            .user
            .as_ref()
            .and_then(|u| u.email.as_deref())
            .map(|s| s.to_string())
            .unwrap_or_default(),
        KEY_STORAGE_COMPRESSION => config
            .storage
            .as_ref()
            .and_then(|s| s.compression.as_deref())
            .map(|s| s.to_string())
            .unwrap_or_default(),
        KEY_STORAGE_REFLINK => config
            .storage
            .as_ref()
            .map(|s| s.enable_reflink.to_string())
            .unwrap_or_default(),
        KEY_TELEMETRY_ENABLED | KEY_REMOTE_CONSOLE_URL | KEY_REMOTE_SUPABASE_URL
        | KEY_REMOTE_SUPABASE_ANON_KEY => {
            anyhow::bail!(
                "'{}' is a global-only setting; use --global to read it",
                key
            );
        }
        _ => {
            anyhow::bail!(
                "unsupported config key '{}'; supported: {:?}",
                key,
                SUPPORTED_KEYS
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

    let compute_param = key.strip_prefix(COMPUTE_PARAM_PREFIX);
    if compute_param.is_none() && !SUPPORTED_KEYS.contains(&key) {
        anyhow::bail!(
            "unsupported config key '{}'; supported: {:?} (or '{}<name>')",
            key,
            SUPPORTED_KEYS,
            COMPUTE_PARAM_PREFIX,
        );
    }

    let gfs_dir = repo_path.join(GFS_DIR);
    if !gfs_dir.exists() {
        repo_layout::init_repo_layout(repo_path, None)
            .map_err(|e| repo_error_to_anyhow(e, repo_path))?;
    }

    let mut config = GfsConfig::load(repo_path).map_err(|e| repo_error_to_anyhow(e, repo_path))?;

    if let Some(name) = compute_param {
        if name.is_empty() {
            anyhow::bail!("missing parameter name after '{}'", COMPUTE_PARAM_PREFIX);
        }
        let mut compute = config.compute.clone().unwrap_or_default();
        compute.params.insert(name.to_string(), value.to_string());
        config.compute = Some(compute);
        config
            .save(repo_path)
            .map_err(|e| repo_error_to_anyhow(e, repo_path))?;
        return Ok(());
    }

    match key {
        KEY_USER_NAME => {
            let mut user = config.user.clone().unwrap_or_default();
            user.name = Some(value.to_string());
            config.user = Some(user);
        }
        KEY_USER_EMAIL => {
            let mut user = config.user.clone().unwrap_or_default();
            user.email = Some(value.to_string());
            config.user = Some(user);
        }
        KEY_STORAGE_COMPRESSION => {
            let mut storage = config.storage.clone().unwrap_or_default();
            storage.compression = Some(value.to_string());
            config.storage = Some(storage);
        }
        KEY_STORAGE_REFLINK => {
            let mut storage = config.storage.clone().unwrap_or_default();
            storage.enable_reflink = value == "true" || value == "1";
            config.storage = Some(storage);
        }
        _ => unreachable!(),
    }
    config
        .save(repo_path)
        .map_err(|e| repo_error_to_anyhow(e, repo_path))?;

    if let Some(storage) = config.storage.as_ref() {
        apply_storage_settings(key, &gfs_dir, storage);
    }

    Ok(())
}

fn apply_storage_settings(
    key: &str,
    gfs_dir: &std::path::Path,
    storage: &gfs_domain::model::config::StorageConfig,
) {
    let should_apply = match key {
        KEY_STORAGE_COMPRESSION => storage.compression.is_some(),
        KEY_STORAGE_REFLINK => storage.enable_reflink,
        _ => false,
    };

    if !should_apply {
        return;
    }

    #[cfg(target_os = "linux")]
    {
        if !gfs_storage_btrfs::is_btrfs(gfs_dir) {
            eprintln!("{}", unsupported_storage_message(key));
            return;
        }

        gfs_storage_btrfs::apply_storage_config(gfs_dir, storage);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = gfs_dir;
        let _ = storage;
        eprintln!("{}", unsupported_storage_message(key));
    }
}

fn unsupported_storage_message(key: &str) -> &'static str {
    if key == KEY_STORAGE_COMPRESSION {
        "Warning: compression is only supported on btrfs. Your filesystem is not btrfs."
    } else {
        "Warning: reflink is only supported on btrfs. Your filesystem is not btrfs."
    }
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
        KEY_REMOTE_CONSOLE_URL | KEY_REMOTE_SUPABASE_URL | KEY_REMOTE_SUPABASE_ANON_KEY => {
            let out = gfs_console_remote::get_remote_config_value(key)?
                .unwrap_or_default();
            if !out.is_empty() {
                print!("{out}");
            }
        }
        _ => {
            anyhow::bail!(
                "unsupported config key '{}'; supported: {:?}",
                key,
                SUPPORTED_KEYS
            );
        }
    }

    Ok(())
}

fn set_global(key: &str, value: &str) -> Result<()> {
    if REMOTE_GLOBAL_KEYS.contains(&key) {
        gfs_console_remote::set_remote_config_value(key, value)?;
        return Ok(());
    }

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
                "unsupported config key '{}'; supported: {:?}",
                key,
                SUPPORTED_KEYS
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
