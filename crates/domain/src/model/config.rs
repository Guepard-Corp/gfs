use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::errors::RepoError;
use crate::model::layout::{CONFIG_FILE, GFS_DIR};

/// Returns the user's home directory ($HOME on Unix, %USERPROFILE% on Windows).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserConfig {
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentConfig {
    pub database_provider: String,
    pub database_version: String,
    #[serde(default)]
    pub database_port: Option<u16>,
    /// Console-provided deployment label (`deployment_request.label_name`).
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Runtime backend (e.g. `"docker"`, `"kubernetes"`, `"guepard"`).
    pub runtime_provider: String,
    pub runtime_version: String,
    pub container_name: String,
}

/// Console-mediated remote target (no local k3s API).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub console_url: String,
    /// Supabase deployment UUID. Omitted in legacy repos (same as `database_id`).
    #[serde(default)]
    pub deployment_id: Option<String>,
    pub node_id: String,
    /// CP `cpDatabaseId` (engine row). Legacy repos stored the deployment UUID here.
    pub database_id: String,
    #[serde(default = "default_remote_project")]
    pub project: String,
}

impl RemoteConfig {
    /// Supabase deployment UUID for deployment-scoped console routes.
    pub fn deployment_id(&self) -> &str {
        self.deployment_id.as_deref().unwrap_or(&self.database_id)
    }

    /// CP engine database id for thin node-level VCS proxy routes.
    pub fn cp_database_id(&self) -> &str {
        &self.database_id
    }
}

fn default_remote_project() -> String {
    "default".to_string()
}

impl fmt::Display for RuntimeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RuntimeConfig(provider: {}, version: {}, container: {})",
            self.runtime_provider, self.runtime_version, self.container_name
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StorageConfig {
    #[serde(default)]
    pub compression: Option<String>,
    #[serde(default)]
    pub enable_reflink: bool,
}

impl fmt::Display for StorageConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "StorageConfig(compression: {:?}, reflink: {})",
            self.compression, self.enable_reflink
        )
    }
}

/// Provider-agnostic tuning of the database container.
///
/// `params` is an open map of logical setting names to values (e.g.
/// `max_connections = "200"`, `shared_buffers = "256MB"`). It is intentionally
/// not validated here: each database provider renders these into its own
/// container CLI syntax at provision time (PostgreSQL `-c name=value`, MySQL
/// `--name=value`, …) via `DatabaseProvider::render_param_overrides`. Keys must
/// therefore be valid settings for the configured provider.
///
/// A `BTreeMap` keeps the rendered argument order deterministic.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ComputeConfig {
    #[serde(default)]
    pub params: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GfsConfig {
    pub mount_point: Option<String>,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub user: Option<UserConfig>,
    #[serde(default)]
    pub environment: Option<EnvironmentConfig>,
    #[serde(default)]
    pub runtime: Option<RuntimeConfig>,
    #[serde(default)]
    pub storage: Option<StorageConfig>,
    #[serde(default)]
    pub compute: Option<ComputeConfig>,
    #[serde(default)]
    pub remote: Option<RemoteConfig>,
}

impl GfsConfig {
    pub fn is_guepard_remote(&self) -> bool {
        let provider = self
            .runtime
            .as_ref()
            .map(|r| r.runtime_provider.trim().to_ascii_lowercase())
            .unwrap_or_default();
        matches!(provider.as_str(), "guepard" | "console" | "remote")
            && self.remote.is_some()
    }

    pub fn load(repo_path: &Path) -> Result<Self, RepoError> {
        let config_path = repo_path.join(GFS_DIR).join(CONFIG_FILE);
        let content = std::fs::read_to_string(config_path)?;
        let config =
            toml::from_str(&content).map_err(|e| RepoError::InvalidConfig(e.to_string()))?;
        Ok(config)
    }

    pub fn save(&self, repo_path: &Path) -> Result<(), RepoError> {
        let config_path = repo_path.join(GFS_DIR).join(CONFIG_FILE);
        let content =
            toml::to_string_pretty(self).map_err(|e| RepoError::InvalidConfig(e.to_string()))?;
        std::fs::write(config_path, content)?;
        Ok(())
    }

