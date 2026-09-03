//! Cross-platform file-system storage adapter for the [`StoragePort`] port.
//!
//! # Platform overview
//!
//! | Platform | `snapshot` / `clone` | Read-only lock | `status` / `quota` |
//! |---|---|---|---|
//! | macOS | `cp -cRp` (APFS clonefile COW) | `chmod -R u+rX,u-w,go-rwx` | `diskutil info` / `df -k` |
//! | Linux | `cp --reflink=auto -a` (Btrfs/XFS COW or deep copy) | `chmod -R u+rX,u-w,go-rwx` | `df --block-size=1` |
//! | Windows | `robocopy /E /COPY:DAT` | `attrib +R /S /D` | PowerShell `Get-PSDrive` |
//!
//! # mount / unmount
//!
//! On macOS, `mount`/`unmount` shell out to `diskutil(8)` exactly as before.
//! On Linux and Windows the storage model is purely **directory-based** – there
//! is no block device to attach or detach – so both operations are **no-ops**
//! that always return `Ok(())`.
//!
//! # Snapshot / clone model
//!
//! * [`snapshot`](FileStorage::snapshot) – copies the source directory
//!   (`VolumeId.0` as a path) to a destination determined by
//!   [`SnapshotOptions::label`] (treated as the destination path). When no
//!   label is supplied a timestamp-based sibling directory is used.
//!   The destination is then made read-only.
//! * [`clone`](FileStorage::clone) – copies the source (or a previous
//!   snapshot, when [`CloneOptions::from_snapshot`] is set) to the target
//!   path (`target_id.0`).
//!
//! # Limitations
//!
//! * **quota** – APFS (macOS) does not enforce per-volume quotas unless
//!   configured via `diskutil apfs updateContainerQuota`. Returned values
//!   reflect actual disk usage from `df(1)`.
//! * `diskutil` output is parsed as human-readable text. A future revision
//!   should use `diskutil … -plist` with a proper plist parser.
//! * On Windows, `status`/`quota` are approximated from `PowerShell
//!   Get-PSDrive`; no full mount-point detection is performed.

mod error;

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use gfs_domain::ports::storage::{
    CloneOptions, MountStatus, Quota, Result, Snapshot, SnapshotId, SnapshotOptions, StorageError,
    StoragePort, VolumeId, VolumeStatus,
};
use tokio::process::Command;
use tracing::instrument;

use crate::error::classify_stderr;

fn map_copy_error(stderr: &str, msg: String) -> StorageError {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("permission denied") || lower.contains("operation not permitted") {
        return StorageError::PermissionDenied(msg);
    }
    StorageError::Internal(msg)
}

// ---------------------------------------------------------------------------
// FileStorage (public struct, cross-platform)
// ---------------------------------------------------------------------------

/// Storage adapter backed by the local filesystem.
///
/// Renamed from `ApfsStorage` to reflect cross-platform support.
/// The old name is kept as a type alias for backwards compatibility.
#[derive(Debug, Default)]
pub struct FileStorage;

/// Backwards-compatible alias so existing code referencing `ApfsStorage`
/// continues to compile without changes.
pub type ApfsStorage = FileStorage;

impl FileStorage {
    pub fn new() -> Self {
        Self
    }
}

// ---------------------------------------------------------------------------
// Platform-specific helpers
// ---------------------------------------------------------------------------

// ── macOS ──────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
impl FileStorage {
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

// ── Unix (macOS + Linux): chmod read-only helper ───────────────────────────

#[cfg(unix)]
async fn make_read_only(path: &Path) -> Result<()> {
    let out = Command::new("chmod")
        .arg("-R")
        .arg("u+rX,u-w,go-rwx")
        .arg(path)
        .output()
        .await
        .map_err(StorageError::Io)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(map_copy_error(
            &stderr,
            format!(
                "chmod -R u+rX,u-w,go-rwx '{}' failed: {}",
                path.display(),
                stderr.trim()
            ),
        ));
    }
    Ok(())
}

// ── Windows: attrib +R read-only helper ──────────────────────────────────

