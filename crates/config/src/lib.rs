use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Top-level configuration, loaded from a TOML file.
///
/// Every section and field carries a default value, so the config file is
/// entirely optional — missing sections or keys fall back to their defaults.
///
/// ```toml
/// [node]
/// id     = "customer-vm-prod"
/// region = "eu-west-1"
///
/// [control_plane]
/// otel_logs_endpoint    = "http://localhost:7281"
/// otel_traces_endpoint  = "http://localhost:7281"
/// otel_metrics_endpoint = "http://localhost:7280"
///
/// [metrics]
/// interval_secs = 5
/// ```
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub node: NodeConfig,
    pub control_plane: ControlPlaneConfig,
    pub metrics: MetricsConfig,
}

/// Identity of this data-plane node.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    /// Unique node identifier (e.g. `"customer-vm-prod"`).
    /// Defaults to the system hostname, falling back to `"unknown"`.
    pub id: String,
    /// AWS / cloud region the node is deployed in (e.g. `"eu-west-1"`).
    /// Defaults to an empty string.
    pub region: String,
}

/// Connectivity settings toward the control plane.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ControlPlaneConfig {
    /// gRPC endpoint for OTEL logs. Defaults to `"http://localhost:7281"`.
    pub otel_logs_endpoint: String,
    /// gRPC endpoint for OTEL traces. Defaults to `"http://localhost:7281"`.
    pub otel_traces_endpoint: String,
    /// HTTP endpoint for OTEL metrics. Defaults to `"http://localhost:7280"`.
    pub otel_metrics_endpoint: String,
}

/// Metrics collection settings.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// How often metrics are collected and flushed, in seconds.
    /// Defaults to `5`.
    pub interval_secs: u64,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            id: hostname().unwrap_or_else(|| "unknown".to_string()),
            region: String::new(),
        }
    }
}

fn hostname() -> Option<String> {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

impl Default for ControlPlaneConfig {
    fn default() -> Self {
        Self {
            otel_logs_endpoint: "http://localhost:7281".to_string(),
            otel_traces_endpoint: "http://localhost:7281".to_string(),
            otel_metrics_endpoint: "http://localhost:7280/api/v1/otel-metrics-v0_1/ingest"
                .to_string(),
        }
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { interval_secs: 5 }
    }
}

impl Config {
    /// Load configuration from a TOML file.
    ///
    /// Any section or key absent from the file falls back to its default value.
    pub fn from_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;

        toml::from_str(&raw)
            .with_context(|| format!("failed to parse config file: {}", path.display()))
    }

    /// Load configuration from `path` when provided, or return all defaults
    /// when `None` is passed (useful when the config file is optional).
    pub fn load(path: Option<&Path>) -> Result<Self> {
        match path {
            Some(p) => Self::from_file(p),
            None => Ok(Self::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_load_none_returns_defaults() {
        let config = Config::load(None).unwrap();
        assert!(!config.node.id.is_empty() || config.node.id == "unknown");
        assert_eq!(
            config.control_plane.otel_logs_endpoint,
            "http://localhost:7281"
        );
        assert!(config.control_plane.otel_metrics_endpoint.contains("7280"));
        assert_eq!(config.metrics.interval_secs, 5);
    }

    #[test]
    fn config_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[node]
id = "test-node"
region = "eu-west-1"

[control_plane]
otel_logs_endpoint = "http://logs:7281"
otel_traces_endpoint = "http://traces:7281"
otel_metrics_endpoint = "http://metrics:7280"

[metrics]
interval_secs = 10
"#,
        )
        .unwrap();

        let config = Config::from_file(&path).unwrap();
        assert_eq!(config.node.id, "test-node");
        assert_eq!(config.node.region, "eu-west-1");
        assert_eq!(config.control_plane.otel_logs_endpoint, "http://logs:7281");
        assert_eq!(config.metrics.interval_secs, 10);
    }

    #[test]
    fn config_load_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[node]\nid = \"loaded\"\n").unwrap();

        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.node.id, "loaded");
    }

    #[test]
    fn config_from_file_missing() {
        let result = Config::from_file(Path::new("/nonexistent/config.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn config_default_section_defaults() {
        let config = Config::default();
        assert_eq!(config.metrics.interval_secs, 5);
        assert!(config.control_plane.otel_logs_endpoint.contains("7281"));
    }
}
