use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::errors::RepoError;
use crate::model::layout::{CONFIG_FILE, GFS_DIR};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserConfig {
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentConfig {
    pub database_provider: String,
    pub database_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Runtime backend (e.g. `"docker"`, `"firecracker"`).
    pub runtime_provider: String,
    pub runtime_version: String,
    pub container_name: String,
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
}

impl GfsConfig {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::model::layout::GFS_DIR;

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
            }),
            runtime: Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "c1".into(),
            }),
        };
        config.save(dir.path()).unwrap();

        let loaded = GfsConfig::load(dir.path()).unwrap();
        assert_eq!(loaded.mount_point, config.mount_point);
        assert_eq!(loaded.version, config.version);
        assert_eq!(
            loaded.environment.as_ref().unwrap().database_provider,
            "postgres"
        );
        assert_eq!(loaded.runtime.as_ref().unwrap().container_name, "c1");
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
    fn config_save_error_no_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = GfsConfig {
            mount_point: None,
            version: "1".into(),
            description: "test".into(),
            user: None,
            environment: None,
            runtime: None,
        };
        // Pass path where .gfs does not exist; save writes to repo_path/.gfs/config.toml
        let result = config.save(dir.path());
        assert!(result.is_err());
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