    /// Container parameter overrides (`[compute.params]`), or an empty map when
    /// none are configured. Suitable to pass straight to
    /// `DatabaseProvider::definition_with_overrides`.
    pub fn compute_params(&self) -> std::collections::BTreeMap<String, String> {
        self.compute
            .as_ref()
            .map(|c| c.params.clone())
            .unwrap_or_default()
    }

    /// Load `compute_params` for `repo_path`, tolerating a missing/invalid
    /// config (returns an empty map). Convenience for provisioning sites that
    /// only need the overrides.
    pub fn load_compute_params(repo_path: &Path) -> std::collections::BTreeMap<String, String> {
        Self::load(repo_path)
            .map(|c| c.compute_params())
            .unwrap_or_default()
    }
}

fn default_telemetry() -> bool {
    true
}

/// Global GFS settings stored in `~/.gfs/config.toml`.
///
/// Provides system-wide defaults for user identity (name, email) that apply
/// to every repository, similar to `~/.gitconfig`.
#[derive(Debug, Serialize, Deserialize)]
pub struct GlobalSettings {
    #[serde(default)]
    pub user: Option<UserConfig>,
    #[serde(default = "default_telemetry")]
    pub telemetry: bool,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            user: None,
            telemetry: true,
        }
    }
}

impl GlobalSettings {
    /// Path to the global config file: `$HOME/.gfs/config.toml`.
    pub fn path() -> Option<PathBuf> {
        home_dir().map(|h| h.join(".gfs").join("config.toml"))
    }

    /// Load global settings from `~/.gfs/config.toml`.
    /// Returns `None` if the file does not exist or cannot be parsed.
    pub fn load() -> Option<Self> {
        let path = Self::path()?;
        let content = std::fs::read_to_string(path).ok()?;
        toml::from_str(&content).ok()
    }

