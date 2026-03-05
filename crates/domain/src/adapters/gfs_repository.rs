//! GFS implementation of the [`Repository`] port.
//!
//! Delegates all operations to the underlying `repo_layout` helpers and maps
//! `RepoError` to `RepositoryError`. This is the only place in the domain that
//! touches `repo_layout` for layout/config operations.

use std::fs;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::process::Command;

use async_trait::async_trait;

use crate::model::commit::{Commit, CommitWithRefs, FileEntry, NewCommit, file_entry_diff_stats};
use crate::model::config::{EnvironmentConfig, GfsConfig, RuntimeConfig, UserConfig};
use crate::model::errors::RepoError;
use crate::model::layout::{
    BRANCH_WORKSPACE_SEGMENT, GFS_DIR, HEADS_DIR, OBJECTS_DIR, REFS_DIR, SNAPSHOTS_DIR,
    WORKSPACE_DATA_DIR, WORKSPACES_DIR,
};
use crate::ports::repository::{LogOptions, RemoteOptions, Repository, RepositoryError, Result};
use crate::repo_utils::repo_layout;
use crate::utils::hash::hash_commit;

fn map_err(e: RepoError) -> RepositoryError {
    match e {
        RepoError::RevisionNotFound(rev) => RepositoryError::RevisionNotFound(rev),
        RepoError::NoRepoFound(path) => RepositoryError::NotFound(path.display().to_string()),
        _ => RepositoryError::Internal(e.to_string()),
    }
}

/// Recursively copy directory contents from `src` into `dst` (dst must exist).
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut file_count = 0;
    let mut dir_count = 0;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            fs::create_dir_all(&dst_path)?;
            copy_dir_all(&entry.path(), &dst_path)?;
            dir_count += 1;
        } else {
            fs::copy(entry.path(), dst_path)?;
            file_count += 1;
        }
    }
    tracing::debug!(
        "copy_dir_all: copied {} files and {} dirs from {:?} to {:?}",
        file_count,
        dir_count,
        src,
        dst
    );
    Ok(())
}