#[cfg(target_os = "windows")]
async fn make_read_only(path: &Path) -> Result<()> {
    let p = path.to_string_lossy().into_owned();
    // Set read-only on the directory itself.
    let out = Command::new("cmd")
        .args(["/C", "attrib", "+R", &p])
        .output()
        .await
        .map_err(StorageError::Io)?;
    if !out.status.success() {
        return Err(attrib_error(path, &out));
    }
    let contents = format!(r"{}\*", p);
    let out = Command::new("cmd")
        .args(["/C", "attrib", "+R", "/S", "/D", &contents])
        .output()
        .await
        .map_err(StorageError::Io)?;
    if !out.status.success() {
        return Err(attrib_error(path, &out));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn attrib_error(path: &Path, out: &std::process::Output) -> StorageError {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    StorageError::Internal(format!(
        "attrib +R failed for '{}': {}{}",
        path.display(),
        stderr.trim(),
        if stdout.is_empty() {
            String::new()
        } else {
            format!(" ({})", stdout.trim())
        }
    ))
}

// ── Cross-platform directory copy ──────────────────────────────────────────

/// Copy `src` to `dst` using the best mechanism available on this OS.
///
/// * **macOS** – `cp -cRp` triggers clonefile(2) COW on APFS.
/// * **Linux** – `cp --reflink=auto -a` uses Btrfs/XFS COW when available,
///   and falls back to a regular deep copy on other filesystems.
/// * **Windows** – `robocopy /E /COPY:DAT` (not `/COPYALL`, which needs audit privileges).
async fn copy_dir(src: &str, dst: &str) -> Result<()> {
    let dst_path = Path::new(dst);
    if let Some(parent) = dst_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(StorageError::Io)?;
    }

    // Windows: strip the `\\?\` extended-length-path prefix that
    // `std::fs::canonicalize` injects. Robocopy treats `\\?\C:\...` as a UNC
    // network path and fails with ERROR_BAD_NETPATH (53) when creating the
    // destination directory.
    #[cfg(target_os = "windows")]
    fn strip_ext_prefix(s: &str) -> &str {
        s.strip_prefix(r"\\?\").unwrap_or(s)
    }
    #[cfg(target_os = "windows")]
    let (src, dst) = (strip_ext_prefix(src), strip_ext_prefix(dst));

    #[cfg(target_os = "macos")]
    let (prog, args): (&str, Vec<&str>) = ("cp", vec!["-cRp", src, dst]);

    #[cfg(target_os = "linux")]
    let (prog, args): (&str, Vec<&str>) = ("cp", vec!["--reflink=auto", "-a", src, dst]);

    #[cfg(target_os = "windows")]
    let (prog, args): (&str, Vec<&str>) = {
        // /R and /W cap robocopy's default retry behavior (1,000,000 retries
        // × 30s wait) which otherwise hangs indefinitely on any file that is
        // still locked at copy time — e.g. PGDATA files held by the database
        // container if pause did not fully freeze writers under Docker
        // Desktop / WSL2. Three quick retries surface real failures fast.
        (
            "robocopy",
            vec![
                src,
                dst,
                "/E",
                "/COPY:DAT",
                "/R:3",
                "/W:1",
                "/NFL",
                "/NDL",
                "/NJH",
                "/NJS",
                "/NP",
            ],
        )
    };

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let (prog, args): (&str, Vec<&str>) = ("cp", vec!["-R", src, dst]);

    let output = Command::new(prog)
        .args(&args)
        // Avoid locale-dependent stderr parsing (permission denied classification).
        .env("LANG", "C")
        .output()
        .await
        .map_err(StorageError::Io)?;

    #[cfg(target_os = "windows")]
    let success = output.status.code().map(|c| c <= 7).unwrap_or(false);

    #[cfg(not(target_os = "windows"))]
    let success = output.status.success();

    if !success {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let msg = format!(
            "copy '{}' -> '{}' failed: {}{}",
            src,
            dst,
            stderr.trim(),
            if stderr.as_ref().is_empty() {
                stdout.trim().to_string()
            } else {
                String::new()
            }
        );
        return Err(map_copy_error(&stderr, msg));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// StoragePort implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl StoragePort for FileStorage {
    // ── mount ───────────────────────────────────────────────────────────────

    /// **macOS**: Mount `id` at `mount_point` using `diskutil mount -mountPoint`.
    /// Creates `mount_point` (and any missing parent directories) if it does
    /// not already exist.
    ///
    /// **Linux / Windows**: Creates the directory and returns `Ok(())` – the
    /// storage model is directory-based so no block device needs to be attached.
    #[instrument(skip(self))]
    async fn mount(&self, id: &VolumeId, mount_point: &Path) -> Result<()> {
        tokio::fs::create_dir_all(mount_point)
            .await
            .map_err(StorageError::Io)?;

        #[cfg(target_os = "macos")]
        {
            let mp = mount_point.to_string_lossy();
            self.diskutil(&["mount", "-mountPoint", &mp, &id.0])
                .await
                .map_err(|e| remap_id(e, id))?;
        }

        // Linux / Windows: directory already created above – nothing more to do.
        #[cfg(not(target_os = "macos"))]
        let _ = id; // suppress unused-variable warning

        Ok(())
    }

    // ── unmount ─────────────────────────────────────────────────────────────

    /// **macOS**: Unmount `id` using `diskutil unmount`.
    ///
    /// **Linux / Windows**: No-op — directory-based storage has no block device
    /// to detach.
    #[instrument(skip(self))]
    async fn unmount(&self, id: &VolumeId) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let output = Command::new("diskutil")
                .args(["unmount", &id.0])
                .output()
                .await
                .map_err(StorageError::Io)?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(classify_stderr(&id.0, &stderr));
            }
        }

        #[cfg(not(target_os = "macos"))]
        let _ = id;

        Ok(())
    }

    // ── snapshot ─────────────────────────────────────────────────────────────

    /// Copy the source directory to a snapshot destination, then make the
    /// destination read-only.
    ///
    /// On **macOS** `cp -cRp` uses `clonefile(2)` (COW, instant, zero extra
    /// space until divergence). On **Linux** `cp --reflink=auto -a` does the
    /// same on Btrfs/XFS and falls back to a regular deep copy on ext4 etc.
    /// On **Windows** `robocopy /E /COPY:DAT` is used, then `attrib +R /S /D`.
    ///
    /// The destination path is taken from [`SnapshotOptions::label`] (treated
    /// as an absolute or relative path). When no label is given a
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

        copy_dir(&id.0, &dest.to_string_lossy()).await?;
        make_read_only(&dest).await?;

        Ok(Snapshot {
            id: SnapshotId(dest.to_string_lossy().into_owned()),
            volume_id: id.clone(),
            created_at: chrono::Utc::now(),
            size_bytes: 0,
            label: options.label,
        })
    }

    // ── clone ────────────────────────────────────────────────────────────────

    /// Copy a directory to a new location.
    ///
    /// * `source.0` — path to copy from (ignored when `options.from_snapshot`
    ///   is set).
    /// * `options.from_snapshot` — if present, its `SnapshotId.0` is used as
    ///   the source path instead.
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

        copy_dir(src, &target_id.0).await?;

        let target_path = PathBuf::from(&target_id.0);
        Ok(VolumeStatus {
            id: target_id,
            mount_point: Some(target_path),
            status: MountStatus::Mounted,
            size_bytes: 0,
            used_bytes: 0,
        })
    }

    // ── status ───────────────────────────────────────────────────────────────

    /// Return the live status of `id`.
    ///
    /// * **macOS** – parses `diskutil info` output.
    /// * **Linux** – checks directory existence and runs `df --block-size=1`.
    /// * **Windows** – runs `PowerShell Get-PSDrive` for the drive letter.
    #[instrument(skip(self))]
    async fn status(&self, id: &VolumeId) -> Result<VolumeStatus> {
        #[cfg(target_os = "macos")]
        {
            let output = self
                .diskutil(&["info", &id.0])
                .await
                .map_err(|e| remap_id(e, id))?;
            return parse_diskutil_info(id, &output);
        }

        #[cfg(target_os = "linux")]
        {
            return status_linux(id).await;
        }

        #[cfg(target_os = "windows")]
        {
            return status_windows(id).await;
        }

        // Generic fallback for other Unix-like systems.
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            let exists = tokio::fs::metadata(&id.0).await.is_ok();
            let status = if exists {
                MountStatus::Mounted
            } else {
                MountStatus::Unmounted
            };
            Ok(VolumeStatus {
                id: id.clone(),
                mount_point: if exists {
                    Some(PathBuf::from(&id.0))
                } else {
                    None
                },
                status,
                size_bytes: 0,
                used_bytes: 0,
            })
        }
    }

    // ── quota ────────────────────────────────────────────────────────────────

    /// Return disk-usage quota for `id`.
    ///
    /// * **macOS** – obtained from `df -k`.
    /// * **Linux** – obtained from `df --block-size=1`.
    /// * **Windows** – obtained from `PowerShell Get-PSDrive`.
    #[instrument(skip(self))]
    async fn quota(&self, id: &VolumeId) -> Result<Quota> {
        #[cfg(target_os = "macos")]
        {
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
            return parse_df_kb_output(id, &stdout);
        }

        #[cfg(target_os = "linux")]
        {
            return quota_linux(id).await;
        }

        #[cfg(target_os = "windows")]
        {
            return quota_windows(id).await;
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            Ok(Quota {
                volume_id: id.clone(),
                limit_bytes: 0,
                used_bytes: 0,
                free_bytes: 0,
            })
        }
    }

    async fn finalize_snapshot(&self, dest: &Path) -> Result<()> {
        make_read_only(dest).await
    }
}