    /// Save global settings to `~/.gfs/config.toml`, creating `~/.gfs/` if needed.
    pub fn save(&self) -> Result<(), RepoError> {
        let path = Self::path()
            .ok_or_else(|| RepoError::InvalidConfig("cannot determine home directory".into()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content =
            toml::to_string_pretty(self).map_err(|e| RepoError::InvalidConfig(e.to_string()))?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    use crate::model::layout::GFS_DIR;

    static HOME_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn config_load_and_save() {
        let dir = tempfile::tempdir().unwrap();
        let gfs_dir = dir.path().join(GFS_DIR);
        std::fs::create_dir_all(&gfs_dir).unwrap();

        let config = GfsConfig {
            mount_point: Some("/mnt".into()),
            version: "1".into(),
            description: "test".into(),
            user: Some(UserConfig {
                name: Some("Alice".into()),
                email: Some("alice@example.com".into()),
            }),
            environment: Some(EnvironmentConfig {
                database_provider: "postgres".into(),
                database_version: "17".into(),
                database_port: Some(5432),
                display_name: Some("my-db".into()),
            }),
            runtime: Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "c1".into(),
            }),
            storage: Some(StorageConfig {
                compression: Some("zstd".into()),
                enable_reflink: true,
            }),
            compute: None,
            remote: None,
        };
        config.save(dir.path()).unwrap();

        let loaded = GfsConfig::load(dir.path()).unwrap();
        assert_eq!(loaded.mount_point, config.mount_point);
        assert_eq!(loaded.version, config.version);
        assert_eq!(
            loaded.environment.as_ref().unwrap().database_provider,
            "postgres"
        );
        assert_eq!(
            loaded.environment.as_ref().unwrap().database_port,
            Some(5432)
        );
        assert_eq!(
            loaded.environment.as_ref().unwrap().display_name.as_deref(),
            Some("my-db")
        );
        assert_eq!(loaded.runtime.as_ref().unwrap().container_name, "c1");
        assert_eq!(
            loaded.storage.as_ref().unwrap().compression,
            Some("zstd".into())
        );
        assert!(loaded.storage.as_ref().unwrap().enable_reflink);
    }

    #[test]
    fn config_load_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = GfsConfig::load(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn config_load_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let gfs_dir = dir.path().join(GFS_DIR);
        std::fs::create_dir_all(&gfs_dir).unwrap();
        std::fs::write(gfs_dir.join(CONFIG_FILE), "invalid toml [[[").unwrap();
        let result = GfsConfig::load(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn user_config_default() {
        let uc = UserConfig::default();
        assert!(uc.name.is_none());
        assert!(uc.email.is_none());
    }

    #[test]
    fn compute_params_roundtrip_and_accessor() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(GFS_DIR)).unwrap();
        let mut params = std::collections::BTreeMap::new();
        params.insert("max_connections".to_string(), "200".to_string());
        params.insert("shared_buffers".to_string(), "256MB".to_string());
        let config = GfsConfig {
            mount_point: None,
            version: "1".into(),
            description: "test".into(),
            user: None,
            environment: None,
            runtime: None,
            storage: None,
            compute: Some(ComputeConfig { params }),
            remote: None,
        };
        config.save(dir.path()).unwrap();

        let loaded = GfsConfig::load(dir.path()).unwrap();
        let p = loaded.compute_params();
        assert_eq!(p.get("max_connections").map(String::as_str), Some("200"));
        assert_eq!(p.get("shared_buffers").map(String::as_str), Some("256MB"));
    }

    #[test]
    fn compute_params_defaults_to_empty_when_absent() {
        assert!(
            GfsConfig {
                mount_point: None,
                version: String::new(),
                description: String::new(),
                user: None,
                environment: None,
                runtime: None,
                storage: None,
                compute: None,
                remote: None,
            }
            .compute_params()
            .is_empty()
        );
    }

    #[test]
    fn config_save_error_no_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = GfsConfig {
            mount_point: None,
            version: "1".into(),
            description: "test".into(),
            user: None,
            environment: None,
            runtime: None,
            storage: None,
            compute: None,
            remote: None,
        };
        // Pass path where .gfs does not exist; save writes to repo_path/.gfs/config.toml
        let result = config.save(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn global_settings_load_save_roundtrip() {
        let _home_guard = HOME_TEST_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let home_path = dir.path().to_string_lossy().to_string();
        unsafe { std::env::set_var("HOME", &home_path) };

        let settings = GlobalSettings {
            user: Some(UserConfig {
                name: Some("Bob".into()),
                email: Some("bob@example.com".into()),
            }),
            telemetry: true,
        };
        settings.save().unwrap();

        let loaded = GlobalSettings::load().unwrap();
        assert_eq!(loaded.user.as_ref().unwrap().name.as_deref(), Some("Bob"));
        assert_eq!(
            loaded.user.as_ref().unwrap().email.as_deref(),
            Some("bob@example.com")
        );
    }

    #[test]
    fn global_settings_load_missing_returns_none() {
        let _home_guard = HOME_TEST_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let home_path = dir.path().to_string_lossy().to_string();
        unsafe { std::env::set_var("HOME", &home_path) };
        assert!(GlobalSettings::load().is_none());
    }

    #[test]
    fn global_settings_telemetry_default_true() {
        let s = GlobalSettings::default();
        assert!(s.telemetry, "telemetry should default to true");
    }

    #[test]
    fn global_settings_telemetry_serde_default() {
        // When the field is absent from TOML, it should default to true.
        let s: GlobalSettings = toml::from_str("").unwrap();
        assert!(s.telemetry);
        let s2: GlobalSettings = toml::from_str("telemetry = false").unwrap();
        assert!(!s2.telemetry);
    }

    #[test]
    fn runtime_config_display() {
        let r = RuntimeConfig {
            runtime_provider: "docker".into(),
            runtime_version: "24".into(),
            container_name: "abc123".into(),
        };
        let s = r.to_string();
        assert!(s.contains("docker"));
        assert!(s.contains("24"));
        assert!(s.contains("abc123"));
    }
}
