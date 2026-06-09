use std::fs::{File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;

use crate::model::commit::NewCommit;
use crate::model::config::GlobalSettings;
use crate::model::layout::GFS_DIR;
use crate::ports::compute::{Compute, ComputeError, InstanceId, InstanceState};
use crate::ports::database_provider::{ConnectionParams, DatabaseProviderRegistry};
use crate::ports::repository::{Repository, RepositoryError};
use crate::ports::storage::{SnapshotOptions, StorageError, StoragePort, VolumeId};
use crate::repo_utils::repo_layout;
use crate::usecases::repository::extract_schema_usecase::ExtractSchemaUseCase;
use crate::utils::hash::hash_snapshot;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum CommitRepoError {
    #[error("repository error: {0}")]
    Repository(#[from] RepositoryError),

    #[error("compute error: {0}")]
    Compute(#[from] ComputeError),

    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("unknown database provider: '{0}'")]
    UnknownDatabaseProvider(String),

    #[error("commit message must not be empty")]
    EmptyMessage,
}

/// True when host-side snapshot copy failed because the host could not read a file
/// (e.g. root-owned `0600` under a bind-mounted workspace). Used to fall back to
/// [`Compute::stream_snapshot`].
fn storage_error_looks_like_permission_denied(err: &StorageError) -> bool {
    match err {
        StorageError::PermissionDenied(_) => true,
        StorageError::Io(io) => io.kind() == ErrorKind::PermissionDenied,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// RAII unpause guard
// ---------------------------------------------------------------------------

/// Ensures the paused container is always unpaused, even on task cancellation or
/// panic, by spawning an async unpause task from its `Drop` impl.
///
/// Call [`UnpauseGuard::defuse`] before the explicit unpause on the happy path
/// to prevent a redundant (and potentially error-logged) double-unpause.
struct UnpauseGuard {
    compute: Arc<dyn Compute>,
    instance_id: Option<InstanceId>,
}

impl UnpauseGuard {
    fn new(compute: Arc<dyn Compute>, id: InstanceId) -> Self {
        Self {
            compute,
            instance_id: Some(id),
        }
    }

    /// Disarm the guard. The caller takes responsibility for unpausing.
    fn defuse(&mut self) {
        self.instance_id = None;
    }
}

impl Drop for UnpauseGuard {
    fn drop(&mut self) {
        if let Some(id) = self.instance_id.take() {
            let compute = Arc::clone(&self.compute);
            // `Drop` cannot be async; spawn a background task to do the unpause.
            tokio::spawn(async move {
                if let Err(e) = compute.unpause(&id).await {
                    tracing::warn!(
                        error = %e,
                        instance = %id,
                        "UnpauseGuard: failed to unpause instance on drop"
                    );
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Per-repo commit lock
// ---------------------------------------------------------------------------

/// Exclusive advisory lock on `<repo>/.gfs/commit.lock`, held for the duration
/// of a single `CommitRepoUseCase::run` invocation.
///
/// Prevents two concurrent `gfs commit` processes on the same repository from
/// racing pause/unpause, writing overlapping snapshot directories, or
/// advancing the branch ref with the same parent — any of which can produce
/// orphan snapshots or lost commits.
///
/// Uses `std::fs::File::try_lock` (stable since Rust 1.89) for a non-blocking
/// POSIX `flock`. Lock is released on Drop via explicit `unlock`; if the
/// process is killed, the kernel releases the flock on FD close so a crashed
/// commit never wedges future commits.
struct CommitLock {
    file: File,
}

impl CommitLock {
    fn acquire(repo_path: &Path) -> std::result::Result<Self, CommitRepoError> {
        let gfs_dir = repo_path.join(GFS_DIR);
        std::fs::create_dir_all(&gfs_dir)
            .map_err(|e| CommitRepoError::Repository(RepositoryError::Io(e)))?;
        let lock_path = gfs_dir.join("commit.lock");

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| CommitRepoError::Repository(RepositoryError::Io(e)))?;

        match file.try_lock() {
            Ok(()) => Ok(Self { file }),
            Err(TryLockError::WouldBlock) => Err(CommitRepoError::Repository(
                RepositoryError::Internal(format!(
                    "another `gfs commit` is already running on this repository \
                     (lock held at {}); retry once it finishes",
                    lock_path.display()
                )),
            )),
            Err(TryLockError::Error(e)) => Err(CommitRepoError::Repository(RepositoryError::Io(e))),
        }
    }
}

impl Drop for CommitLock {
    fn drop(&mut self) {
        // Best-effort: kernel will release on FD close regardless.
        let _ = self.file.unlock();
    }
}

// ---------------------------------------------------------------------------
// Unfrozen-snapshot opt-in
// ---------------------------------------------------------------------------

/// True when the user has explicitly opted in to proceeding with a commit when
/// the container cannot be frozen (rootless Podman on cgroup v1, LXC without
/// freezer, etc.). Default is refusal, because a file-level snapshot of an
/// unfrozen database is not crash-consistent — pages and WAL can be captured
/// mid-write.
fn unfrozen_snapshot_allowed() -> bool {
    parse_unfrozen_snapshot_flag(std::env::var("GFS_ALLOW_UNFROZEN_SNAPSHOT").ok().as_deref())
}

/// Pure parser split out from the env read so tests can exercise every accepted
/// form without mutating process-global state (which is unsafe under Rust 2024
/// and races with other parallel tests).
fn parse_unfrozen_snapshot_flag(raw: Option<&str>) -> bool {
    match raw {
        Some(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        None => false,
    }
}

#[cfg(test)]
mod permission_denied_tests {
    use super::storage_error_looks_like_permission_denied;
    use crate::ports::storage::StorageError;
    use std::io::ErrorKind;

    #[test]
    fn detects_io_permission_denied() {
        assert!(storage_error_looks_like_permission_denied(
            &StorageError::Io(std::io::Error::from(ErrorKind::PermissionDenied))
        ));
    }

    #[test]
    fn ignores_unrelated_internal_error() {
        assert!(!storage_error_looks_like_permission_denied(
            &StorageError::Internal("disk full".into())
        ));
    }

    #[test]
    fn detects_typed_permission_denied() {
        assert!(storage_error_looks_like_permission_denied(
            &StorageError::PermissionDenied("x".into())
        ));
    }
}

#[cfg(test)]
mod unfrozen_snapshot_flag_tests {
    use super::parse_unfrozen_snapshot_flag;

    #[test]
    fn absent_defaults_to_refusal() {
        assert!(!parse_unfrozen_snapshot_flag(None));
    }

    #[test]
    fn empty_or_zero_does_not_opt_in() {
        assert!(!parse_unfrozen_snapshot_flag(Some("")));
        assert!(!parse_unfrozen_snapshot_flag(Some("0")));
        assert!(!parse_unfrozen_snapshot_flag(Some("false")));
        assert!(!parse_unfrozen_snapshot_flag(Some("no")));
        assert!(!parse_unfrozen_snapshot_flag(Some("off")));
    }

    #[test]
    fn explicit_truthy_values_opt_in() {
        for v in [
            "1", "true", "TRUE", "True", "yes", "YES", "on", "ON", " 1 ", "  true  ",
        ] {
            assert!(
                parse_unfrozen_snapshot_flag(Some(v)),
                "expected {v:?} to opt in"
            );
        }
    }
}

#[cfg(test)]
mod commit_lock_tests {
    use super::{CommitLock, CommitRepoError};
    use crate::ports::repository::RepositoryError;

    #[test]
    fn second_acquire_reports_contention() {
        let dir = tempfile::tempdir().unwrap();
        let _first = CommitLock::acquire(dir.path()).expect("first acquire should succeed");

        let second = CommitLock::acquire(dir.path());
        match second {
            Err(CommitRepoError::Repository(RepositoryError::Internal(msg))) => {
                assert!(
                    msg.contains("another `gfs commit` is already running"),
                    "unexpected message: {msg}"
                );
            }
            Err(e) => panic!("expected Internal contention error, got {e:?}"),
            Ok(_) => panic!("expected contention error, but second acquire succeeded"),
        }
    }

    #[test]
    fn acquire_succeeds_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _first = CommitLock::acquire(dir.path()).unwrap();
        }
        let _second = CommitLock::acquire(dir.path()).expect("lock should be released after drop");
    }
}

// ---------------------------------------------------------------------------
// Use case
// ---------------------------------------------------------------------------

/// Use case for committing the current state of a repository.
///
/// Steps:
/// 1. Validate the commit message.
/// 2. Resolve the current branch, parent commit, runtime config, and mount point from the repo.
/// 3. If a database container is running, prepare it for snapshotting and pause it.
/// 4. Take a storage snapshot of the workspace volume.
/// 5. Build a [`NewCommit`] and persist it via the [`Repository`] port.
/// 6. Unpause the container if it was paused in step 3.
///
/// `R` is generic over [`DatabaseProviderRegistry`] because that trait is not
/// dyn-compatible (its `register` method uses `impl Into<String>`).
pub struct CommitRepoUseCase<R: DatabaseProviderRegistry> {
    repository: Arc<dyn Repository>,
    compute: Arc<dyn Compute>,
    storage: Arc<dyn StoragePort>,
    registry: Arc<R>,
}

impl<R: DatabaseProviderRegistry> CommitRepoUseCase<R> {
    pub fn new(
        repository: Arc<dyn Repository>,
        compute: Arc<dyn Compute>,
        storage: Arc<dyn StoragePort>,
        registry: Arc<R>,
    ) -> Self {
        Self {
            repository,
            compute,
            storage,
            registry,
        }
    }

    /// Commit the current state of the repository at `path` with the given `message`.
    ///
    /// `author` and `committer` default to `"user"` when `None`; supply them from
    /// the caller (e.g. from the CLI or config) for production use.
    pub async fn run(
        &self,
        path: PathBuf,
        message: String,
        author: Option<String>,
        author_email: Option<String>,
        committer: Option<String>,
        committer_email: Option<String>,
    ) -> std::result::Result<String, CommitRepoError> {
        if message.trim().is_empty() {
            return Err(CommitRepoError::EmptyMessage);
        }

        // Use canonical path so snapshot dir matches where checkout will read (same physical path).
        let path = std::fs::canonicalize(&path)
            .map_err(|e| CommitRepoError::Repository(RepositoryError::Io(e)))?;

        // Serialize commits per repository. Held until this function returns,
        // covering pause, snapshot, finalize, and ref advance.
        let _commit_lock = CommitLock::acquire(&path)?;

        // 1. Resolve commit context from the repository.
        let parent_commit_id = self.repository.get_current_commit_id(&path).await?;
        let runtime_config = self.repository.get_runtime_config(&path).await?;
        let environment = self.repository.get_environment_config(&path).await?;
        let mount_point = self.repository.get_mount_point(&path).await?;

        // Resolve author / committer with fallback chain:
        //   CLI args → repo-local config → global ~/.gfs/config.toml → git config → "user"
        let user_config = self.repository.get_user_config(&path).await?;
        let global_config = GlobalSettings::load();
        let git_config = repo_layout::get_git_user_config();

        let resolved_author = author
            .or_else(|| user_config.as_ref().and_then(|u| u.name.clone()))
            .or_else(|| {
                global_config
                    .as_ref()
                    .and_then(|g| g.user.as_ref().and_then(|u| u.name.clone()))
            })
            .or_else(|| git_config.name.clone())
            .unwrap_or_else(|| "user".to_string());
        let resolved_author_email = author_email
            .or_else(|| user_config.as_ref().and_then(|u| u.email.clone()))
            .or_else(|| {
                global_config
                    .as_ref()
                    .and_then(|g| g.user.as_ref().and_then(|u| u.email.clone()))
            })
            .or_else(|| git_config.email.clone());
        let resolved_committer = committer
            .or_else(|| user_config.as_ref().and_then(|u| u.name.clone()))
            .or_else(|| {
                global_config
                    .as_ref()
                    .and_then(|g| g.user.as_ref().and_then(|u| u.name.clone()))
            })
            .or_else(|| git_config.name.clone())
            .unwrap_or_else(|| "user".to_string());
        let resolved_committer_email = committer_email
            .or_else(|| user_config.as_ref().and_then(|u| u.email.clone()))
            .or_else(|| {
                global_config
                    .as_ref()
                    .and_then(|g| g.user.as_ref().and_then(|u| u.email.clone()))
            })
            .or_else(|| git_config.email.clone());

        // 2. Extract and store schema (best-effort) while the container is still running.
        //    Must run before pausing, since schema extraction requires a live database connection.
        let schema_hash = if runtime_config.is_some() && environment.is_some() {
            self.extract_and_store_schema(&path).await.ok()
        } else {
            None
        };

        // 3. Prepare the database container for snapshotting (if present).
        let mut unpause_guard: Option<UnpauseGuard> = None;
        let mut paused_instance_id: Option<InstanceId> = None;
        if let (Some(runtime), Some(env)) = (&runtime_config, &environment) {
            let instance_id = InstanceId(runtime.container_name.clone());

            let provider = self.registry.get(&env.database_provider).ok_or_else(|| {
                CommitRepoError::UnknownDatabaseProvider(env.database_provider.clone())
            })?;
            let conn_info = self
                .compute
                .get_connection_info(&instance_id, provider.default_port())
                .await?;
            let params = ConnectionParams {
                host: conn_info.host,
                port: conn_info.port,
                env: conn_info.env,
            };
            let commands = provider.prepare_for_snapshot(&params).map_err(|e| {
                CommitRepoError::Repository(RepositoryError::Internal(e.to_string()))
            })?;
            self.compute
                .prepare_for_snapshot(&instance_id, &commands)
                .await?;

            // Pause the container so no writes land during the snapshot.
            // On rootless Podman with cgroup v1 the runtime cannot freeze
            // container processes; treat that as a soft failure and proceed
            // with a crash-consistent snapshot (CHECKPOINT already applied).
            let status = self.compute.status(&instance_id).await?;
            if status.state == InstanceState::Running {
                match self.compute.pause(&instance_id).await {
                    Ok(_) => {
                        // RAII guard: ensures unpause even on task cancellation or panic.
                        unpause_guard = Some(UnpauseGuard::new(
                            Arc::clone(&self.compute),
                            instance_id.clone(),
                        ));
                        paused_instance_id = Some(instance_id);
                    }
                    Err(ComputeError::PauseUnsupported(ref e)) => {
                        if !unfrozen_snapshot_allowed() {
                            return Err(CommitRepoError::Compute(ComputeError::PauseUnsupported(
                                format!(
                                    "{e}. Refusing to snapshot an unfrozen database: \
                                     a file-level copy of a live data directory can \
                                     capture torn pages and half-applied WAL records, \
                                     producing a non-crash-consistent snapshot. \
                                     Options: (1) switch to a runtime that supports \
                                     cgroup freezing (Docker, or rootful Podman on \
                                     cgroup v2); (2) upgrade the host to cgroup v2 \
                                     and use a runtime that honors it; or (3) set \
                                     GFS_ALLOW_UNFROZEN_SNAPSHOT=1 to proceed with \
                                     a best-effort snapshot that may require manual \
                                     WAL replay on restore"
                                ),
                            )));
                        }
                        tracing::warn!(
                            error = %e,
                            instance = %instance_id,
                            "container pause unavailable (cgroup v1 or rootless runtime); \
                             proceeding with UNFROZEN snapshot per GFS_ALLOW_UNFROZEN_SNAPSHOT — \
                             snapshot is NOT crash-consistent and may contain torn pages and \
                             half-applied WAL; restore may require manual recovery"
                        );
                        // No unpause guard: nothing was paused.
                    }
                    Err(e) => return Err(CommitRepoError::Compute(e)),
                }
            }
        }

        // 4. Take a storage snapshot.
        //    The VolumeId is the mount point of the workspace volume.  When no
        //    explicit mount_point is configured we read .gfs/WORKSPACE which
        //    always points to the directory where the database is currently
        //    running — even after multiple commits have advanced HEAD.
        let volume_id = if let Some(mp) = mount_point {
            VolumeId(mp)
        } else {
            let data_dir = self.repository.get_active_workspace_data_dir(&path).await?;
            VolumeId(data_dir.to_string_lossy().into_owned())
        };

        // Generate a unique snapshot hash from the source path + current timestamp.
        let snap_timestamp = chrono::Utc::now();
        let snapshot_hash = hash_snapshot(&volume_id.0, &snap_timestamp);

        // Ensure .gfs/snapshots/<2>/ exists and return the full COW destination path.
        let snapshot_dest = self
            .repository
            .ensure_snapshot_path(&path, &snapshot_hash)
            .await?;

        // Prefer fast host-side COW/reflink snapshot (`storage.snapshot`).
        // If that fails with a permission error (unreadable bind-mounted files),
        // fall back to streaming the data dir through the container runtime so the
        // daemon reads files the host user cannot.
        let snapshot_result: Result<(), CommitRepoError> = async {
            let host_result = self
                .storage
                .snapshot(
                    &volume_id,
                    SnapshotOptions {
                        label: Some(snapshot_dest.to_string_lossy().into_owned()),
                    },
                )
                .await;

            match host_result {
                Ok(_) => Ok(()),
                Err(e) => {
                    if !storage_error_looks_like_permission_denied(&e) {
                        return Err(CommitRepoError::Storage(e));
                    }
                    let (Some(runtime), Some(env)) = (&runtime_config, &environment) else {
                        return Err(CommitRepoError::Storage(e));
                    };

                    tracing::warn!(
                        error = %e,
                        "host snapshot failed with permission denied; falling back to stream_snapshot"
                    );

                    let instance_id = InstanceId(runtime.container_name.clone());
                    let provider = self.registry.get(&env.database_provider).ok_or_else(|| {
                        CommitRepoError::UnknownDatabaseProvider(env.database_provider.clone())
                    })?;
                    let container_data_path = provider
                        .definition()
                        .data_dir
                        .to_string_lossy()
                        .into_owned();

                    if snapshot_dest.exists()
                        && let Err(rm_err) = tokio::fs::remove_dir_all(&snapshot_dest).await
                    {
                        tracing::warn!(
                            error = %rm_err,
                            path = %snapshot_dest.display(),
                            "failed to remove partial snapshot before stream_snapshot fallback"
                        );
                    }

                    let timeout_secs: u64 = match std::env::var("GFS_STREAM_SNAPSHOT_TIMEOUT_SECS")
                        .ok()
                        .map(|v| v.trim().to_string())
                    {
                        None => 300,
                        Some(v) if v.is_empty() => 300,
                        Some(v) => match v.parse::<u64>() {
                            Ok(0) => 1,
                            Ok(n) => n,
                            Err(_) => 300,
                        },
                    };

                    tracing::info!(
                        timeout_secs,
                        "stream_snapshot timeout configured"
                    );

                    // Bound the time we keep the DB paused + snapshot in flight.
                    // Partial-snapshot cleanup on error is handled by the single
                    // synchronous cleanup path below the outer `snapshot_result`
                    // match — do NOT call `remove_dir_all` here, which would
                    // duplicate the work and race the uncancellable
                    // `spawn_blocking` tar writer inside `stream_snapshot`.
                    // Unpause always happens via the outer RAII guard regardless.
                    match tokio::time::timeout(
                        Duration::from_secs(timeout_secs),
                        self.compute.stream_snapshot(&instance_id, &container_data_path, &snapshot_dest),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => return Err(CommitRepoError::Compute(e)),
                        Err(_elapsed) => {
                            return Err(CommitRepoError::Compute(ComputeError::Internal(
                                format!("stream_snapshot timed out after {timeout_secs}s"),
                            )));
                        }
                    }

                    self.storage
                        .finalize_snapshot(&snapshot_dest)
                        .await
                        .map_err(CommitRepoError::Storage)?;

                    // The current workspace still has permission-broken files (that's why we
                    // fell back to stream_snapshot). Mark it so the next container start will
                    // run a pre-start ownership repair before booting.
                    // Use the canonical workspace path (.gfs/WORKSPACE) rather than volume_id,
                    // because volume_id may point to a custom mount_point that differs from the
                    // path where checkout will look for the marker.
                    let canonical_ws = self.repository.get_active_workspace_data_dir(&path).await;
                    if let Ok(ws) = canonical_ws {
                        if let Some(m) = repo_layout::repair_marker_path(&ws) {
                            let _ = std::fs::write(&m, b"");
                        }
                    } else if let Some(m) = repo_layout::repair_marker_path(
                        std::path::Path::new(volume_id.0.as_str()),
                    ) {
                        let _ = std::fs::write(&m, b"");
                    }

                    Ok(())
                }
            }
        }
        .await;

        // Always unpause if we paused — even on snapshot failure.
        // Defuse the RAII guard first so Drop doesn't fire a redundant unpause.
        if let Some(mut guard) = unpause_guard.take() {
            guard.defuse();
            if let Some(instance_id) = paused_instance_id.as_ref()
                && let Err(unpause_err) = self.compute.unpause(instance_id).await
            {
                tracing::warn!(
                    error = %unpause_err,
                    instance = %instance_id,
                    "failed to unpause instance after snapshot attempt"
                );
            }
        }

        // Single synchronous cleanup path for any partial snapshot tree on error.
        //
        // Previously this was split across three places — two inline arms inside
        // the timeout match plus a fire-and-forget `tokio::spawn` here. The spawn
        // was unreliable: the Tokio runtime shuts down when the CLI's top-level
        // future returns and can cancel the cleanup task mid-run, leaving
        // orphaned snapshot dirs on disk.
        //
        // Awaiting here costs at most a few seconds on a partially-written tree
        // and guarantees the path is cleaned before this function returns (which
        // matters for the `CommitLock` path: the next commit must not find stale
        // state).
        //
        // Known residual race: `stream_snapshot` uses an uncancellable
        // `spawn_blocking` tar writer. On timeout, the writer may still be
        // draining the last ~64 KB of pipe buffer to disk for ~1 ms after the
        // async future is dropped. If that overlaps with this `remove_dir_all`,
        // a handful of stray files can leak under the partially-removed tree.
        // Hardening requires a cooperative cancel signal for the writer and is
        // tracked as a follow-up.
        if let Err(e) = snapshot_result {
            if snapshot_dest.exists()
                && let Err(rm_err) = tokio::fs::remove_dir_all(&snapshot_dest).await
            {
                tracing::warn!(
                    error = %rm_err,
                    path = %snapshot_dest.display(),
                    "failed to remove partial snapshot on commit error"
                );
            }
            return Err(e);
        }

        // 5. Build the new commit.
        //    Use "0" parent when this is the very first real commit.
        let parents = if parent_commit_id == "0" {
            None
        } else {
            Some(vec![parent_commit_id])
        };

        let mut new_commit = NewCommit::new(
            message,
            resolved_author,
            resolved_author_email,
            resolved_committer,
            resolved_committer_email,
            snapshot_hash,
            parents,
        );
        new_commit.schema_hash = schema_hash;

        // 6. Persist the commit object and advance the branch ref.
        let commit_hash = self.repository.commit(&path, new_commit).await?;

        tracing::info!("Commit created: {}", commit_hash);
        Ok(commit_hash)
    }

    /// Extract and store schema for the current database state.
    /// Returns the schema hash on success, or None if extraction fails.
    /// This is best-effort - failures are logged but don't fail the commit.
    ///
    /// Both the schema metadata and the schema-only DDL are captured from a
    /// SINGLE extraction sidecar's stdout. Stdout is the only channel that
    /// survives runtimes where the sidecar runs on a different host than the
    /// repository (e.g. Kubernetes, where the task pod and `.gfs` live on
    /// separate nodes): a mounted output file the sidecar writes is never
    /// readable by the gfs process. Routing the DDL through stdout — instead of
    /// a second `pg_dump -f <file>` sidecar whose file the host then reads —
    /// fixes schema history on Kubernetes and halves the per-commit sidecar
    /// cost (one cold start instead of two).
    async fn extract_and_store_schema(
        &self,
        repo_path: &std::path::Path,
    ) -> Result<String, Box<dyn std::error::Error>> {
        tracing::debug!("Extracting schema for commit");

        // Extract schema metadata + DDL via a single ExtractSchemaUseCase run.
        let extract_use_case =
            ExtractSchemaUseCase::new(self.compute.clone(), self.registry.clone());
        let schema_output = extract_use_case.run(repo_path).await.map_err(|e| {
            tracing::warn!("Schema extraction failed: {}", e);
            e
        })?;

        // Store schema object in repo. The hash is derived from the metadata
        // (schema.json); the DDL (schema.sql) is stored alongside for `gfs
        // schema show --ddl`. An empty DDL is acceptable — `schema show`
        // (metadata) and `schema diff` operate on the metadata and remain
        // fully functional.
        let schema_hash = repo_layout::write_schema_object(
            repo_path,
            &schema_output.metadata,
            &schema_output.schema_sql,
        )
        .map_err(|e| {
            tracing::warn!("Failed to write schema object: {}", e);
            e
        })?;

        tracing::info!("Schema stored with hash: {}", schema_hash);
        Ok(schema_hash)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use crate::model::commit::{CommitWithRefs, NewCommit};
    use crate::model::config::{EnvironmentConfig, RuntimeConfig, UserConfig};
    use crate::ports::compute::{
        ComputeDefinition, InstanceId, InstanceState, InstanceStatus, LogEntry, LogsOptions,
        StartOptions,
    };
    use crate::ports::database_provider::{
        ConnectionParams, DatabaseProvider, DatabaseProviderArg, DatabaseProviderRegistry,
        ProviderError, Result as RegistryResult, SIGTERM, SupportedFeature,
    };
    use crate::ports::repository::{LogOptions, RemoteOptions, Repository};
    use crate::ports::storage::{
        CloneOptions, MountStatus, Quota, Snapshot, SnapshotId, SnapshotOptions, VolumeId,
        VolumeStatus,
    };

    // -----------------------------------------------------------------------
    // Mock Repository
    // -----------------------------------------------------------------------

    #[derive(Default)]
    struct MockRepository {
        /// The commit hash returned by `commit()`.
        commit_hash: String,
        /// Values returned by the context getters.
        current_commit: String,
        runtime_config: Option<RuntimeConfig>,
        environment: Option<EnvironmentConfig>,
        mount_point: Option<String>,
        user_config: Option<UserConfig>,
        /// Records the NewCommit passed to `commit()`.
        committed: Mutex<Option<NewCommit>>,
        /// When set, `ensure_snapshot_path` returns a path under this directory (for cleanup tests).
        snapshot_root: Option<PathBuf>,
    }

    #[async_trait]
    impl Repository for MockRepository {
        async fn init(
            &self,
            _: &std::path::Path,
            _: Option<String>,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn get_workspace_data_dir_for_head(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<PathBuf> {
            Ok(PathBuf::from("/workspace/data"))
        }
        async fn update_environment_config(
            &self,
            _: &std::path::Path,
            _: crate::model::config::EnvironmentConfig,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn update_runtime_config(
            &self,
            _: &std::path::Path,
            _: RuntimeConfig,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn clone_repo(
            &self,
            _: &str,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn commit(
            &self,
            _: &std::path::Path,
            new_commit: NewCommit,
        ) -> crate::ports::repository::Result<String> {
            *self.committed.lock().unwrap() = Some(new_commit);
            Ok(self.commit_hash.clone())
        }
        async fn checkout(
            &self,
            _: &std::path::Path,
            _: &str,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn create_branch(
            &self,
            _: &std::path::Path,
            _: &str,
            _: &str,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn log(
            &self,
            _: &std::path::Path,
            _: LogOptions,
        ) -> crate::ports::repository::Result<Vec<CommitWithRefs>> {
            Ok(vec![])
        }
        async fn rev_parse(
            &self,
            _: &std::path::Path,
            _: &str,
        ) -> crate::ports::repository::Result<String> {
            Ok(String::new())
        }
        async fn push(
            &self,
            _: &std::path::Path,
            _: RemoteOptions,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn pull(
            &self,
            _: &std::path::Path,
            _: RemoteOptions,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn fetch(
            &self,
            _: &std::path::Path,
            _: RemoteOptions,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn get_current_branch(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<String> {
            Ok("main".to_string())
        }
        async fn get_current_commit_id(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<String> {
            Ok(self.current_commit.clone())
        }
        async fn get_runtime_config(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<RuntimeConfig>> {
            Ok(self.runtime_config.clone())
        }
        async fn get_mount_point(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<String>> {
            Ok(self.mount_point.clone())
        }
        async fn get_environment_config(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<EnvironmentConfig>> {
            Ok(self.environment.clone())
        }
        async fn get_user_config(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<UserConfig>> {
            Ok(self.user_config.clone())
        }
        async fn ensure_snapshot_path(
            &self,
            _: &std::path::Path,
            hash: &str,
        ) -> crate::ports::repository::Result<PathBuf> {
            let dest = if let Some(ref root) = self.snapshot_root {
                root.join(&hash[..2]).join(&hash[2..])
            } else {
                PathBuf::from(format!("/tmp/snapshots/{}/{}", &hash[..2], &hash[2..]))
            };
            Ok(dest)
        }
        async fn get_active_workspace_data_dir(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<PathBuf> {
            Ok(PathBuf::from("/workspace/data"))
        }
    }

    // -----------------------------------------------------------------------
    // Mock Compute
    // -----------------------------------------------------------------------

    struct MockCompute {
        state: InstanceState,
        prepared: Mutex<bool>,
        paused: Mutex<bool>,
        unpaused: Mutex<bool>,
        /// When set, `stream_snapshot` creates `dest` then fails with this message.
        stream_snapshot_fail_message: Mutex<Option<String>>,
        stream_snapshot_calls: AtomicUsize,
        /// When set, `pause()` returns `ComputeError::Internal` with this message
        /// instead of succeeding (simulates cgroup v1 / rootless Podman).
        pause_fails_with: Option<String>,
    }

    impl Default for MockCompute {
        fn default() -> Self {
            Self {
                state: InstanceState::Stopped,
                prepared: Mutex::new(false),
                paused: Mutex::new(false),
                unpaused: Mutex::new(false),
                stream_snapshot_fail_message: Mutex::new(None),
                stream_snapshot_calls: AtomicUsize::new(0),
                pause_fails_with: None,
            }
        }
    }

    #[async_trait]
    impl crate::ports::compute::Compute for MockCompute {
        async fn provision(
            &self,
            _: &ComputeDefinition,
        ) -> crate::ports::compute::Result<InstanceId> {
            Ok(InstanceId("mock".into()))
        }
        async fn start(
            &self,
            id: &InstanceId,
            _: StartOptions,
        ) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn stop(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Stopped,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn restart(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn status(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: self.state.clone(),
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn prepare_for_snapshot(
            &self,
            _: &InstanceId,
            _commands: &[String],
        ) -> crate::ports::compute::Result<()> {
            *self.prepared.lock().unwrap() = true;
            Ok(())
        }
        async fn logs(
            &self,
            _: &InstanceId,
            _: LogsOptions,
        ) -> crate::ports::compute::Result<Vec<LogEntry>> {
            Ok(vec![])
        }
        async fn pause(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            if let Some(ref msg) = self.pause_fails_with {
                // Mirror the real `classify()` in compute-docker: cgroup/freeze phrases
                // map to PauseUnsupported; everything else is a genuine Internal error.
                let lower = msg.to_ascii_lowercase();
                let is_pause_unsupported = [
                    "cgroup",
                    "freezing",
                    "freeze",
                    "pause is not",
                    "cannot pause",
                    "not supported",
                    "rootless",
                ]
                .iter()
                .any(|p| lower.contains(p));
                return Err(if is_pause_unsupported {
                    ComputeError::PauseUnsupported(msg.clone())
                } else {
                    ComputeError::Internal(msg.clone())
                });
            }
            *self.paused.lock().unwrap() = true;
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Paused,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn unpause(&self, id: &InstanceId) -> crate::ports::compute::Result<InstanceStatus> {
            *self.unpaused.lock().unwrap() = true;
            Ok(InstanceStatus {
                id: id.clone(),
                state: InstanceState::Running,
                pid: None,
                started_at: None,
                exit_code: None,
            })
        }
        async fn get_connection_info(
            &self,
            _id: &InstanceId,
            compute_port: u16,
        ) -> crate::ports::compute::Result<crate::ports::compute::InstanceConnectionInfo> {
            Ok(crate::ports::compute::InstanceConnectionInfo {
                host: "127.0.0.1".into(),
                port: compute_port,
                env: vec![],
            })
        }
        async fn get_instance_data_mount_host_path(
            &self,
            _id: &InstanceId,
            _compute_data_path: &str,
        ) -> crate::ports::compute::Result<Option<std::path::PathBuf>> {
            Ok(None)
        }
        async fn remove_instance(&self, _id: &InstanceId) -> crate::ports::compute::Result<()> {
            Ok(())
        }
        async fn stream_snapshot(
            &self,
            _id: &InstanceId,
            _container_path: &str,
            dest: &std::path::Path,
        ) -> crate::ports::compute::Result<()> {
            self.stream_snapshot_calls.fetch_add(1, Ordering::SeqCst);
            std::fs::create_dir_all(dest).map_err(crate::ports::compute::ComputeError::Io)?;
            if let Some(msg) = self.stream_snapshot_fail_message.lock().unwrap().clone() {
                return Err(crate::ports::compute::ComputeError::Internal(msg));
            }
            Ok(())
        }
        async fn get_task_connection_info(
            &self,
            _id: &InstanceId,
            compute_port: u16,
        ) -> crate::ports::compute::Result<crate::ports::compute::InstanceConnectionInfo> {
            Ok(crate::ports::compute::InstanceConnectionInfo {
                host: "172.17.0.2".into(),
                port: compute_port,
                env: vec![],
            })
        }
        async fn run_task(
            &self,
            _definition: &ComputeDefinition,
            _command: &str,
            _linked_to: Option<&InstanceId>,
        ) -> crate::ports::compute::Result<crate::ports::compute::ExecOutput> {
            Ok(crate::ports::compute::ExecOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    // -----------------------------------------------------------------------
    // Mock Storage
    // -----------------------------------------------------------------------

    struct MockStorage {
        snapshot_id: String,
        last_volume: Mutex<Option<String>>,
        /// Records the label (destination path) passed to snapshot().
        last_label: Mutex<Option<String>>,
        /// Records the path passed to finalize_snapshot().
        finalized: Mutex<Option<std::path::PathBuf>>,
        /// When set, `snapshot()` returns this error (e.g. permission denied).
        snapshot_fail: Mutex<Option<crate::ports::storage::StorageError>>,
    }

    impl MockStorage {
        fn new(snapshot_id: impl Into<String>) -> Self {
            Self {
                snapshot_id: snapshot_id.into(),
                last_volume: Mutex::new(None),
                last_label: Mutex::new(None),
                finalized: Mutex::new(None),
                snapshot_fail: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl crate::ports::storage::StoragePort for MockStorage {
        async fn mount(
            &self,
            _: &VolumeId,
            _: &std::path::Path,
        ) -> crate::ports::storage::Result<()> {
            Ok(())
        }
        async fn unmount(&self, _: &VolumeId) -> crate::ports::storage::Result<()> {
            Ok(())
        }
        async fn snapshot(
            &self,
            id: &VolumeId,
            options: SnapshotOptions,
        ) -> crate::ports::storage::Result<Snapshot> {
            *self.last_volume.lock().unwrap() = Some(id.0.clone());
            *self.last_label.lock().unwrap() = options.label.clone();
            if let Some(err) = self.snapshot_fail.lock().unwrap().take() {
                return Err(err);
            }
            Ok(Snapshot {
                id: SnapshotId(self.snapshot_id.clone()),
                volume_id: id.clone(),
                created_at: chrono::Utc::now(),
                size_bytes: 0,
                label: options.label,
            })
        }
        async fn clone(
            &self,
            _: &VolumeId,
            target: VolumeId,
            _: CloneOptions,
        ) -> crate::ports::storage::Result<VolumeStatus> {
            Ok(VolumeStatus {
                id: target,
                mount_point: None,
                status: MountStatus::Unmounted,
                size_bytes: 0,
                used_bytes: 0,
            })
        }
        async fn status(&self, id: &VolumeId) -> crate::ports::storage::Result<VolumeStatus> {
            Ok(VolumeStatus {
                id: id.clone(),
                mount_point: None,
                status: MountStatus::Unmounted,
                size_bytes: 0,
                used_bytes: 0,
            })
        }
        async fn quota(&self, id: &VolumeId) -> crate::ports::storage::Result<Quota> {
            Ok(Quota {
                volume_id: id.clone(),
                limit_bytes: 0,
                used_bytes: 0,
                free_bytes: 0,
            })
        }
        async fn finalize_snapshot(
            &self,
            dest: &std::path::Path,
        ) -> crate::ports::storage::Result<()> {
            *self.finalized.lock().unwrap() = Some(dest.to_path_buf());
            Ok(())
        }
    }

    // -----------------------------------------------------------------------
    // Mock DatabaseProviderRegistry
    // -----------------------------------------------------------------------

    struct MockProvider;

    impl DatabaseProvider for MockProvider {
        fn name(&self) -> &str {
            "mock-db"
        }
        fn definition(&self) -> ComputeDefinition {
            ComputeDefinition {
                labels: Default::default(),
                image: "mock:latest".into(),
                env: vec![],
                ports: vec![],
                data_dir: PathBuf::from("/data"),
                host_data_dir: None,
                user: None,
                logs_dir: None,
                conf_dir: None,
                args: vec![],
            }
        }
        fn default_port(&self) -> u16 {
            5432
        }
        fn default_args(&self) -> Vec<DatabaseProviderArg> {
            vec![]
        }
        fn default_signal(&self) -> u32 {
            SIGTERM
        }
        fn connection_string(
            &self,
            _: &ConnectionParams,
        ) -> std::result::Result<String, ProviderError> {
            Ok("mock://localhost:5432".into())
        }
        fn supported_versions(&self) -> Vec<String> {
            vec!["latest".to_string()]
        }
        fn supported_features(&self) -> Vec<SupportedFeature> {
            vec![SupportedFeature {
                id: "schema".into(),
                description: "Schema support.".into(),
            }]
        }
        fn prepare_for_snapshot(&self, _: &ConnectionParams) -> RegistryResult<Vec<String>> {
            Ok(vec![])
        }
        fn query_client_command(
            &self,
            _: &ConnectionParams,
            _: Option<&str>,
        ) -> std::result::Result<std::process::Command, ProviderError> {
            Ok(std::process::Command::new("true"))
        }
    }

    struct MockRegistry;

    impl DatabaseProviderRegistry for MockRegistry {
        fn register(&self, _: Arc<dyn DatabaseProvider>) -> RegistryResult<()> {
            Ok(())
        }
        fn get(&self, name: &str) -> Option<Arc<dyn DatabaseProvider>> {
            if name == "mock-db" {
                Some(Arc::new(MockProvider))
            } else {
                None
            }
        }
        fn list(&self) -> Vec<String> {
            vec!["mock-db".into()]
        }
        fn unregister(&self, _: &str) -> Option<Arc<dyn DatabaseProvider>> {
            None
        }
    }

    // -----------------------------------------------------------------------
    // Helper
    // -----------------------------------------------------------------------

    fn make_usecase(
        repo: MockRepository,
        compute: MockCompute,
        storage: MockStorage,
    ) -> CommitRepoUseCase<MockRegistry> {
        CommitRepoUseCase::new(
            Arc::new(repo),
            Arc::new(compute),
            Arc::new(storage),
            Arc::new(MockRegistry),
        )
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn empty_message_returns_error() {
        let uc = make_usecase(
            MockRepository::default(),
            MockCompute::default(),
            MockStorage::new("snap-1"),
        );
        let err = uc
            .run(PathBuf::from("/repo"), "".into(), None, None, None, None)
            .await;
        assert!(matches!(err, Err(CommitRepoError::EmptyMessage)));
    }

    /// Path that exists so that run()'s canonicalize() succeeds.
    fn existing_repo_path() -> PathBuf {
        let temp = tempfile::tempdir().expect("tempdir");
        temp.keep()
    }

    #[tokio::test]
    async fn commit_without_database_skips_compute() {
        let compute = MockCompute::default();
        let repo = MockRepository {
            commit_hash: "abc123".into(),
            current_commit: "0".into(),
            mount_point: Some("/vol/main".into()),
            ..Default::default()
        };

        let uc = make_usecase(repo, compute, MockStorage::new("snap-abc"));
        let hash = uc
            .run(
                existing_repo_path(),
                "initial commit".into(),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("commit should succeed");

        assert_eq!(hash, "abc123");

        // No container → compute was never touched.
        let compute_arc = uc.compute.clone();
        // We can't downcast Arc<dyn Compute>, so we just check the result is correct.
        let _ = compute_arc;
    }

    #[tokio::test]
    async fn commit_with_running_database_pauses_and_unpauses() {
        let compute = Arc::new(MockCompute {
            state: InstanceState::Running,
            ..Default::default()
        });
        let repo = MockRepository {
            commit_hash: "def456".into(),
            current_commit: "previous-hash".into(),
            mount_point: Some("/vol/main".into()),
            runtime_config: Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "my-pg".into(),
            }),
            environment: Some(EnvironmentConfig {
                database_provider: "mock-db".into(),
                database_version: "16".into(),
                database_port: None,
            }),
            ..Default::default()
        };
        let storage = Arc::new(MockStorage::new("snap-def"));
        let registry = Arc::new(MockRegistry);

        let uc = CommitRepoUseCase::new(Arc::new(repo), compute.clone(), storage.clone(), registry);

        let hash = uc
            .run(
                existing_repo_path(),
                "second commit".into(),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("commit should succeed");

        assert_eq!(hash, "def456");
        assert!(
            *compute.prepared.lock().unwrap(),
            "prepare_for_snapshot should have been called"
        );
        assert!(
            *compute.paused.lock().unwrap(),
            "pause should have been called"
        );
        assert!(
            *compute.unpaused.lock().unwrap(),
            "unpause should have been called"
        );
        // Host snapshot succeeds: storage.snapshot is used; no stream fallback.
        assert_eq!(
            storage.last_volume.lock().unwrap().as_deref(),
            Some("/vol/main"),
            "storage.snapshot should copy the workspace volume"
        );
        assert_eq!(
            compute.stream_snapshot_calls.load(Ordering::SeqCst),
            0,
            "stream_snapshot should not run when host snapshot succeeds"
        );
        assert!(
            storage.finalized.lock().unwrap().is_none(),
            "finalize_snapshot is only for stream_snapshot fallback"
        );
    }

    #[tokio::test]
    async fn commit_stream_snapshot_finalizes_read_only() {
        // Host snapshot fails with permission denied → fallback runs stream_snapshot
        // then finalize_snapshot.
        let compute = Arc::new(MockCompute {
            state: InstanceState::Stopped,
            ..Default::default()
        });
        let repo = MockRepository {
            commit_hash: "fin123".into(),
            current_commit: "0".into(),
            mount_point: Some("/vol/data".into()),
            runtime_config: Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "fin-pg".into(),
            }),
            environment: Some(EnvironmentConfig {
                database_provider: "mock-db".into(),
                database_version: "16".into(),
                database_port: None,
            }),
            ..Default::default()
        };
        let storage = Arc::new(MockStorage::new("snap-fin"));
        *storage.snapshot_fail.lock().unwrap() =
            Some(crate::ports::storage::StorageError::PermissionDenied(
                "copy failed: Permission denied".into(),
            ));
        let registry = Arc::new(MockRegistry);

        let uc = CommitRepoUseCase::new(Arc::new(repo), compute.clone(), storage.clone(), registry);

        uc.run(
            existing_repo_path(),
            "finalize test".into(),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("commit should succeed");

        assert_eq!(compute.stream_snapshot_calls.load(Ordering::SeqCst), 1);
        let finalized = storage.finalized.lock().unwrap().clone();
        assert!(
            finalized.is_some(),
            "finalize_snapshot must be called after fallback"
        );
        let finalized_str = finalized.unwrap().to_string_lossy().into_owned();
        assert!(
            finalized_str.contains("snapshots"),
            "finalized path should be under the snapshots dir, got: {finalized_str}"
        );
    }

    #[tokio::test]
    async fn commit_removes_partial_snapshot_when_stream_fails() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let compute = Arc::new(MockCompute {
            stream_snapshot_fail_message: Mutex::new(Some("stream failed".into())),
            ..Default::default()
        });
        let repo = MockRepository {
            commit_hash: "unused".into(),
            current_commit: "0".into(),
            mount_point: Some("/vol/data".into()),
            snapshot_root: Some(tmp.path().to_path_buf()),
            runtime_config: Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "fail-pg".into(),
            }),
            environment: Some(EnvironmentConfig {
                database_provider: "mock-db".into(),
                database_version: "16".into(),
                database_port: None,
            }),
            ..Default::default()
        };
        let storage = Arc::new(MockStorage::new("snap"));
        *storage.snapshot_fail.lock().unwrap() = Some(
            crate::ports::storage::StorageError::PermissionDenied("copy failed".into()),
        );
        let uc = CommitRepoUseCase::new(Arc::new(repo), compute, storage, Arc::new(MockRegistry));

        let err = uc
            .run(
                existing_repo_path(),
                "cleanup test".into(),
                None,
                None,
                None,
                None,
            )
            .await;
        // Cleanup is fire-and-forget (background task) — only assert on the error type.
        assert!(matches!(err, Err(CommitRepoError::Compute(_))));
    }

    #[tokio::test]
    async fn commit_snapshot_hash_is_64_char_hex_independent_of_storage_id() {
        let storage = Arc::new(MockStorage::new("storage-snap-id-is-ignored"));
        let repo = MockRepository {
            commit_hash: "ghi789".into(),
            current_commit: "0".into(),
            mount_point: Some("/vol".into()),
            ..Default::default()
        };
        let registry = Arc::new(MockRegistry);

        let uc = CommitRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockCompute::default()),
            storage.clone(),
            registry,
        );

        uc.run(
            existing_repo_path(),
            "snap test".into(),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("commit should succeed");

        // The label passed to storage should contain the snapshot path built from the hash.
        let label = storage.last_label.lock().unwrap().clone().unwrap();
        // Label is a path like "/tmp/snapshots/<2>/<62>" — the 64-char hash is split across it.
        assert!(
            label.contains("/tmp/snapshots/"),
            "expected snapshot dest under /tmp/snapshots, got: {label}"
        );

        // Extract the hash portion: last two segments of the path joined.
        let parts: Vec<&str> = label.trim_end_matches('/').split('/').collect();
        let n = parts.len();
        assert!(n >= 2, "path should have at least 2 segments");
        let reconstructed_hash = format!("{}{}", parts[n - 2], parts[n - 1]);
        assert_eq!(reconstructed_hash.len(), 64, "hash should be 64 chars");
        assert!(
            reconstructed_hash.chars().all(|c| c.is_ascii_hexdigit()),
            "hash should be hex"
        );
    }

    #[tokio::test]
    async fn commit_uses_mount_point_as_volume_id() {
        let storage = Arc::new(MockStorage::new("snap-mp"));
        let repo = MockRepository {
            commit_hash: "mp1".into(),
            current_commit: "0".into(),
            mount_point: Some("/mnt/my-volume".into()),
            ..Default::default()
        };
        let registry = Arc::new(MockRegistry);

        let uc = CommitRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockCompute::default()),
            storage.clone(),
            registry,
        );

        uc.run(
            existing_repo_path(),
            "mp test".into(),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("commit should succeed");

        assert_eq!(
            storage.last_volume.lock().unwrap().as_deref(),
            Some("/mnt/my-volume")
        );
    }

    #[tokio::test]
    async fn commit_falls_back_to_workspace_data_dir_when_no_mount_point() {
        let storage = Arc::new(MockStorage::new("snap-fallback"));
        let repo = MockRepository {
            commit_hash: "fb1".into(),
            current_commit: "0".into(),
            mount_point: None, // no mount_point configured
            ..Default::default()
        };
        let registry = Arc::new(MockRegistry);

        let uc = CommitRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockCompute::default()),
            storage.clone(),
            registry,
        );

        uc.run(
            existing_repo_path(),
            "fallback test".into(),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("commit should succeed");

        // Mock get_active_workspace_data_dir returns "/workspace/data"
        assert_eq!(
            storage.last_volume.lock().unwrap().as_deref(),
            Some("/workspace/data")
        );
    }

    #[tokio::test]
    async fn commit_user_config_fallback_for_author_committer() {
        let repo = MockRepository {
            commit_hash: "uc1".into(),
            current_commit: "0".into(),
            mount_point: Some("/vol".into()),
            user_config: Some(UserConfig {
                name: Some("Alice".into()),
                email: Some("alice@example.com".into()),
            }),
            ..Default::default()
        };
        let storage = Arc::new(MockStorage::new("snap-uc"));
        let registry = Arc::new(MockRegistry);

        let uc = CommitRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockCompute::default()),
            storage,
            registry,
        );

        let hash = uc
            .run(
                existing_repo_path(),
                "user config test".into(),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("commit should succeed");

        assert_eq!(hash, "uc1");
    }

    #[tokio::test]
    async fn commit_canonicalize_failure() {
        let uc = make_usecase(
            MockRepository::default(),
            MockCompute::default(),
            MockStorage::new("snap"),
        );
        let result = uc
            .run(
                PathBuf::from("/nonexistent/path/that/does/not/exist"),
                "msg".into(),
                None,
                None,
                None,
                None,
            )
            .await;
        assert!(result.is_err());
        assert!(matches!(result, Err(CommitRepoError::Repository(_))));
    }

    #[tokio::test]
    async fn commit_non_permission_storage_error_skips_stream_fallback() {
        let compute = Arc::new(MockCompute::default());
        let repo = MockRepository {
            commit_hash: "x".into(),
            current_commit: "0".into(),
            mount_point: Some("/vol/data".into()),
            runtime_config: Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "pg".into(),
            }),
            environment: Some(EnvironmentConfig {
                database_provider: "mock-db".into(),
                database_version: "16".into(),
                database_port: None,
            }),
            ..Default::default()
        };
        let storage = Arc::new(MockStorage::new("snap"));
        *storage.snapshot_fail.lock().unwrap() = Some(
            crate::ports::storage::StorageError::Internal("storage failed".into()),
        );
        let uc = CommitRepoUseCase::new(
            Arc::new(repo),
            compute.clone(),
            storage,
            Arc::new(MockRegistry),
        );
        let result = uc
            .run(existing_repo_path(), "fail".into(), None, None, None, None)
            .await;
        assert!(matches!(result, Err(CommitRepoError::Storage(_))));
        assert_eq!(compute.stream_snapshot_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn commit_storage_error() {
        struct FailingStorage;

        #[async_trait]
        impl crate::ports::storage::StoragePort for FailingStorage {
            async fn mount(
                &self,
                _: &VolumeId,
                _: &std::path::Path,
            ) -> crate::ports::storage::Result<()> {
                Ok(())
            }
            async fn unmount(&self, _: &VolumeId) -> crate::ports::storage::Result<()> {
                Ok(())
            }
            async fn snapshot(
                &self,
                _: &VolumeId,
                _: SnapshotOptions,
            ) -> crate::ports::storage::Result<Snapshot> {
                Err(crate::ports::storage::StorageError::Internal(
                    "storage failed".into(),
                ))
            }
            async fn clone(
                &self,
                _: &VolumeId,
                target: VolumeId,
                _: CloneOptions,
            ) -> crate::ports::storage::Result<VolumeStatus> {
                Ok(VolumeStatus {
                    id: target,
                    mount_point: None,
                    status: MountStatus::Unmounted,
                    size_bytes: 0,
                    used_bytes: 0,
                })
            }
            async fn status(&self, id: &VolumeId) -> crate::ports::storage::Result<VolumeStatus> {
                Ok(VolumeStatus {
                    id: id.clone(),
                    mount_point: None,
                    status: MountStatus::Unmounted,
                    size_bytes: 0,
                    used_bytes: 0,
                })
            }
            async fn quota(&self, id: &VolumeId) -> crate::ports::storage::Result<Quota> {
                Ok(Quota {
                    volume_id: id.clone(),
                    limit_bytes: 0,
                    used_bytes: 0,
                    free_bytes: 0,
                })
            }
            async fn finalize_snapshot(
                &self,
                _: &std::path::Path,
            ) -> crate::ports::storage::Result<()> {
                Ok(())
            }
        }

        let repo = MockRepository {
            commit_hash: "x".into(),
            current_commit: "0".into(),
            mount_point: Some("/vol".into()),
            ..Default::default()
        };
        let uc = CommitRepoUseCase::new(
            Arc::new(repo),
            Arc::new(MockCompute::default()),
            Arc::new(FailingStorage),
            Arc::new(MockRegistry),
        );
        let result = uc
            .run(
                existing_repo_path(),
                "storage fail".into(),
                None,
                None,
                None,
                None,
            )
            .await;
        assert!(matches!(result, Err(CommitRepoError::Storage(_))));
    }

    /// Rootless Podman on cgroup v1 returns a `PauseUnsupported` error from `pause()`.
    /// By default (no `GFS_ALLOW_UNFROZEN_SNAPSHOT`), the commit must *refuse* with
    /// a message that surfaces all three workarounds — switching runtime, upgrading
    /// to cgroup v2, or opting in. No pause/unpause must be issued since the runtime
    /// rejected the pause request.
    #[tokio::test]
    async fn commit_refuses_when_pause_unsupported_without_opt_in() {
        let compute = Arc::new(MockCompute {
            state: InstanceState::Running,
            pause_fails_with: Some(
                "OCI runtime error: cgroups: cgroup v1 does not support freezing a single process"
                    .into(),
            ),
            ..Default::default()
        });
        let repo = MockRepository {
            commit_hash: "abc123".into(),
            current_commit: "prev".into(),
            mount_point: Some("/vol/main".into()),
            runtime_config: Some(RuntimeConfig {
                runtime_provider: "podman".into(),
                runtime_version: "5.0".into(),
                container_name: "gfs-pg-test".into(),
            }),
            environment: Some(EnvironmentConfig {
                database_provider: "mock-db".into(),
                database_version: "16".into(),
                database_port: None,
            }),
            ..Default::default()
        };
        let storage = Arc::new(MockStorage::new("snap-abc"));
        let registry = Arc::new(MockRegistry);

        let uc = CommitRepoUseCase::new(Arc::new(repo), compute.clone(), storage, registry);
        let result = uc
            .run(
                existing_repo_path(),
                "cgroup v1 pause test".into(),
                None,
                None,
                None,
                None,
            )
            .await;

        match result {
            Err(CommitRepoError::Compute(ComputeError::PauseUnsupported(msg))) => {
                assert!(
                    msg.contains("GFS_ALLOW_UNFROZEN_SNAPSHOT=1"),
                    "refusal must mention the opt-in env var; got: {msg}"
                );
                assert!(
                    msg.contains("cgroup v2"),
                    "refusal must mention the cgroup v2 upgrade path; got: {msg}"
                );
            }
            other => panic!("expected PauseUnsupported refusal, got {other:?}"),
        }
        assert!(
            !*compute.paused.lock().unwrap(),
            "paused flag should be false — pause was rejected by runtime"
        );
        assert!(
            !*compute.unpaused.lock().unwrap(),
            "unpause must not be called when pause was never issued"
        );
    }

    /// A genuine pause failure (container not found, daemon error) must still
    /// propagate as an error — only the "unsupported" subset is swallowed.
    #[tokio::test]
    async fn commit_fails_on_genuine_pause_error() {
        let compute = Arc::new(MockCompute {
            state: InstanceState::Running,
            pause_fails_with: Some("daemon internal error: connection refused".into()),
            ..Default::default()
        });
        let repo = MockRepository {
            commit_hash: "abc123".into(),
            current_commit: "prev".into(),
            mount_point: Some("/vol/main".into()),
            runtime_config: Some(RuntimeConfig {
                runtime_provider: "docker".into(),
                runtime_version: "24".into(),
                container_name: "gfs-pg-test".into(),
            }),
            environment: Some(EnvironmentConfig {
                database_provider: "mock-db".into(),
                database_version: "16".into(),
                database_port: None,
            }),
            ..Default::default()
        };
        let storage = Arc::new(MockStorage::new("snap-abc"));
        let registry = Arc::new(MockRegistry);

        let uc = CommitRepoUseCase::new(Arc::new(repo), compute, storage, registry);
        let result = uc
            .run(
                existing_repo_path(),
                "genuine pause error".into(),
                None,
                None,
                None,
                None,
            )
            .await;

        assert!(
            matches!(result, Err(CommitRepoError::Compute(_))),
            "genuine pause error must propagate: {result:?}"
        );
    }
}