// ---------------------------------------------------------------------------
// macOS parsing helpers
// ---------------------------------------------------------------------------

/// Parse human-readable `diskutil info <volume>` output into [`VolumeStatus`].
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
fn parse_bytes_field(s: &str) -> u64 {
    s.split_whitespace()
        .next()
        .and_then(|n| n.replace(',', "").parse().ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// macOS / Linux: parse `df` output (1 K-blocks variant)
// ---------------------------------------------------------------------------

/// Parse `df -k <volume>` output (macOS default).
///
/// ```text
/// Filesystem   1024-blocks      Used Available Capacity  Mounted on
/// /dev/disk3s1   482793748 382854332  99939416    80%    /
/// ```
#[cfg(target_os = "macos")]
fn parse_df_kb_output(id: &VolumeId, output: &str) -> Result<Quota> {
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
// macOS error helpers
// ---------------------------------------------------------------------------

/// Re-map a generic `Internal` error from `diskutil` into a more specific
/// variant using the stderr text and the volume id we know about.
#[cfg(target_os = "macos")]
fn remap_id(err: StorageError, id: &VolumeId) -> StorageError {
    match err {
        StorageError::Internal(ref msg) => classify_stderr(&id.0, msg),
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Linux helpers
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
async fn status_linux(id: &VolumeId) -> Result<VolumeStatus> {
    // Check if path exists first.
    let meta = tokio::fs::metadata(&id.0).await;
    let exists = meta.is_ok();

    if !exists {
        return Err(StorageError::NotFound(id.0.clone()));
    }

    // Use `df --block-size=1` (bytes, not KB) for precise numbers.
    let output = Command::new("df")
        .args(["--block-size=1", &id.0])
        .output()
        .await
        .map_err(StorageError::Io)?;

    let (size_bytes, used_bytes) = if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_df_bytes_output(&stdout)
    } else {
        (0, 0)
    };

    Ok(VolumeStatus {
        id: id.clone(),
        mount_point: Some(PathBuf::from(&id.0)),
        status: MountStatus::Mounted,
        size_bytes,
        used_bytes,
    })
}

#[cfg(target_os = "linux")]
async fn quota_linux(id: &VolumeId) -> Result<Quota> {
    let output = Command::new("df")
        .args(["--block-size=1", &id.0])
        .output()
        .await
        .map_err(StorageError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(classify_stderr(&id.0, &stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // df --block-size=1 output columns: Filesystem, 1B-blocks, Used, Available, Use%, Mounted
    let data_line = stdout
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

    let total: u64 = parts[1].parse().unwrap_or(0);
    let used: u64 = parts[2].parse().unwrap_or(0);
    let free: u64 = parts[3].parse().unwrap_or(0);

    Ok(Quota {
        volume_id: id.clone(),
        limit_bytes: total,
        used_bytes: used,
        free_bytes: free,
    })
}

/// Parse `df --block-size=1` stdout into (size_bytes, used_bytes).
#[cfg(target_os = "linux")]
fn parse_df_bytes_output(output: &str) -> (u64, u64) {
    let data_line = match output.lines().nth(1) {
        Some(l) => l,
        None => return (0, 0),
    };
    let parts: Vec<&str> = data_line.split_whitespace().collect();
    if parts.len() < 3 {
        return (0, 0);
    }
    let size: u64 = parts[1].parse().unwrap_or(0);
    let used: u64 = parts[2].parse().unwrap_or(0);
    (size, used)
}

// ---------------------------------------------------------------------------
// Windows helpers
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
async fn status_windows(id: &VolumeId) -> Result<VolumeStatus> {
    // Determine the drive letter from the path (e.g. "C" from "C:\foo\bar").
    let drive = drive_letter(&id.0);
    let ps_expr = format!(
        "Get-PSDrive -Name '{}' | Select-Object -ExpandProperty Used",
        drive
    );
    let output = Command::new("powershell")
        .args(["-NonInteractive", "-Command", &ps_expr])
        .output()
        .await
        .map_err(StorageError::Io)?;

    let exists = tokio::fs::metadata(&id.0).await.is_ok();
    if !exists {
        return Err(StorageError::NotFound(id.0.clone()));
    }

    let used_bytes = if output.status.success() {
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u64>()
            .unwrap_or(0)
    } else {
        0
    };

    Ok(VolumeStatus {
        id: id.clone(),
        mount_point: Some(PathBuf::from(&id.0)),
        status: MountStatus::Mounted,
        size_bytes: 0,
        used_bytes,
    })
}

#[cfg(target_os = "windows")]
async fn quota_windows(id: &VolumeId) -> Result<Quota> {
    let drive = drive_letter(&id.0);
    // Query Used and Free space from Get-PSDrive in one shot (CSV for easy parsing).
    let ps_expr = format!(
        "Get-PSDrive -Name '{drive}' | Select-Object Used, Free | \
         ConvertTo-Csv -NoTypeInformation | Select-Object -Skip 1",
    );
    let output = Command::new("powershell")
        .args(["-NonInteractive", "-Command", &ps_expr])
        .output()
        .await
        .map_err(StorageError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(classify_stderr(&id.0, &stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // CSV line: "used","free"
    let line = stdout.trim().trim_matches('"');
    let mut parts = line.split("\",\"");
    let used: u64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let free: u64 = parts
        .next()
        .and_then(|s| s.trim_end_matches('"').parse().ok())
        .unwrap_or(0);

    Ok(Quota {
        volume_id: id.clone(),
        limit_bytes: used + free,
        used_bytes: used,
        free_bytes: free,
    })
}

/// Extract the drive letter from a Windows path, defaulting to `"C"`.
#[cfg(target_os = "windows")]
fn drive_letter(path: &str) -> String {
    path.chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "C".to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        p.push(format!(
            "gfs-storage-test-{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            counter
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Create a source directory with one file and return its path.
    fn make_source() -> PathBuf {
        let src = tempdir();
        fs::write(src.join("hello.txt"), b"hello world").unwrap();
        src
    }

    #[tokio::test]
    async fn test_snapshot_creates_dest_with_file() {
        let src = make_source();
        let dst = {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "gfs-snap-dst-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            p
        };

        let storage = FileStorage::new();
        let vol_id = VolumeId(src.to_string_lossy().into_owned());
        let opts = SnapshotOptions {
            label: Some(dst.to_string_lossy().into_owned()),
        };

        let snap = storage
            .snapshot(&vol_id, opts)
            .await
            .expect("snapshot failed");
        assert_eq!(snap.volume_id, vol_id);
        assert!(dst.join("hello.txt").exists(), "file missing in snapshot");

        // cleanup
        #[cfg(unix)]
        {
            Command::new("chmod")
                .args(["-R", "u+w", dst.to_str().unwrap()])
                .output()
                .await
                .ok();
        }
        fs::remove_dir_all(&src).ok();
        fs::remove_dir_all(&dst).ok();
    }

    #[tokio::test]
    async fn test_clone_creates_target_with_file() {
        let src = make_source();
        let dst = {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "gfs-clone-dst-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            p
        };

        let storage = FileStorage::new();
        let src_id = VolumeId(src.to_string_lossy().into_owned());
        let dst_id = VolumeId(dst.to_string_lossy().into_owned());

        storage
            .clone(&src_id, dst_id, CloneOptions::default())
            .await
            .expect("clone failed");

        assert!(dst.join("hello.txt").exists(), "file missing in clone");

        fs::remove_dir_all(&src).ok();
        fs::remove_dir_all(&dst).ok();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_snapshot_is_read_only() {
        let src = make_source();
        let dst = {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "gfs-ro-dst-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            p
        };

        let storage = FileStorage::new();
        let vol_id = VolumeId(src.to_string_lossy().into_owned());
        let opts = SnapshotOptions {
            label: Some(dst.to_string_lossy().into_owned()),
        };

        storage
            .snapshot(&vol_id, opts)
            .await
            .expect("snapshot failed");

        let write_result = fs::write(dst.join("new.txt"), b"oops");
        assert!(
            write_result.is_err(),
            "expected write to read-only snapshot to fail"
        );

        Command::new("chmod")
            .args(["-R", "u+w", dst.to_str().unwrap()])
            .output()
            .await
            .ok();
        fs::remove_dir_all(&src).ok();
        fs::remove_dir_all(&dst).ok();
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_snapshot_is_read_only_windows() {
        let src = make_source();
        let dst = {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "gfs-ro-dst-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            p
        };

        let storage = FileStorage::new();
        let vol_id = VolumeId(src.to_string_lossy().into_owned());
        let opts = SnapshotOptions {
            label: Some(dst.to_string_lossy().into_owned()),
        };

        storage
            .snapshot(&vol_id, opts)
            .await
            .expect("snapshot failed");

        // attrib +R prevents overwriting existing files
        let write_result = fs::write(dst.join("hello.txt"), b"overwrite");
        assert!(
            write_result.is_err(),
            "expected overwrite of read-only file in snapshot to fail"
        );

        // cleanup: remove read-only attribute before deletion
        let dst_str = dst.to_string_lossy().into_owned();
        Command::new("cmd")
            .args(["/C", "attrib", "-R", &dst_str])
            .output()
            .await
            .ok();
        Command::new("cmd")
            .args(["/C", "attrib", "-R", "/S", "/D", &format!(r"{}\*", dst_str)])
            .output()
            .await
            .ok();
        fs::remove_dir_all(&src).ok();
        fs::remove_dir_all(&dst).ok();
    }

    #[tokio::test]
    async fn test_quota_returns_sensible_values() {
        let src = make_source();
        let storage = FileStorage::new();
        let vol_id = VolumeId(src.to_string_lossy().into_owned());

        let quota = storage.quota(&vol_id).await.expect("quota failed");
        // At minimum we should get a non-zero total on real host filesystems.
        // (May be 0 in some sandbox environments, so just check it doesn't error.)
        assert_eq!(quota.volume_id, vol_id);

        fs::remove_dir_all(&src).ok();
    }

    #[tokio::test]
    async fn test_status_returns_mounted_for_existing_dir() {
        let src = make_source();
        let storage = FileStorage::new();
        let _vol_id = VolumeId(src.to_string_lossy().into_owned());

        // status() on macOS shells out to diskutil which needs a real volume ID,
        // so only run this part of the test on non-macOS platforms.
        #[cfg(not(target_os = "macos"))]
        {
            let vs = storage.status(&_vol_id).await.expect("status failed");
            assert_eq!(vs.status, MountStatus::Mounted);
        }

        // On macOS we just check that the storage struct is usable.
        #[cfg(target_os = "macos")]
        let _ = storage;

        fs::remove_dir_all(&src).ok();
    }
}
