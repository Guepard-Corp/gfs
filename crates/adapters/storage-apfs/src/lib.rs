//! APFS adapter for the [`StoragePort`] port.
//!
//! Directory-level operations (`snapshot`, `clone`) use `cp -cRp` which
//! leverages the macOS clonefile(2) syscall to produce instant copy-on-write
//! (COW) duplicates on APFS without consuming extra disk space until the
//! copies diverge.
//!
//! Volume-level operations (`mount`, `unmount`, `status`, `quota`) continue
//! to shell out to `diskutil(8)` / `df(1)`.
//!
//! # Snapshot / clone model
//!
//! * [`snapshot`](ApfsStorage::snapshot) – COW-copies the source directory
//!   (`VolumeId.0` as a path) to a destination determined by
//!   [`SnapshotOptions::label`] (treated as the destination path).  When no
//!   label is supplied a timestamp-based sibling directory is used.
//! * [`clone`](ApfsStorage::clone) – COW-copies the source (or a previous
//!   snapshot, when [`CloneOptions::from_snapshot`] is set) to the target
//!   path (`target_id.0`).
//!
//! # Limitations
//!
//! * **quota** – APFS does not enforce per-volume quotas unless configured via
//!   `diskutil apfs updateContainerQuota`.  The returned values reflect actual
//!   disk usage obtained from `df(1)`.
//! * `diskutil` output is parsed as human-readable text.  A future revision
//!   should use `diskutil … -plist` with a proper plist parser for robustness.

mod error;

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use gfs_domain::ports::storage::{
    CloneOptions, MountStatus, Quota, Result, Snapshot, SnapshotId, SnapshotOptions, StorageError,
    StoragePort, VolumeId, VolumeStatus,
};
use tokio::process::Command;
use tracing::instrument;

use crate::error::classify_diskutil_stderr;

