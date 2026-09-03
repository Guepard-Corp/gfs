//! Btrfs-backed storage adapter for the [`StoragePort`] port.
//!
//! On Linux, live workspaces and snapshots are modeled as native Btrfs
//! subvolumes. Compression is applied through
//! `btrfs property set <path> compression <algo>` when configured.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use gfs_domain::model::config::{GfsConfig, StorageConfig};
use gfs_domain::ports::storage::{
    CloneOptions, MountStatus, Quota, Result, Snapshot, SnapshotId, SnapshotOptions, StorageError,
    StoragePort, VolumeId, VolumeStatus,
};
use tokio::process::Command;
use tracing::instrument;

const VALID_COMPRESSION_ALGOS: &[&str] = &["zstd", "zlib", "lzo"];
#[cfg(not(target_os = "linux"))]
const UNSUPPORTED_ERR: &str = "BtrfsStorage is only available on Linux";
#[cfg(target_os = "linux")]
const PODMAN_STORAGE_SEGMENT: &str = ".local/share/containers/storage";

#[derive(Debug, Clone)]
pub struct BtrfsStorage {
    compression: Option<String>,
    enable_reflink: bool,
}

impl Default for BtrfsStorage {
    fn default() -> Self {
        Self {
            compression: None,
            enable_reflink: true,
        }
    }
}

impl BtrfsStorage {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_repo(repo_path: &Path) -> Self {
        let storage = GfsConfig::load(repo_path)
            .ok()
            .and_then(|config| config.storage);
        Self::from_storage_config(storage.as_ref())
    }

    pub fn from_storage_config(storage: Option<&StorageConfig>) -> Self {
        Self {
            compression: storage.and_then(|cfg| cfg.compression.clone()),
            enable_reflink: storage.map(|cfg| cfg.enable_reflink).unwrap_or(true),
        }
    }

    #[cfg(target_os = "linux")]
    async fn apply_runtime_settings(&self, path: &Path) {
        if let Some(compression) = &self.compression {
            apply_compression(path, compression);
        }

        if self.enable_reflink {
            tracing::debug!("Btrfs subvolume snapshots already use native copy-on-write");
        }
    }

    #[cfg(target_os = "linux")]
    async fn ensure_live_subvolume(&self, path: &Path) -> Result<()> {
        ensure_btrfs_subvolume(path).await?;
        self.apply_runtime_settings(path).await;
        Ok(())
    }
}

fn ensure_existing_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(StorageError::NotFound(path.to_string_lossy().into_owned()));
    }

    if !path.is_dir() {
        return Err(StorageError::Internal(format!(
            "path is not a directory: {}",
            path.display()
        )));
    }

    Ok(())
}

pub fn is_btrfs(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        let output = StdCommand::new("stat")
            .args(["-f", "-c", "%T", &path.to_string_lossy()])
            .output();

        match output {
            Ok(output) if output.status.success() => {
                String::from_utf8_lossy(&output.stdout).trim() == "btrfs"
            }
            _ => false,
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        false
    }
}

pub fn apply_storage_config(path: &Path, storage: &StorageConfig) {
    #[cfg(target_os = "linux")]
    {
        if let Some(compression) = &storage.compression {
            apply_compression(path, compression);
        }

        if storage.enable_reflink {
            tracing::info!(
                "Btrfs subvolume snapshots already use native copy-on-write for {}",
                path.display()
            );
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, storage);
    }
}

#[cfg(target_os = "linux")]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(target_os = "linux")]
fn podman_storage_root() -> Option<PathBuf> {
    home_dir().map(|home| home.join(PODMAN_STORAGE_SEGMENT))
}

#[cfg(target_os = "linux")]
fn needs_podman_unshare(path: &Path) -> bool {
    podman_storage_root()
        .as_ref()
        .is_some_and(|root| path.starts_with(root))
}

#[cfg(target_os = "linux")]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(target_os = "linux")]
fn run_podman_unshare_sync(script: &str) -> std::io::Result<std::process::Output> {
    // Force LANG=C so classify_stderr's substring matches (e.g. "permission
    // denied") don't silently fail on localized hosts.
    StdCommand::new("podman")
        .args(["unshare", "sh", "-lc", script])
        .env("LANG", "C")
        .output()
}

