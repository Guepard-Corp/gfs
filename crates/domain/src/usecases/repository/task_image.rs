//! Shared helper for resolving sidecar-task container images.

use crate::model::config::GfsConfig;

/// Re-tag a provider's default sidecar-task image with the repository's
/// configured `database_version`, so schema/commit/export/import/clone task
/// pods run the same database version as the deployed instance instead of the
/// provider's hardcoded default tag (e.g. `gfs-postgres:16`). Mirrors the
/// deploy/checkout image-versioning logic.
///
/// Falls back to `"17"` (the supported default) when the config carries no
/// version, so the result never silently depends on the provider's default tag.
pub(crate) fn task_image_for_version(default_image: &str, config: &GfsConfig) -> String {
    let version = config
        .environment
        .as_ref()
        .map(|e| e.database_version.clone())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "17".to_string());
    let base = default_image.split(':').next().unwrap_or(default_image);
    format!("{base}:{version}")
}