async fn make_tree_read_only(path: &Path) -> Result<()> {
    let chmod_out = Command::new("chmod")
        .arg("-R")
        .arg("u+rX,u-w,go-rwx")
        .arg(path)
        .env("LANG", "C")
        .output()
        .await
        .map_err(StorageError::Io)?;
    if !chmod_out.status.success() {
        let stderr = String::from_utf8_lossy(&chmod_out.stderr);
        return Err(StorageError::Internal(format!(
            "chmod -R u+rX,u-w,go-rwx '{}' failed: {}",
            path.display(),
            stderr.trim()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ApfsStorage
// ---------------------------------------------------------------------------

/// Storage adapter backed by macOS APFS via `diskutil(8)`.
#[derive(Debug, Default)]
pub struct ApfsStorage;

impl ApfsStorage {
    pub fn new() -> Self {
        Self
    }

    /// Run `diskutil <args>` and return stdout on success.
    async fn diskutil(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("diskutil")
            .args(args)
            .output()
            .await
            .map_err(StorageError::Io)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(StorageError::Internal(stderr.trim().to_owned()));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

// ---------------------------------------------------------------------------
// StoragePort implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl StoragePort for ApfsStorage {
    /// Mount `id` at `mount_point` using `diskutil mount -mountPoint`.
    ///
    /// Creates `mount_point` (and any missing parent directories) if it does
    /// not already exist.
    #[instrument(skip(self))]
    async fn mount(&self, id: &VolumeId, mount_point: &Path) -> Result<()> {
        tokio::fs::create_dir_all(mount_point)
            .await
            .map_err(StorageError::Io)?;

        let mp = mount_point.to_string_lossy();
        self.diskutil(&["mount", "-mountPoint", &mp, &id.0])
            .await
            .map_err(|e| remap_id(e, id))?;
        Ok(())
    }

    /// Unmount `id` using `diskutil unmount`.
    #[instrument(skip(self))]
    async fn unmount(&self, id: &VolumeId) -> Result<()> {
        let output = Command::new("diskutil")
            .args(["unmount", &id.0])
            .output()
            .await
            .map_err(StorageError::Io)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(classify_diskutil_stderr(&id.0, &stderr));
        }
        Ok(())
    }

    /// COW-copy the source directory to a snapshot destination using `cp -cRp`,
    /// then make the destination tree read-only with `chmod -R a-w`.
    ///
    /// On APFS `cp -cRp` uses clonefile(2) under the hood, producing an
    /// instant copy-on-write snapshot that starts with zero unique bytes.
    /// The snapshot is then made immutable (read-only) so it cannot be modified.
    ///
    /// The destination path is taken from [`SnapshotOptions::label`] (treated
    /// as an absolute or relative path).  When no label is given a
    /// timestamp-based name is appended to the parent of the source directory.
    ///
    /// The returned [`SnapshotId`] contains the destination path string so
    /// callers can pass it back via [`CloneOptions::from_snapshot`].
    #[instrument(skip(self))]
    async fn snapshot(&self, id: &VolumeId, options: SnapshotOptions) -> Result<Snapshot> {
        let source = Path::new(&id.0);

        let dest: PathBuf = match &options.label {
            Some(label) => PathBuf::from(label),
            None => {
                let name = format!("snap-{}", chrono::Utc::now().format("%Y%m%dT%H%M%SZ"));
                source
                    .parent()
                    .map(|p| p.join(&name))
                    .unwrap_or_else(|| PathBuf::from(&name))
            }
        };

        let output = Command::new("cp")
            .args(["-cRp", &id.0, dest.to_string_lossy().as_ref()])
            .env("LANG", "C")
            .output()
            .await
            .map_err(StorageError::Io)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let msg = format!(
                "cp -cRp '{}' '{}' failed: {}",
                id,
                dest.display(),
                stderr.trim()
            );
            let lower = stderr.to_ascii_lowercase();
            if lower.contains("permission denied") || lower.contains("operation not permitted") {
                return Err(StorageError::PermissionDenied(msg));
            }
            return Err(StorageError::Internal(msg));
        }

        make_tree_read_only(&dest).await?;

        Ok(Snapshot {
            id: SnapshotId(dest.to_string_lossy().into_owned()),
            volume_id: id.clone(),
            created_at: chrono::Utc::now(),
            // COW copies start with 0 unique bytes on APFS.
            size_bytes: 0,
            label: options.label,
        })
    }

    /// COW-copy a directory to a new location using `cp -cRp`.
    ///
    /// On APFS `cp -cRp` uses clonefile(2) to produce an instant
    /// copy-on-write clone that consumes no additional space until the
    /// two copies diverge.
    ///
    /// * `source.0` — path to the directory to copy from (ignored when
    ///   `options.from_snapshot` is set).
    /// * `options.from_snapshot` — if present, its `SnapshotId.0` is used as
    ///   the source path instead (clone from an existing snapshot).
    /// * `target_id.0` — destination path for the new copy.
    #[instrument(skip(self))]
    async fn clone(
        &self,
        source: &VolumeId,
        target_id: VolumeId,
        options: CloneOptions,
    ) -> Result<VolumeStatus> {
        let src = options
            .from_snapshot
            .as_ref()
            .map(|s| s.0.as_str())
            .unwrap_or(&source.0);

        let output = Command::new("cp")
            .args(["-cRp", src, &target_id.0])
            .env("LANG", "C")
            .output()
            .await
            .map_err(StorageError::Io)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let msg = format!(
                "cp -cRp '{}' '{}' failed: {}",
                src,
                target_id,
                stderr.trim()
            );
            let lower = stderr.to_ascii_lowercase();
            if lower.contains("permission denied") || lower.contains("operation not permitted") {
                return Err(StorageError::PermissionDenied(msg));
            }
            return Err(StorageError::Internal(msg));
        }

        let target_path = PathBuf::from(&target_id.0);
        Ok(VolumeStatus {
            id: target_id,
            mount_point: Some(target_path),
            status: MountStatus::Mounted,
            size_bytes: 0,
            used_bytes: 0,
        })
    }

    /// Return the live status of `id` by parsing `diskutil info` output.
    #[instrument(skip(self))]
    async fn status(&self, id: &VolumeId) -> Result<VolumeStatus> {
        let output = self
            .diskutil(&["info", &id.0])
            .await
            .map_err(|e| remap_id(e, id))?;

        parse_diskutil_info(id, &output)
    }

    /// Return disk-usage quota for `id` obtained from `df(1)`.
    ///
    /// APFS containers share free space between volumes; the returned
    /// `limit_bytes` reflects the container capacity.
    #[instrument(skip(self))]
    async fn quota(&self, id: &VolumeId) -> Result<Quota> {
        let output = Command::new("df")
            .args(["-k", &id.0])
            .output()
            .await
            .map_err(StorageError::Io)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(StorageError::Internal(stderr.trim().to_owned()));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_df_output(id, &stdout)
    }

    async fn finalize_snapshot(&self, dest: &Path) -> Result<()> {
        make_tree_read_only(dest).await
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Parse human-readable `diskutil info <volume>` output into [`VolumeStatus`].
fn parse_diskutil_info(id: &VolumeId, output: &str) -> Result<VolumeStatus> {
    let mut mount_point: Option<PathBuf> = None;
    let mut size_bytes: u64 = 0;
    let mut used_bytes: u64 = 0;

    for line in output.lines() {
        let line = line.trim();

        if let Some(rest) = line.strip_prefix("Mount Point:") {
            let mp = rest.trim();
            if !mp.is_empty() && mp != "Not applicable (not mounted)" {
                mount_point = Some(PathBuf::from(mp));
            }
        } else if let Some(rest) = line.strip_prefix("Volume Capacity:") {
            size_bytes = parse_bytes_field(rest);
        } else if let Some(rest) = line.strip_prefix("Volume Used:") {
            used_bytes = parse_bytes_field(rest);
        }
    }

    let status = if mount_point.is_some() {
        MountStatus::Mounted
    } else {
        MountStatus::Unmounted
    };

    Ok(VolumeStatus {
        id: id.clone(),
        mount_point,
        status,
        size_bytes,
        used_bytes,
    })
}

/// Extract the leading integer from a field like `"494384795648 Bytes (460.4 GB)"`.
fn parse_bytes_field(s: &str) -> u64 {
    s.split_whitespace()
        .next()
        .and_then(|n| n.replace(',', "").parse().ok())
        .unwrap_or(0)
}

/// Parse `df -k <volume>` output into a [`Quota`].
///
/// ```text
/// Filesystem   1024-blocks      Used Available Capacity  Mounted on
/// /dev/disk3s1   482793748 382854332  99939416    80%    /
/// ```
fn parse_df_output(id: &VolumeId, output: &str) -> Result<Quota> {
    let data_line = output
        .lines()
        .nth(1)
        .ok_or_else(|| StorageError::Internal(format!("empty df output for '{}'", id)))?;

    let parts: Vec<&str> = data_line.split_whitespace().collect();
    if parts.len() < 4 {
        return Err(StorageError::Internal(format!(
            "unexpected df output format for '{}'",
            id
        )));
    }

    let total_kb: u64 = parts[1].parse().unwrap_or(0);
    let used_kb: u64 = parts[2].parse().unwrap_or(0);
    let free_kb: u64 = parts[3].parse().unwrap_or(0);

    Ok(Quota {
        volume_id: id.clone(),
        limit_bytes: total_kb * 1024,
        used_bytes: used_kb * 1024,
        free_bytes: free_kb * 1024,
    })
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

/// Re-map a generic `Internal` error from `diskutil` into a more specific
/// variant using the stderr text and the volume id we know about.
fn remap_id(err: StorageError, id: &VolumeId) -> StorageError {
    match err {
        StorageError::Internal(ref msg) => classify_diskutil_stderr(&id.0, msg),
        other => other,
    }
}
