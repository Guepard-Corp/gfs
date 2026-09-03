//! Status response DTO for the status use case.
//!
//! Read-only aggregate of repository, config, and compute runtime data.

use serde::{Deserialize, Serialize};

/// Top-level status response for a GFS repository and its compute instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    /// Name of the branch at HEAD (e.g. `main`, `develop`), or commit hash / `(detached)` when HEAD is detached.
    pub current_branch: String,

    /// Compute/database instance status. Omitted or partial when no compute is provisioned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute: Option<ComputeStatus>,

    /// Path to the active workspace data directory (from WORKSPACE file). Used to detect bind mismatch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_workspace_data_dir: Option<String>,

    /// Set when the container is bound to a different path than the active workspace (e.g. after checkout).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_mismatch_warning: Option<String>,

    /// Where a lazy clone stands relative to its source (issue #133). Populated by
    /// the CLI (the drift SQL lives there, next to `gfs fetch`/`gfs pull`); `None`
    /// when the repository is not a lazy clone or its database is not running, and
    /// omitted from JSON in that case so non-clone output is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceStatus>,
}

/// Compute section of the status response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeStatus {
    /// Compute/database provider (e.g. `postgresql`, `mysql`). From repo config `environment.database_provider`.
    pub provider: String,

    /// Database or image version (e.g. `16`, `latest`). From repo config `environment.database_version`.
    pub version: String,

    /// Current runtime state of the container: `starting`, `running`, `paused`, `stopping`, `stopped`, `restarting`, `failed`, `unknown`.
    pub container_status: String,

    /// Unique identifier of the compute instance (e.g. Docker container ID or name).
    pub container_id: String,

    /// Client connection string for the database (e.g. `postgresql://user:pass@localhost:5432/db`).
    /// Empty when not available (e.g. container not running or credentials not resolvable).
    #[serde(default)]
    pub connection_string: String,

    /// Host path the container's data volume is bound to (from Docker inspect). Used to detect bind mismatch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_bind_host_path: Option<String>,
}

/// Source section of the status response (lazy clones only).
///
/// A cached verdict, not a probe: it reports what the clone already knows about
/// its source (`gfs fetch --check` refreshes it). Counts are table-granular --
/// GFS has no per-row change log on either side, so "behind" means "this table
/// changed", not "these rows changed" (see RFC 007).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceStatus {
    /// Tables registered for drift tracking (`gfs.clone_source`). `0` means
    /// nothing has been compared yet, not "up to date".
    pub tracked: u64,

    /// Tables whose rows changed on the source since last synced (`drift_state.drifted`).
    pub behind: u64,

    /// Subset of `behind` that ALSO has local writes; `gfs pull` refuses these
    /// without `--force`.
    pub diverged: u64,

    /// When the drift verdict was last computed (`YYYY-MM-DD HH:MM:SS`).
    /// Named `last_checked` (the column is `checked_at`) so `gfs status
    /// --output json` and `gfs fetch --json` agree on one name.
    /// Empty (and omitted from JSON) until the first check.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_checked: String,

    /// #132: `Some(true)` when this clone was frozen into a detached snapshot
    /// (`gfs.clone_mode`). Omitted from JSON for ordinary lazy clones so
    /// pre-#132 output is byte-identical. When frozen, `diverged` above counts
    /// the tables the freeze KEPT for local writes (there is no live drift any
    /// more, so the lazy-clone reading of that field cannot apply).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen: Option<bool>,

    /// #132: when the clone was frozen (`YYYY-MM-DD HH:MM:SS`); omitted unless
    /// frozen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository that is not a lazy clone must serialize exactly as before
    /// #133: no `source` key at all, not `"source": null`.
    #[test]
    fn source_omitted_when_none() {
        let s = StatusResponse {
            current_branch: "main".into(),
            compute: None,
            active_workspace_data_dir: None,
            bind_mismatch_warning: None,
            source: None,
        };
        let json = serde_json::to_string(&s).expect("serialize");
        assert!(!json.contains("source"), "unexpected source key in {json}");
        // and a pre-#133 payload (no source field) still deserializes
        let back: StatusResponse = serde_json::from_str(&json).expect("deserialize");
        assert!(back.source.is_none());
    }

    #[test]
    fn source_round_trips() {
        let s = StatusResponse {
            current_branch: "main".into(),
            compute: None,
            active_workspace_data_dir: None,
            bind_mismatch_warning: None,
            source: Some(SourceStatus {
                tracked: 3,
                behind: 2,
                diverged: 1,
                last_checked: "2026-08-31 12:00:00".into(),
                frozen: None,
                frozen_at: None,
            }),
        };
        let json = serde_json::to_string(&s).expect("serialize");
        assert!(json.contains("\"last_checked\""));
        let back: StatusResponse = serde_json::from_str(&json).expect("deserialize");
        let src = back.source.expect("source present");
        assert_eq!(src.tracked, 3);
        assert_eq!(src.behind, 2);
        assert_eq!(src.diverged, 1);
        assert_eq!(src.last_checked, "2026-08-31 12:00:00");
    }

    /// #132: a lazy (unfrozen) clone must serialize exactly as it did before
    /// snapshot mode existed -- no `frozen`/`frozen_at` keys, not nulls -- and a
    /// frozen clone's fields must round-trip.
    #[test]
    fn frozen_omitted_until_frozen() {
        let lazy = SourceStatus {
            tracked: 3,
            behind: 0,
            diverged: 0,
            last_checked: String::new(),
            frozen: None,
            frozen_at: None,
        };
        let json = serde_json::to_string(&lazy).expect("serialize");
        assert!(!json.contains("frozen"), "unexpected frozen key in {json}");
        // and a pre-#132 payload (no frozen fields) still deserializes
        let back: SourceStatus = serde_json::from_str(&json).expect("deserialize");
        assert!(back.frozen.is_none());

        let frozen = SourceStatus {
            tracked: 3,
            behind: 0,
            diverged: 1,
            last_checked: String::new(),
            frozen: Some(true),
            frozen_at: Some("2026-08-31 12:00:00".into()),
        };
        let json = serde_json::to_string(&frozen).expect("serialize");
        assert!(json.contains("\"frozen\":true"));
        let back: SourceStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.frozen, Some(true));
        assert_eq!(back.frozen_at.as_deref(), Some("2026-08-31 12:00:00"));
    }
}