/// Best-effort permission normalization for workspace directories.
///
/// On some runtimes (notably rootless Podman with user namespace remapping),
/// repository files can be owned by subordinate UIDs that are not chmod-able
/// from the host user. In that case, we continue checkout and let the runtime
/// handle access through its own namespace mapping.
fn set_workspace_dir_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let output = Command::new("chmod")
            .arg("-R")
            .arg("0700")
            .arg(path)
            .output()
            .map_err(std::io::Error::other)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let first_line = stderr
                .lines()
                .next()
                .unwrap_or("chmod -R 0700 failed")
                .to_string();

            let is_permission_error = {
                let lower = stderr.to_ascii_lowercase();
                lower.contains("operation not permitted")
                    || lower.contains("permission denied")
                    || lower.contains("not permitted")
            };

            if is_permission_error {
                tracing::warn!(
                    workspace_path = %path.display(),
                    error = %first_line,
                    "Checkout: failed to normalize workspace permissions; continuing"
                );
            } else {
                return Err(std::io::Error::other(format!(
                    "chmod -R 0700 failed for '{}': {first_line}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

/// Concrete implementation of [`Repository`] backed by the GFS on-disk layout.
#[derive(Debug, Default)]
pub struct GfsRepository;

impl GfsRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Repository for GfsRepository {
    async fn init(&self, path: &Path, mount_point: Option<String>) -> Result<()> {
        repo_layout::init_repo_layout(path, mount_point).map_err(map_err)
    }

    async fn get_workspace_data_dir_for_head(&self, repo: &Path) -> Result<PathBuf> {
        repo_layout::get_workspace_data_dir_for_head(repo).map_err(map_err)
    }

    async fn update_environment_config(
        &self,
        repo: &Path,
        config: EnvironmentConfig,
    ) -> Result<()> {
        repo_layout::update_environment_config(repo, config).map_err(map_err)
    }

    async fn update_runtime_config(&self, repo: &Path, config: RuntimeConfig) -> Result<()> {
        repo_layout::update_runtime_config(repo, config).map_err(map_err)
    }

    async fn clone_repo(&self, _url: &str, _target: &Path) -> Result<()> {
        Err(RepositoryError::Internal("not implemented".into()))
    }

    async fn commit(&self, repo: &Path, new_commit: NewCommit) -> Result<String> {
        // Resolve repo to an absolute path so snapshot_path matches where storage wrote the snapshot.
        let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());

        // 1. Hash the commit content.
        let commit_hash =
            hash_commit(&new_commit).map_err(|e| RepositoryError::Internal(e.to_string()))?;

        // 2. Build the full Commit struct to persist.
        let mut commit = Commit::from_new_commit(&new_commit, commit_hash.clone());

        // 3. Enrich with database_provider and database_version from repo config.
        if let Ok(config) = GfsConfig::load(&repo)
            && let Some(env) = config.environment
        {
            commit.database_provider = Some(env.database_provider);
            commit.database_version = Some(env.database_version);
        }

        // 4. Enrich with files list, file stats, and snapshot physical size.
        let current_snapshot_path = repo_layout::snapshot_path(&repo, &new_commit.snapshot_hash);
        if current_snapshot_path.exists() {
            if let Ok(size) = repo_layout::directory_physical_size_bytes(&current_snapshot_path) {
                commit.snapshot_size_bytes = Some(size);
            }
            if let Ok(entries) = repo_layout::collect_file_entries(&current_snapshot_path, "") {
                let parent_files: Option<Vec<FileEntry>> = new_commit
                    .parents
                    .as_ref()
                    .and_then(|p| p.first())
                    .and_then(|parent_id| repo_layout::get_commit_from_hash(&repo, parent_id).ok())
                    .and_then(|parent_commit| {
                        repo_layout::get_file_entries_for_commit(&repo, &parent_commit)
                            .ok()
                            .flatten()
                    });
                let (added, deleted, modified) =
                    file_entry_diff_stats(&entries, parent_files.as_deref());
                commit.files_added = Some(added);
                commit.files_deleted = Some(deleted);
                commit.files_modified = Some(modified);
                commit.files_count = Some(entries.len());
                let files_hash =
                    repo_layout::write_files_object(&repo, &entries).map_err(map_err)?;
                commit.files_ref = Some(files_hash);
            }
        }

        // 5. Split hash: first 2 chars → subdirectory, remainder → filename.
        let (dir_part, file_part) = commit_hash.split_at(2);
        let object_dir = repo.join(GFS_DIR).join(OBJECTS_DIR).join(dir_part);
        let object_path = object_dir.join(file_part);

        // 6. Create object directory and write the JSON-serialised commit.
        fs::create_dir_all(&object_dir).map_err(RepositoryError::Io)?;
        let json = serde_json::to_string_pretty(&commit)
            .map_err(|e| RepositoryError::Internal(e.to_string()))?;
        fs::write(&object_path, json).map_err(RepositoryError::Io)?;

        // 7. Advance the current branch ref to the new commit.
        let branch = repo_layout::get_current_branch(&repo).map_err(map_err)?;
        // Only update a named branch ref; skip when HEAD is detached (64-char hex).
        if !(branch.len() == 64 && branch.chars().all(|c| c.is_ascii_hexdigit())) {
            repo_layout::update_branch_ref(&repo, &branch, &commit_hash).map_err(map_err)?;
        }

        tracing::info!(
            "Committed '{}' on branch '{}' → {}",
            new_commit.message,
            branch,
            commit_hash
        );
        Ok(commit_hash)
    }

    async fn checkout(&self, repo: &Path, revision: &str) -> Result<()> {
        // Use canonical repo path so snapshot_dir matches where commit wrote (avoids /var vs /private/var etc.).
        let repo = repo.canonicalize().map_err(RepositoryError::Io)?;

        let revision = revision.trim();
        if revision.is_empty() {
            return Err(RepositoryError::RevisionNotFound("(empty)".to_string()));
        }

        // Resolve revision to full commit hash.
        let commit_hash = repo_layout::rev_parse(&repo, revision).map_err(map_err)?;

        // Reject initial state: cannot checkout "0".
        if commit_hash == "0" {
            let msg = if repo_layout::is_branch(&repo, revision) {
                format!("cannot checkout: branch '{revision}' has no commits")
            } else {
                "cannot checkout: branch has no commits".to_string()
            };
            return Err(RepositoryError::Internal(msg));
        }

        // Determine HEAD update: branch name iff refs/heads/<revision> exists and tip matches.
        let branch_segment = if repo_layout::is_branch(&repo, revision) {
            let ref_path = repo
                .join(GFS_DIR)
                .join(REFS_DIR)
                .join(HEADS_DIR)
                .join(revision);
            let tip = fs::read_to_string(&ref_path).map_err(RepositoryError::Io)?;
            if tip.trim() == commit_hash {
                revision.to_string()
            } else {
                "detached".to_string()
            }
        } else {
            "detached".to_string()
        };

        // Workspace path: one persistent dir per branch (workspaces/<branch>/0/data), or per-commit when detached.
        let workspace_segment = if branch_segment == "detached" {
            repo_layout::short_commit_id_for_workspace(&commit_hash)
        } else {
            BRANCH_WORKSPACE_SEGMENT.to_string()
        };
        let workspace_path = repo
            .join(GFS_DIR)
            .join(WORKSPACES_DIR)
            .join(&branch_segment)
            .join(&workspace_segment)
            .join(WORKSPACE_DATA_DIR);

        // Only populate from snapshot when the workspace does not exist (preserve live DB state in branch workspace).
        let workspace_exists = workspace_path.exists();
        tracing::info!(
            "Checkout: workspace_path={:?}, exists={}",
            workspace_path,
            workspace_exists
        );
        if !workspace_exists {
            let commit = repo_layout::get_commit_from_hash(&repo, &commit_hash).map_err(map_err)?;
            let snapshot_hash = commit.snapshot_hash;
            let snapshot_dir = repo
                .join(GFS_DIR)
                .join(SNAPSHOTS_DIR)
                .join(&snapshot_hash[..2])
                .join(&snapshot_hash[2..]);

            tracing::info!(
                "Checkout: snapshot_dir={:?}, exists={}",
                snapshot_dir,
                snapshot_dir.exists()
            );
            if snapshot_dir.exists() && snapshot_dir.is_dir() {
                fs::create_dir_all(&workspace_path).map_err(RepositoryError::Io)?;
                tracing::info!("Checkout: created workspace_path, copying from snapshot...");
                copy_dir_all(&snapshot_dir, &workspace_path).map_err(RepositoryError::Io)?;
                tracing::info!("Checkout: copy completed");
                // Remove stale Postgres lock files so a new instance can start on this copy.
                let _ = fs::remove_file(workspace_path.join("postmaster.pid"));
                let _ = fs::remove_file(workspace_path.join("postmaster.opts"));
            } else {
                tracing::warn!(
                    "Checkout: snapshot_dir does not exist or is not a directory, creating empty workspace"
                );
                fs::create_dir_all(&workspace_path).map_err(RepositoryError::Io)?;
            }
        }
        set_workspace_dir_permissions(&workspace_path).map_err(RepositoryError::Io)?;

        // Update HEAD.
        if branch_segment == "detached" {
            repo_layout::update_head_with_commit(&repo, &commit_hash).map_err(map_err)?;
        } else {
            repo_layout::update_head_with_branch(&repo, &branch_segment).map_err(map_err)?;
        }

        // Point active workspace to the new directory.
        repo_layout::set_active_workspace_data_dir(&repo, &workspace_path).map_err(map_err)?;

        tracing::info!(
            "Checkout {} -> {} ({})",
            revision,
            branch_segment,
            &commit_hash[..7.min(commit_hash.len())]
        );
        Ok(())
    }

    async fn create_branch(&self, repo: &Path, name: &str, commit_hash: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            return Err(RepositoryError::RevisionNotFound(
                "(empty branch name)".to_string(),
            ));
        }
        if repo_layout::is_branch(repo, name) {
            return Err(RepositoryError::BranchAlreadyExists(name.to_string()));
        }
        let ref_path = repo.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR).join(name);
        if let Some(parent) = ref_path.parent() {
            fs::create_dir_all(parent).map_err(RepositoryError::Io)?;
        }
        fs::write(&ref_path, commit_hash).map_err(RepositoryError::Io)?;
        Ok(())
    }

    async fn log(&self, repo: &Path, options: LogOptions) -> Result<Vec<CommitWithRefs>> {
        let start = if let Some(ref from) = options.from {
            repo_layout::rev_parse(repo, from).map_err(map_err)?
        } else {
            repo_layout::get_current_commit_id(repo).map_err(map_err)?
        };

        if start == "0" {
            return Ok(vec![]);
        }

        let until_hash = match &options.until {
            Some(u) => Some(repo_layout::rev_parse(repo, u).map_err(map_err)?),
            None => None,
        };

        let mut commits = Vec::new();
        let mut current = start;

        loop {
            if let Some(ref until) = until_hash
                && current == *until
            {
                break;
            }
            if let Some(limit) = options.limit
                && commits.len() >= limit
            {
                break;
            }

            let mut commit = repo_layout::get_commit_from_hash(repo, &current).map_err(map_err)?;
            let parent = commit
                .parents
                .as_ref()
                .and_then(|p| p.first())
                .filter(|p| *p != "0")
                .cloned();
            commit.hash = Some(current.clone());
            let refs = repo_layout::get_refs_pointing_to(repo, &current).map_err(map_err)?;
            commits.push(CommitWithRefs { commit, refs });
            match parent {
                Some(p) => current = p,
                None => break,
            }
        }

        Ok(commits)
    }

    async fn rev_parse(&self, repo: &Path, revision: &str) -> Result<String> {
        repo_layout::rev_parse(repo, revision).map_err(map_err)
    }

    async fn push(&self, _repo: &Path, _options: RemoteOptions) -> Result<()> {
        Err(RepositoryError::Internal("not implemented".into()))
    }

    async fn pull(&self, _repo: &Path, _options: RemoteOptions) -> Result<()> {
        Err(RepositoryError::Internal("not implemented".into()))
    }

    async fn fetch(&self, _repo: &Path, _options: RemoteOptions) -> Result<()> {
        Err(RepositoryError::Internal("not implemented".into()))
    }

    async fn get_current_branch(&self, repo: &Path) -> Result<String> {
        repo_layout::get_current_branch(repo).map_err(map_err)
    }

    async fn get_current_commit_id(&self, repo: &Path) -> Result<String> {
        repo_layout::get_current_commit_id(repo).map_err(map_err)
    }

    async fn get_runtime_config(&self, repo: &Path) -> Result<Option<RuntimeConfig>> {
        repo_layout::get_runtime_config(repo).map_err(map_err)
    }

    async fn get_mount_point(&self, repo: &Path) -> Result<Option<String>> {
        repo_layout::get_mount_point(repo).map_err(map_err)
    }

    async fn get_environment_config(&self, repo: &Path) -> Result<Option<EnvironmentConfig>> {
        repo_layout::get_environment_config(repo).map_err(map_err)
    }

    async fn get_user_config(&self, repo: &Path) -> Result<Option<UserConfig>> {
        repo_layout::get_user_config(repo).map_err(map_err)
    }

    async fn ensure_snapshot_path(&self, repo: &Path, hash: &str) -> Result<PathBuf> {
        repo_layout::ensure_snapshot_path(repo, hash).map_err(map_err)
    }

    async fn get_active_workspace_data_dir(&self, repo: &Path) -> Result<PathBuf> {
        repo_layout::get_active_workspace_data_dir(repo).map_err(map_err)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use tempfile::TempDir;

    use crate::model::layout::{CONFIG_FILE, HEAD_FILE, MAIN_BRANCH, WORKSPACE_FILE};
    use crate::ports::repository::{LogOptions, Repository};
    use crate::utils::hash::hash_commit;

    /// Initialise a minimal .gfs repo layout (equivalent to `init_repo_layout`).
    fn setup_repo() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let gfs = dir.join(GFS_DIR);
        fs::create_dir_all(&gfs).unwrap();
        fs::write(
            gfs.join(HEAD_FILE),
            format!("ref: {}/{}/{}", REFS_DIR, HEADS_DIR, MAIN_BRANCH),
        )
        .unwrap();

        let config = r#"version = "0.1.0"
description = "test"
"#;
        fs::write(gfs.join(CONFIG_FILE), config).unwrap();

        let commit_id = "0";
        fs::create_dir_all(gfs.join(REFS_DIR).join(HEADS_DIR)).unwrap();
        fs::write(
            gfs.join(REFS_DIR).join(HEADS_DIR).join(MAIN_BRANCH),
            commit_id,
        )
        .unwrap();

        fs::create_dir_all(gfs.join(OBJECTS_DIR)).unwrap();

        let workspace_data_dir = gfs
            .join(WORKSPACES_DIR)
            .join(MAIN_BRANCH)
            .join(commit_id)
            .join(WORKSPACE_DATA_DIR);
        fs::create_dir_all(&workspace_data_dir).unwrap();

        // Write the WORKSPACE file pointing at the initial data dir.
        fs::write(
            gfs.join(WORKSPACE_FILE),
            workspace_data_dir.to_string_lossy().as_ref(),
        )
        .unwrap();

        tmp
    }

    fn make_new_commit(message: &str, snapshot_hash: &str) -> NewCommit {
        NewCommit::new(
            message.to_string(),
            "alice".to_string(),
            Some("alice@example.com".to_string()),
            "alice".to_string(),
            Some("alice@example.com".to_string()),
            snapshot_hash.to_string(),
            None,
        )
    }

    fn make_new_commit_with_parent(
        message: &str,
        snapshot_hash: &str,
        parent: String,
    ) -> NewCommit {
        NewCommit::new(
            message.to_string(),
            "alice".to_string(),
            Some("alice@example.com".to_string()),
            "alice".to_string(),
            Some("alice@example.com".to_string()),
            snapshot_hash.to_string(),
            Some(vec![parent]),
        )
    }

    #[tokio::test]
    async fn commit_writes_object_file() {
        let tmp = setup_repo();
        let repo_path = tmp.path();

        let gfs = GfsRepository::new();
        let new_commit = make_new_commit("first commit", "snap-aabbcc");

        let expected_hash = hash_commit(&new_commit).unwrap();
        let returned_hash = gfs.commit(repo_path, new_commit).await.unwrap();

        assert_eq!(returned_hash, expected_hash);

        // Verify object file exists at objects/<2>/<rest>
        let (dir_part, file_part) = expected_hash.split_at(2);
        let object_path = repo_path
            .join(GFS_DIR)
            .join(OBJECTS_DIR)
            .join(dir_part)
            .join(file_part);
        assert!(
            object_path.exists(),
            "object file should exist at {:?}",
            object_path
        );
    }

    #[tokio::test]
    async fn commit_object_is_valid_json_with_correct_fields() {
        let tmp = setup_repo();
        let repo_path = tmp.path();

        let gfs = GfsRepository::new();
        let new_commit = make_new_commit("json check", "snap-json");

        let hash = gfs.commit(repo_path, new_commit).await.unwrap();

        let (dir_part, file_part) = hash.split_at(2);
        let json_bytes = fs::read(
            repo_path
                .join(GFS_DIR)
                .join(OBJECTS_DIR)
                .join(dir_part)
                .join(file_part),
        )
        .unwrap();

        let commit: crate::model::commit::Commit = serde_json::from_slice(&json_bytes).unwrap();
        assert_eq!(commit.message, "json check");
        assert_eq!(commit.snapshot_hash, "snap-json");
        assert_eq!(commit.author, "alice");
        assert_eq!(commit.hash, Some(hash));
    }

    #[tokio::test]
    async fn commit_advances_branch_ref() {
        let tmp = setup_repo();
        let repo_path = tmp.path();

        let gfs = GfsRepository::new();
        let new_commit = make_new_commit("advance ref", "snap-ref");

        let hash = gfs.commit(repo_path, new_commit).await.unwrap();

        let ref_content = fs::read_to_string(
            repo_path
                .join(GFS_DIR)
                .join(REFS_DIR)
                .join(HEADS_DIR)
                .join(MAIN_BRANCH),
        )
        .unwrap();
        assert_eq!(ref_content.trim(), hash);
    }

    #[tokio::test]
    async fn commit_hash_is_deterministic() {
        let tmp1 = setup_repo();
        let tmp2 = setup_repo();

        let new_commit1 = make_new_commit("deterministic", "snap-det");
        // Build an identical commit for hashing
        let new_commit2 = make_new_commit("deterministic", "snap-det");
        // Force same timestamp by using the same NewCommit data
        // (NewCommit::new sets now(), so we derive the hash from the same struct)
        let hash_direct = hash_commit(&new_commit1).unwrap();

        let gfs = GfsRepository::new();
        let hash_via_commit = gfs.commit(tmp1.path(), new_commit1).await.unwrap();

        // A separately constructed but identical commit won't have the exact same timestamp,
        // so we verify the returned hash matches what hash_commit produces for that commit.
        let hash_via_commit2 = gfs.commit(tmp2.path(), new_commit2).await.unwrap();

        assert_eq!(hash_via_commit, hash_direct);
        // Two commits with different timestamps will have different hashes – that's correct.
        // Just assert both are valid hex strings of length 64.
        assert_eq!(hash_via_commit.len(), 64);
        assert_eq!(hash_via_commit2.len(), 64);
        assert!(hash_via_commit.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn get_current_branch_returns_main_after_init() {
        let tmp = setup_repo();
        let gfs = GfsRepository::new();
        let branch = gfs.get_current_branch(tmp.path()).await.unwrap();
        assert_eq!(branch, "main");
    }

    #[tokio::test]
    async fn get_current_commit_id_returns_zero_on_fresh_repo() {
        let tmp = setup_repo();
        let gfs = GfsRepository::new();
        let id = gfs.get_current_commit_id(tmp.path()).await.unwrap();
        assert_eq!(id, "0");
    }

    // -----------------------------------------------------------------------
    // ensure_snapshot_path tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn ensure_snapshot_path_creates_prefix_dir_and_returns_dest() {
        let tmp = setup_repo();
        let repo_path = tmp.path();
        let gfs = GfsRepository::new();

        let hash = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890ab";
        let dest = gfs.ensure_snapshot_path(repo_path, hash).await.unwrap();

        // Prefix dir (.gfs/snapshots/ab/) must exist.
        let prefix_dir = repo_path.join(GFS_DIR).join(SNAPSHOTS_DIR).join("ab");
        assert!(prefix_dir.exists(), "prefix dir should have been created");

        // The returned dest path = .gfs/snapshots/ab/<62 chars>.
        let expected = prefix_dir.join(&hash[2..]);
        assert_eq!(dest, expected);
        // The dest directory itself is NOT created yet (cp will create it).
        assert!(
            !dest.exists(),
            "dest dir should not exist yet — cp creates it"
        );
    }

    #[tokio::test]
    async fn ensure_snapshot_path_is_idempotent_for_prefix_dir() {
        let tmp = setup_repo();
        let repo_path = tmp.path();
        let gfs = GfsRepository::new();

        let hash = "aabbcc1234567890aabbcc1234567890aabbcc1234567890aabbcc1234567890aa";
        // Call twice — the prefix dir creation should succeed both times.
        let dest1 = gfs.ensure_snapshot_path(repo_path, hash).await.unwrap();
        let dest2 = gfs.ensure_snapshot_path(repo_path, hash).await.unwrap();
        assert_eq!(dest1, dest2);
    }

    #[tokio::test]
    async fn commit_and_ensure_snapshot_path_together_produce_correct_layout() {
        let tmp = setup_repo();
        let repo_path = tmp.path();
        let gfs = GfsRepository::new();

        // Simulate the use case: compute hash, ensure path, then commit.
        let snap_ts = chrono::Utc::now();
        let snapshot_hash = crate::utils::hash::hash_snapshot("/vol/main", &snap_ts);

        let snapshot_dest = gfs
            .ensure_snapshot_path(repo_path, &snapshot_hash)
            .await
            .unwrap();

        // Verify the prefix directory was created.
        let prefix_dir = repo_path
            .join(GFS_DIR)
            .join(SNAPSHOTS_DIR)
            .join(&snapshot_hash[..2]);
        assert!(prefix_dir.exists());
        assert_eq!(snapshot_dest, prefix_dir.join(&snapshot_hash[2..]));
        assert_eq!(snapshot_hash.len(), 64);
        assert!(snapshot_hash.chars().all(|c| c.is_ascii_hexdigit()));

        // Now commit with this snapshot hash.
        let new_commit = make_new_commit("layout test", &snapshot_hash);
        let commit_hash = gfs.commit(repo_path, new_commit).await.unwrap();

        // Verify the commit object contains the correct snapshot_hash.
        let (dir, file) = commit_hash.split_at(2);
        let obj_path = repo_path
            .join(GFS_DIR)
            .join(OBJECTS_DIR)
            .join(dir)
            .join(file);
        let json_bytes = fs::read(obj_path).unwrap();
        let commit: crate::model::commit::Commit = serde_json::from_slice(&json_bytes).unwrap();
        assert_eq!(commit.snapshot_hash, snapshot_hash);
    }

    /// When the snapshot directory exists and contains files, commit populates `files` and
    /// files_added/files_deleted/files_modified from the files array.
    #[tokio::test]
    async fn commit_populates_files_and_stats_when_snapshot_dir_exists() {
        let tmp = setup_repo();
        let repo_path = tmp.path();
        let gfs = GfsRepository::new();

        let snapshot_hash = crate::utils::hash::hash_snapshot("/vol/main", &chrono::Utc::now());
        let snapshot_dest = gfs
            .ensure_snapshot_path(repo_path, &snapshot_hash)
            .await
            .unwrap();
        fs::create_dir_all(&snapshot_dest).unwrap();
        fs::write(snapshot_dest.join("file1"), "a").unwrap();
        fs::write(snapshot_dest.join("file2"), "bb").unwrap();

        let new_commit = make_new_commit("first", &snapshot_hash);
        let commit_hash = gfs.commit(repo_path, new_commit).await.unwrap();

        let (dir, file) = commit_hash.split_at(2);
        let obj_path = repo_path
            .join(GFS_DIR)
            .join(OBJECTS_DIR)
            .join(dir)
            .join(file);
        let commit: crate::model::commit::Commit =
            serde_json::from_slice(&fs::read(obj_path).unwrap()).unwrap();

        assert!(
            commit.files_ref.is_some(),
            "files_ref should be populated when snapshot exists"
        );
        assert_eq!(
            commit.files_count,
            Some(2),
            "files_count should be total file count"
        );
        let files = repo_layout::get_file_entries_for_commit(repo_path, &commit)
            .unwrap()
            .unwrap();
        assert_eq!(files.len(), 2, "should list both files");
        let paths: Vec<&str> = files.iter().map(|e| e.relative_path.as_str()).collect();
        assert!(paths.contains(&"file1"));
        assert!(paths.contains(&"file2"));
        assert_eq!(commit.files_added, Some(2));
        assert_eq!(commit.files_deleted, Some(0));
        assert_eq!(commit.files_modified, Some(0));
    }

    /// Second commit: diff stats are computed from current vs parent commit's files array.
    #[tokio::test]
    async fn commit_computes_diff_stats_from_files_array() {
        let tmp = setup_repo();
        let repo_path = tmp.path();
        let gfs = GfsRepository::new();

        // First commit: snapshot with file1 and file2.
        let hash1_ts = chrono::Utc::now();
        let snapshot_hash1 = crate::utils::hash::hash_snapshot("/vol/main", &hash1_ts);
        let dest1 = gfs
            .ensure_snapshot_path(repo_path, &snapshot_hash1)
            .await
            .unwrap();
        fs::create_dir_all(&dest1).unwrap();
        fs::write(dest1.join("file1"), "a").unwrap();
        fs::write(dest1.join("file2"), "bb").unwrap();
        let commit_hash1 = gfs
            .commit(repo_path, make_new_commit("first", &snapshot_hash1))
            .await
            .unwrap();

        // Update branch ref so next commit has a parent (setup_repo leaves ref at "0").
        repo_layout::update_branch_ref(repo_path, MAIN_BRANCH, &commit_hash1).unwrap();

        // Second commit: snapshot with file1 (same), file3 (new), file2 removed.
        let snapshot_hash2 = crate::utils::hash::hash_snapshot("/vol/main", &chrono::Utc::now());
        let dest2 = gfs
            .ensure_snapshot_path(repo_path, &snapshot_hash2)
            .await
            .unwrap();
        fs::create_dir_all(&dest2).unwrap();
        fs::write(dest2.join("file1"), "a").unwrap();
        fs::write(dest2.join("file3"), "ccc").unwrap();

        let new_commit = make_new_commit_with_parent("second", &snapshot_hash2, commit_hash1);
        let commit_hash2 = gfs.commit(repo_path, new_commit).await.unwrap();

        let (dir, file) = commit_hash2.split_at(2);
        let obj_path = repo_path
            .join(GFS_DIR)
            .join(OBJECTS_DIR)
            .join(dir)
            .join(file);
        let commit: crate::model::commit::Commit =
            serde_json::from_slice(&fs::read(obj_path).unwrap()).unwrap();

        assert_eq!(commit.files_added, Some(1), "file3 added");
        assert_eq!(commit.files_deleted, Some(1), "file2 deleted");
        assert_eq!(
            commit.files_modified,
            Some(0),
            "file1 unchanged (same size)"
        );
        let files = repo_layout::get_file_entries_for_commit(repo_path, &commit)
            .unwrap()
            .unwrap();
        assert_eq!(files.len(), 2);
        let paths: Vec<&str> = files.iter().map(|e| e.relative_path.as_str()).collect();
        assert!(paths.contains(&"file1"));
        assert!(paths.contains(&"file3"));
    }

    // -----------------------------------------------------------------------
    // get_active_workspace_data_dir tests
    // -----------------------------------------------------------------------

    /// After `setup_repo` (which writes the WORKSPACE file), the returned path
    /// should match the initial workspace data dir.
    #[tokio::test]
    async fn get_active_workspace_data_dir_returns_workspace_file_path() {
        let tmp = setup_repo();
        let repo_path = tmp.path();
        let gfs = GfsRepository::new();

        let expected = repo_path
            .join(GFS_DIR)
            .join(WORKSPACES_DIR)
            .join(MAIN_BRANCH)
            .join("0")
            .join(WORKSPACE_DATA_DIR);

        let active = gfs.get_active_workspace_data_dir(repo_path).await.unwrap();
        assert_eq!(active, expected, "should return path from WORKSPACE file");
    }

    /// When the WORKSPACE file is absent the method falls back to the workspace
    /// for the current HEAD commit (legacy repos created before this feature).
    #[tokio::test]
    async fn get_active_workspace_data_dir_falls_back_when_workspace_file_missing() {
        let tmp = setup_repo();
        let repo_path = tmp.path();

        // Remove the WORKSPACE file to simulate a legacy repo.
        fs::remove_file(repo_path.join(GFS_DIR).join(WORKSPACE_FILE)).unwrap();

        let gfs = GfsRepository::new();
        let active = gfs.get_active_workspace_data_dir(repo_path).await.unwrap();

        // Fallback: HEAD is "0", so workspace is workspaces/main/0/data.
        let expected = repo_path
            .join(GFS_DIR)
            .join(WORKSPACES_DIR)
            .join(MAIN_BRANCH)
            .join("0")
            .join(WORKSPACE_DATA_DIR);
        assert_eq!(active, expected, "should fall back to HEAD workspace");
    }

    #[tokio::test]
    async fn log_returns_empty_when_at_initial_commit() {
        let tmp = setup_repo();
        let repo_path = tmp.path();
        let gfs = GfsRepository::new();

        let options = LogOptions::default();
        let commits = gfs.log(repo_path, options).await.unwrap();

        assert!(commits.is_empty());
    }

    #[tokio::test]
    async fn log_returns_single_commit() {
        let tmp = setup_repo();
        let repo_path = tmp.path();
        let gfs = GfsRepository::new();

        let new_commit = make_new_commit("first commit", "snap-aa");
        let hash = gfs.commit(repo_path, new_commit).await.unwrap();

        let options = LogOptions::default();
        let commits = gfs.log(repo_path, options).await.unwrap();

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].commit.hash.as_deref(), Some(hash.as_str()));
        assert_eq!(commits[0].commit.message, "first commit");
    }

    #[tokio::test]
    async fn log_returns_multiple_commits_newest_first() {
        let tmp = setup_repo();
        let repo_path = tmp.path();
        let gfs = GfsRepository::new();

        let hash1 = gfs
            .commit(repo_path, make_new_commit("first", "snap-1"))
            .await
            .unwrap();
        let hash2 = gfs
            .commit(
                repo_path,
                make_new_commit_with_parent("second", "snap-2", hash1.clone()),
            )
            .await
            .unwrap();
        let hash3 = gfs
            .commit(
                repo_path,
                make_new_commit_with_parent("third", "snap-3", hash2.clone()),
            )
            .await
            .unwrap();

        let options = LogOptions::default();
        let commits = gfs.log(repo_path, options).await.unwrap();

        assert_eq!(commits.len(), 3);
        assert_eq!(commits[0].commit.message, "third");
        assert_eq!(commits[0].commit.hash.as_deref(), Some(hash3.as_str()));
        assert_eq!(commits[1].commit.message, "second");
        assert_eq!(commits[1].commit.hash.as_deref(), Some(hash2.as_str()));
        assert_eq!(commits[2].commit.message, "first");
        assert_eq!(commits[2].commit.hash.as_deref(), Some(hash1.as_str()));
    }

    #[tokio::test]
    async fn log_respects_limit() {
        let tmp = setup_repo();
        let repo_path = tmp.path();
        let gfs = GfsRepository::new();

        let hash1 = gfs
            .commit(repo_path, make_new_commit("first", "snap-1"))
            .await
            .unwrap();
        let hash2 = gfs
            .commit(
                repo_path,
                make_new_commit_with_parent("second", "snap-2", hash1.clone()),
            )
            .await
            .unwrap();
        gfs.commit(
            repo_path,
            make_new_commit_with_parent("third", "snap-3", hash2),
        )
        .await
        .unwrap();

        let options = LogOptions {
            limit: Some(2),
            ..Default::default()
        };
        let commits = gfs.log(repo_path, options).await.unwrap();

        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].commit.message, "third");
        assert_eq!(commits[1].commit.message, "second");
    }

    #[tokio::test]
    async fn log_respects_from_branch() {
        let tmp = setup_repo();
        let repo_path = tmp.path();
        let gfs = GfsRepository::new();

        let hash1 = gfs
            .commit(repo_path, make_new_commit("first", "snap-1"))
            .await
            .unwrap();
        let hash2 = gfs
            .commit(
                repo_path,
                make_new_commit_with_parent("second", "snap-2", hash1.clone()),
            )
            .await
            .unwrap();
        gfs.commit(
            repo_path,
            make_new_commit_with_parent("third", "snap-3", hash2),
        )
        .await
        .unwrap();

        let options = LogOptions {
            from: Some("main".to_string()),
            ..Default::default()
        };
        let commits = gfs.log(repo_path, options).await.unwrap();

        assert_eq!(commits.len(), 3);
        assert_eq!(commits[2].commit.hash.as_deref(), Some(hash1.as_str()));
    }

    /// After multiple commits the WORKSPACE file still points to the initial
    /// workspace (not the new HEAD), so snapshots are taken from the correct dir.
    #[tokio::test]
    async fn active_workspace_unchanged_after_commit() {
        let tmp = setup_repo();
        let repo_path = tmp.path();
        let gfs = GfsRepository::new();

        let initial_active = gfs.get_active_workspace_data_dir(repo_path).await.unwrap();

        // Perform two commits — HEAD advances each time.
        for msg in ["first", "second"] {
            let nc = make_new_commit(msg, "snap-xx");
            gfs.commit(repo_path, nc).await.unwrap();
        }

        // HEAD is now at a full 64-char hash, but WORKSPACE is still the same.
        let still_active = gfs.get_active_workspace_data_dir(repo_path).await.unwrap();
        assert_eq!(
            still_active, initial_active,
            "active workspace must not change after commits"
        );
    }
}
