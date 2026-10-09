use std::path::Path;

use async_trait::async_trait;

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("volume not found: '{0}'")]
    NotFound(String),

    #[error("volume already exists: '{0}'")]
    AlreadyExists(String),

    #[error("volume is busy: '{0}'")]
    Busy(String),

    #[error("snapshot not found: '{0}'")]
    SnapshotNotFound(String),

    #[error("quota exceeded on '{volume}': used {used_bytes} / {limit_bytes} bytes")]
    QuotaExceeded {
        volume: String,
        used_bytes: u64,
        limit_bytes: u64,
    },

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, StorageError>;

// ---------------------------------------------------------------------------
// Supporting types
// ---------------------------------------------------------------------------

/// Unique identifier for a managed volume.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VolumeId(pub String);

impl std::fmt::Display for VolumeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Unique identifier for a point-in-time snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SnapshotId(pub String);

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Runtime status of a mounted volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountStatus {
    Mounted,
    Unmounted,
    Degraded,
    Unknown,
}

/// Point-in-time snapshot metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: SnapshotId,
    pub volume_id: VolumeId,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub size_bytes: u64,
    pub label: Option<String>,
}

/// Live status of a volume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeStatus {
    pub id: VolumeId,
    pub mount_point: Option<std::path::PathBuf>,
    pub status: MountStatus,
    pub size_bytes: u64,
    pub used_bytes: u64,
}

/// Disk-usage quota for a volume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quota {
    pub volume_id: VolumeId,
    pub limit_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
}

/// Options for the `snapshot` operation.
#[derive(Debug, Default)]
pub struct SnapshotOptions {
    /// Human-readable label attached to the snapshot.
    pub label: Option<String>,
}

/// Options for the `clone` operation.
#[derive(Debug, Default)]
pub struct CloneOptions {
    /// Snapshot to clone from. Clones the live volume when `None`.
    pub from_snapshot: Option<SnapshotId>,
}

// ---------------------------------------------------------------------------
// Port
// ---------------------------------------------------------------------------

/// Port that abstracts block/filesystem storage operations.
///
/// Implementations may target LVM thin-pools, ZFS datasets, Btrfs subvolumes,
/// or any other storage backend that supports snapshots and quotas.
#[async_trait]
pub trait StoragePort: Send + Sync {
    /// Make the volume identified by `id` accessible at `mount_point`.
    async fn mount(&self, id: &VolumeId, mount_point: &Path) -> Result<()>;

    /// Detach the volume identified by `id` from its current mount point.
    async fn unmount(&self, id: &VolumeId) -> Result<()>;

    /// Create a point-in-time snapshot of `id` and return its metadata.
    async fn snapshot(&self, id: &VolumeId, options: SnapshotOptions) -> Result<Snapshot>;

    /// Clone a volume (optionally from a snapshot) into a new volume named
    /// `target_id` and return its status.
    async fn clone(
        &self,
        source: &VolumeId,
        target_id: VolumeId,
        options: CloneOptions,
    ) -> Result<VolumeStatus>;

    /// Return the current runtime status of a volume.
    async fn status(&self, id: &VolumeId) -> Result<VolumeStatus>;

    /// Return disk-usage quota information for a volume.
    async fn quota(&self, id: &VolumeId) -> Result<Quota>;

    /// Make the directory tree at `dest` read-only.
    ///
    /// Called after `Compute::stream_snapshot` to restore the same immutability
    /// that `snapshot()` already provides internally.  Implementations should
    /// apply the platform-appropriate mechanism (e.g. `chmod -R a-w` on Unix,
    /// `attrib +R /S /D` on Windows).
    async fn finalize_snapshot(&self, dest: &Path) -> Result<()>;

    /// Whether a finished snapshot destination captured any data at all, or
    /// `None` when this adapter cannot say.
    ///
    /// Deliberately a yes/no and not a byte total. The only question asked of it
    /// is "did this capture nothing?", and answering that needs one regular file,
    /// not a sum over the tree — a total would stat every file in the data
    /// directory on the hot path of every commit, which for a real database is
    /// tens of thousands of `stat` calls to learn something the first file
    /// settles.
    ///
    /// `Snapshot::size_bytes` cannot answer it either: `storage-apfs`,
    /// `storage-file` and `storage-kubernetes` all hardcode it to `0` on the
    /// snapshot path, so a caller treating it as a byte count would read every
    /// snapshot as empty and refuse every commit.
    ///
    /// `None` means *cannot tell* and callers must stay silent on it, the same
    /// discipline the reflink probe uses: a measurement that failed says nothing
    /// about the thing being measured, and guessing from it is how a guard starts
    /// firing on healthy repositories.
    async fn captured_any_data(&self, _dest: &Path) -> Result<Option<bool>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_id_display() {
        let id = VolumeId("vol-1".into());
        assert_eq!(id.to_string(), "vol-1");
    }

    #[test]
    fn snapshot_id_display() {
        let id = SnapshotId("snap-1".into());
        assert_eq!(id.to_string(), "snap-1");
    }

    #[test]
    fn storage_error_display() {
        assert_eq!(
            StorageError::NotFound("v1".into()).to_string(),
            "volume not found: 'v1'"
        );
        assert_eq!(
            StorageError::AlreadyExists("v2".into()).to_string(),
            "volume already exists: 'v2'"
        );
        assert_eq!(
            StorageError::Busy("v3".into()).to_string(),
            "volume is busy: 'v3'"
        );
        assert_eq!(
            StorageError::SnapshotNotFound("s1".into()).to_string(),
            "snapshot not found: 's1'"
        );
        assert_eq!(
            StorageError::QuotaExceeded {
                volume: "v".into(),
                used_bytes: 100,
                limit_bytes: 50,
            }
            .to_string(),
            "quota exceeded on 'v': used 100 / 50 bytes"
        );
        assert_eq!(
            StorageError::Internal("err".into()).to_string(),
            "internal error: err"
        );
    }
}