#[cfg(target_os = "linux")]
async fn run_podman_unshare(script: &str) -> std::io::Result<std::process::Output> {
    Command::new("podman")
        .args(["unshare", "sh", "-lc", script])
        .env("LANG", "C")
        .output()
        .await
}

#[cfg(target_os = "linux")]
fn classify_output_error(volume_id: &str, output: &std::process::Output) -> StorageError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let message = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    classify_stderr(volume_id, message)
}

#[cfg(target_os = "linux")]
fn apply_compression(path: &Path, algorithm: &str) {
    let normalized = algorithm.to_lowercase();
    if !VALID_COMPRESSION_ALGOS.contains(&normalized.as_str()) {
        tracing::warn!(
            "Invalid compression algorithm '{}'. Valid: {:?}",
            algorithm,
            VALID_COMPRESSION_ALGOS
        );
        return;
    }

    let direct = StdCommand::new("btrfs")
        .args([
            "property",
            "set",
            &path.to_string_lossy(),
            "compression",
            normalized.as_str(),
        ])
        .output();

    let output = match direct {
        Ok(output) if output.status.success() => output,
        Ok(_) if needs_podman_unshare(path) => match run_podman_unshare_sync(&format!(
            "btrfs property set {} compression {}",
            shell_quote(&path.to_string_lossy()),
            shell_quote(normalized.as_str())
        )) {
            Ok(output) => output,
            Err(err) => {
                tracing::warn!("podman unshare failed while setting compression: {}", err);
                return;
            }
        },
        Ok(output) => output,
        Err(_) if needs_podman_unshare(path) => match run_podman_unshare_sync(&format!(
            "btrfs property set {} compression {}",
            shell_quote(&path.to_string_lossy()),
            shell_quote(normalized.as_str())
        )) {
            Ok(output) => output,
            Err(err) => {
                tracing::warn!("podman unshare failed while setting compression: {}", err);
                return;
            }
        },
        Err(err) => {
            tracing::warn!("btrfs command not available: {}", err);
            return;
        }
    };

    if output.status.success() {
        tracing::info!("Enabled {} compression on {}", normalized, path.display());
    } else {
        tracing::warn!(
            "Failed to set compression on {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
}

#[cfg(target_os = "linux")]
fn is_subvolume(path: &Path) -> bool {
    fn has_btrfs_root_inode(path: &Path) -> bool {
        StdCommand::new("stat")
            .args(["-c", "%i"])
            .arg(path)
            .output()
            .ok()
            .is_some_and(|output| {
                output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "256"
            })
    }

    let output = if needs_podman_unshare(path) {
        run_podman_unshare_sync(&format!(
            "btrfs subvolume show {}",
            shell_quote(&path.to_string_lossy())
        ))
        .ok()
    } else {
        StdCommand::new("btrfs")
            .args(["subvolume", "show"])
            .arg(path)
            .output()
            .ok()
    };

    match output {
        Some(output) if output.status.success() => true,
        Some(_) if is_btrfs(path) => has_btrfs_root_inode(path),
        _ => false,
    }
}

#[cfg(target_os = "linux")]
async fn run_btrfs(args: &[&str], path_for_errors: &Path) -> Result<()> {
    if needs_podman_unshare(path_for_errors) {
        let script = format!(
            "btrfs {}",
            args.iter()
                .map(|arg| shell_quote(arg))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let output = run_podman_unshare(&script)
            .await
            .map_err(StorageError::Io)?;
        if output.status.success() {
            return Ok(());
        }
        return Err(classify_output_error(
            &path_for_errors.to_string_lossy(),
            &output,
        ));
    }

    let output = Command::new("btrfs")
        .args(args)
        .env("LANG", "C")
        .output()
        .await
        .map_err(StorageError::Io)?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(classify_stderr(
        &path_for_errors.to_string_lossy(),
        stderr.trim(),
    ))
}

#[cfg(target_os = "linux")]
async fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(StorageError::Io)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn create_subvolume(path: &Path) -> Result<()> {
    ensure_parent_dir(path).await?;
    run_btrfs(&["subvolume", "create", &path.to_string_lossy()], path).await
}

#[cfg(target_os = "linux")]
async fn snapshot_subvolume(source: &Path, dest: &Path, read_only: bool) -> Result<()> {
    ensure_parent_dir(dest).await?;

    if needs_podman_unshare(source) || needs_podman_unshare(dest) {
        let mut command = String::from("btrfs subvolume snapshot");
        if read_only {
            command.push_str(" -r");
        }
        command.push(' ');
        command.push_str(&shell_quote(&source.to_string_lossy()));
        command.push(' ');
        command.push_str(&shell_quote(&dest.to_string_lossy()));

        let output = run_podman_unshare(&command)
            .await
            .map_err(StorageError::Io)?;
        if output.status.success() {
            return Ok(());
        }
        return Err(classify_output_error(&dest.to_string_lossy(), &output));
    }

    let mut args = vec!["subvolume", "snapshot"];
    if read_only {
        args.push("-r");
    }
    let source_path = source.to_string_lossy();
    let dest_path = dest.to_string_lossy();
    args.push(source_path.as_ref());
    args.push(dest_path.as_ref());

    run_btrfs(&args, dest).await
}

#[cfg(target_os = "linux")]
async fn delete_subvolume(path: &Path) -> Result<()> {
    run_btrfs(&["subvolume", "delete", &path.to_string_lossy()], path).await
}

#[cfg(target_os = "linux")]
async fn copy_dir_contents(src: &Path, dst: &Path) -> Result<()> {
    if needs_podman_unshare(src) || needs_podman_unshare(dst) {
        let script = format!(
            "cp --reflink=auto -a {}/. {}",
            shell_quote(&src.to_string_lossy()),
            shell_quote(&dst.to_string_lossy())
        );
        let output = run_podman_unshare(&script)
            .await
            .map_err(StorageError::Io)?;
        if output.status.success() {
            return Ok(());
        }
        return Err(classify_output_error(&src.to_string_lossy(), &output));
    }

    let source = src.join(".");
    let output = Command::new("cp")
        .args(["--reflink=auto", "-a"])
        .arg(&source)
        .arg(dst)
        .env("LANG", "C")
        .output()
        .await
        .map_err(StorageError::Io)?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let msg = format!(
        "copy '{}' -> '{}' failed: {}{}",
        src.display(),
        dst.display(),
        stderr.trim(),
        if stderr.as_ref().is_empty() {
            stdout.trim().to_string()
        } else {
            String::new()
        }
    );
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("permission denied") || lower.contains("operation not permitted") {
        return Err(StorageError::PermissionDenied(msg));
    }
    Err(StorageError::Internal(msg))
}

#[cfg(target_os = "linux")]
fn unique_migration_path(path: &Path, suffix: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("data");
    path.with_file_name(format!(".{name}.gfs-{suffix}-{stamp}"))
}

#[cfg(target_os = "linux")]
async fn migrate_directory_to_subvolume(path: &Path) -> Result<()> {
    let tmp_subvolume = unique_migration_path(path, "subvolume");
    let backup_dir = unique_migration_path(path, "backup");

    if needs_podman_unshare(path) {
        let script = format!(
            "btrfs subvolume create {} && cp --reflink=auto -a {}/. {} && mv {} {} && mv {} {} && rm -rf {}",
            shell_quote(&tmp_subvolume.to_string_lossy()),
            shell_quote(&path.to_string_lossy()),
            shell_quote(&tmp_subvolume.to_string_lossy()),
            shell_quote(&path.to_string_lossy()),
            shell_quote(&backup_dir.to_string_lossy()),
            shell_quote(&tmp_subvolume.to_string_lossy()),
            shell_quote(&path.to_string_lossy()),
            shell_quote(&backup_dir.to_string_lossy())
        );
        let output = run_podman_unshare(&script)
            .await
            .map_err(StorageError::Io)?;
        if output.status.success() {
            return Ok(());
        }
        return Err(classify_output_error(&path.to_string_lossy(), &output));
    }

    create_subvolume(&tmp_subvolume).await?;

    let migration_result = async {
        copy_dir_contents(path, &tmp_subvolume).await?;
        tokio::fs::rename(path, &backup_dir)
            .await
            .map_err(StorageError::Io)?;
        tokio::fs::rename(&tmp_subvolume, path)
            .await
            .map_err(StorageError::Io)?;
        tokio::fs::remove_dir_all(&backup_dir)
            .await
            .map_err(StorageError::Io)?;
        Ok(())
    }
    .await;

    if migration_result.is_ok() {
        return Ok(());
    }

    let _ = tokio::fs::rename(&backup_dir, path).await;
    let _ = delete_subvolume(&tmp_subvolume).await;
    migration_result
}

#[cfg(target_os = "linux")]
async fn ensure_btrfs_subvolume(path: &Path) -> Result<()> {
    if !path.exists() {
        return create_subvolume(path).await;
    }

    if is_subvolume(path) {
        return Ok(());
    }

    if !path.is_dir() {
        return Err(StorageError::Internal(format!(
            "path is not a directory: {}",
            path.display()
        )));
    }

    migrate_directory_to_subvolume(path).await
}

#[cfg(target_os = "linux")]
async fn status_linux(id: &VolumeId) -> Result<VolumeStatus> {
    if tokio::fs::metadata(&id.0).await.is_err() {
        return Err(StorageError::NotFound(id.0.clone()));
    }

    let output = Command::new("df")
        .args(["--block-size=1", &id.0])
        .output()
        .await
        .map_err(StorageError::Io)?;

    let (size_bytes, used_bytes) = if output.status.success() {
        parse_df_bytes_output(&String::from_utf8_lossy(&output.stdout))
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

    Ok(Quota {
        volume_id: id.clone(),
        limit_bytes: parts[1].parse().unwrap_or(0),
        used_bytes: parts[2].parse().unwrap_or(0),
        free_bytes: parts[3].parse().unwrap_or(0),
    })
}

#[cfg(target_os = "linux")]
fn parse_df_bytes_output(output: &str) -> (u64, u64) {
    let Some(data_line) = output.lines().nth(1) else {
        return (0, 0);
    };
    let parts: Vec<&str> = data_line.split_whitespace().collect();
    if parts.len() < 3 {
        return (0, 0);
    }
    (parts[1].parse().unwrap_or(0), parts[2].parse().unwrap_or(0))
}

fn classify_stderr(volume_id: &str, stderr: &str) -> StorageError {
    let lower = stderr.to_lowercase();
    if lower.contains("no such")
        || lower.contains("not found")
        || lower.contains("cannot find")
        || lower.contains("does not exist")
    {
        StorageError::NotFound(volume_id.to_owned())
    } else if lower.contains("busy") || lower.contains("in use") || lower.contains("being used") {
        StorageError::Busy(volume_id.to_owned())
    } else if lower.contains("already exists") || lower.contains("already exist") {
        StorageError::AlreadyExists(volume_id.to_owned())
    } else if lower.contains("permission denied") || lower.contains("operation not permitted") {
        // Surface as PermissionDenied so the commit use case can trigger the
        // stream_snapshot fallback. Without this branch, rootful btrfs with
        // UID-mismatched data dirs silently wraps the cp/subvolume failure as
        // Internal, the `storage_error_looks_like_permission_denied` heuristic
        // ignores it, and the commit fails hard instead of falling back.
        StorageError::PermissionDenied(stderr.trim().to_owned())
    } else {
        StorageError::Internal(stderr.trim().to_owned())
    }
}

#[cfg(not(target_os = "linux"))]
fn unsupported() -> StorageError {
    StorageError::Internal(UNSUPPORTED_ERR.to_string())
}

#[async_trait]
impl StoragePort for BtrfsStorage {
    #[instrument(skip(self))]
    async fn mount(&self, _id: &VolumeId, mount_point: &Path) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            tokio::fs::create_dir_all(mount_point)
                .await
                .map_err(StorageError::Io)?;
            return Ok(());
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = mount_point;
            Err(unsupported())
        }
    }

    #[instrument(skip(self))]
    async fn unmount(&self, _id: &VolumeId) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            Ok(())
        }

        #[cfg(not(target_os = "linux"))]
        {
            Err(unsupported())
        }
    }

    #[instrument(skip(self))]
    async fn snapshot(&self, id: &VolumeId, options: SnapshotOptions) -> Result<Snapshot> {
        #[cfg(target_os = "linux")]
        {
            let source = Path::new(&id.0);
            ensure_existing_directory(source)?;
            self.ensure_live_subvolume(source).await?;

            let dest = match &options.label {
                Some(label) => PathBuf::from(label),
                None => {
                    let name = format!("snap-{}", chrono::Utc::now().format("%Y%m%dT%H%M%SZ"));
                    source
                        .parent()
                        .map(|parent| parent.join(&name))
                        .unwrap_or_else(|| PathBuf::from(&name))
                }
            };

            // Keep snapshots user-deletable with normal tools (`rm -rf`, file managers).
            // Btrfs read-only snapshots require privileged subvolume deletion on some mounts.
            snapshot_subvolume(source, &dest, false).await?;
            self.apply_runtime_settings(&dest).await;

            return Ok(Snapshot {
                id: SnapshotId(dest.to_string_lossy().into_owned()),
                volume_id: id.clone(),
                created_at: chrono::Utc::now(),
                size_bytes: 0,
                label: options.label,
            });
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = (id, options);
            Err(unsupported())
        }
    }

    #[instrument(skip(self))]
    async fn clone(
        &self,
        source: &VolumeId,
        target_id: VolumeId,
        options: CloneOptions,
    ) -> Result<VolumeStatus> {
        #[cfg(target_os = "linux")]
        {
            let src = options
                .from_snapshot
                .as_ref()
                .map(|snapshot| PathBuf::from(&snapshot.0))
                .unwrap_or_else(|| PathBuf::from(&source.0));

            ensure_existing_directory(&src)?;

            if options.from_snapshot.is_none() {
                self.ensure_live_subvolume(&src).await?;
            }

            snapshot_subvolume(&src, Path::new(&target_id.0), false).await?;
            self.apply_runtime_settings(Path::new(&target_id.0)).await;

            return Ok(VolumeStatus {
                id: target_id.clone(),
                mount_point: Some(PathBuf::from(&target_id.0)),
                status: MountStatus::Mounted,
                size_bytes: 0,
                used_bytes: 0,
            });
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = (source, target_id, options);
            Err(unsupported())
        }
    }

    #[instrument(skip(self))]
    async fn status(&self, id: &VolumeId) -> Result<VolumeStatus> {
        #[cfg(target_os = "linux")]
        {
            return status_linux(id).await;
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = id;
            Err(unsupported())
        }
    }

    #[instrument(skip(self))]
    async fn quota(&self, id: &VolumeId) -> Result<Quota> {
        #[cfg(target_os = "linux")]
        {
            return quota_linux(id).await;
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = id;
            Err(unsupported())
        }
    }

    async fn finalize_snapshot(&self, dest: &Path) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            let output = Command::new("chmod")
                .args(["-R", "u+rX,u-w,go-rwx"])
                .arg(dest)
                .env("LANG", "C")
                .output()
                .await
                .map_err(StorageError::Io)?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(StorageError::Internal(format!(
                    "chmod -R u+rX,u-w,go-rwx '{}' failed: {}",
                    dest.display(),
                    stderr.trim()
                )));
            }
            return Ok(());
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = dest;
            Err(unsupported())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_defaults_to_reflinks() {
        let storage = BtrfsStorage::new();
        assert!(storage.enable_reflink);
        assert!(storage.compression.is_none());
    }

    #[test]
    fn storage_reads_repo_config_shape() {
        let storage = BtrfsStorage::from_storage_config(Some(&StorageConfig {
            compression: Some("zstd".into()),
            enable_reflink: false,
        }));

        assert_eq!(storage.compression.as_deref(), Some("zstd"));
        assert!(!storage.enable_reflink);
    }

    #[test]
    fn missing_source_directory_is_rejected() {
        let missing = std::env::temp_dir().join(format!(
            "gfs-btrfs-missing-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let err = ensure_existing_directory(&missing).unwrap_err();

        assert!(matches!(err, StorageError::NotFound(path) if path == missing.to_string_lossy()));
    }

    #[test]
    fn non_directory_source_is_rejected() {
        let file_path = std::env::temp_dir().join(format!(
            "gfs-btrfs-file-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&file_path, b"test").unwrap();

        let err = ensure_existing_directory(&file_path).unwrap_err();

        assert!(
            matches!(err, StorageError::Internal(message) if message.contains("path is not a directory"))
        );

        let _ = std::fs::remove_file(&file_path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn regular_directory_is_not_detected_as_subvolume() {
        let regular_dir = std::env::temp_dir().join(format!(
            "gfs-btrfs-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&regular_dir).unwrap();

        assert!(!is_subvolume(&regular_dir));

        let _ = std::fs::remove_dir_all(&regular_dir);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn snapshots_are_deletable_with_regular_directory_removal() {
        let base = std::env::current_dir().unwrap().join(format!(
            ".gfs-btrfs-delete-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        if !is_btrfs(base.parent().unwrap_or_else(|| Path::new("."))) {
            return;
        }

        let source = base.join("source");
        let snapshot = base.join("snapshot");

        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("data.txt"), b"data").unwrap();

        let storage = BtrfsStorage::new();
        let result = storage
            .snapshot(
                &VolumeId(source.to_string_lossy().into_owned()),
                SnapshotOptions {
                    label: Some(snapshot.to_string_lossy().into_owned()),
                },
            )
            .await;

        if matches!(result, Err(StorageError::Internal(ref message)) if message.contains("Operation not permitted"))
        {
            let _ = std::fs::remove_dir_all(&base);
            return;
        }

        result.unwrap();

        std::fs::remove_dir_all(&snapshot).unwrap();
        assert!(!snapshot.exists());

        let _ = std::fs::remove_dir_all(&source);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Permission-denied failures from `btrfs subvolume snapshot/create/delete`
    /// must surface as [`StorageError::PermissionDenied`] so the commit use case
    /// can trigger the `stream_snapshot` fallback. Regression test for a bug
    /// where the classifier wrapped these as `Internal` and the fallback never
    /// fired on rootful btrfs with UID-mismatched data dirs.
    #[test]
    fn classify_stderr_maps_permission_denied() {
        // Real-world btrfs-progs stderr strings observed on Linux 6.x, plus
        // the lower-case `operation not permitted` variant that surfaces via
        // `podman unshare sh -lc` wrappers.
        let samples = [
            "ERROR: cannot snapshot '/data': Permission denied",
            "ERROR: Could not create subvolume: Operation not permitted",
            "ERROR: cannot delete '/data': Permission denied",
            "cp: cannot open '/data/pg_control' for reading: Permission denied",
        ];
        for stderr in samples {
            let err = classify_stderr("/vol/x", stderr);
            assert!(
                matches!(err, StorageError::PermissionDenied(_)),
                "expected PermissionDenied for stderr {stderr:?}, got {err:?}"
            );
        }
    }

    /// Non-permission errors must NOT get swallowed into PermissionDenied —
    /// the fallback path is expensive (tar-streaming the whole data dir) and
    /// must only trigger on the right failure class.
    #[test]
    fn classify_stderr_leaves_unrelated_errors_as_internal() {
        let samples = [
            "ERROR: no space left on device",
            "ERROR: input/output error during snapshot",
            "ERROR: quotacheck failed",
        ];
        for stderr in samples {
            let err = classify_stderr("/vol/x", stderr);
            assert!(
                matches!(err, StorageError::Internal(_)),
                "expected Internal for stderr {stderr:?}, got {err:?}"
            );
        }
    }
}
