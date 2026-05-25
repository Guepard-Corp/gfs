use crate::model::commit::{Commit, FileEntry};
use crate::model::config::{EnvironmentConfig, GfsConfig, RuntimeConfig, UserConfig};
use crate::model::errors::RepoError;
use crate::model::layout::{
    BRANCH_WORKSPACE_SEGMENT, CONFIG_FILE, DEFAULT_SHORT_HASH_LEN, GFS_DIR, HEAD_FILE, HEADS_DIR,
    MAIN_BRANCH, MIN_SHORT_HASH_LEN, OBJECTS_DIR, REFS_DIR, SHORT_COMMIT_ID_LEN, SNAPSHOTS_DIR,
    WORKSPACE_DATA_DIR, WORKSPACE_FILE, WORKSPACES_DIR,
};
use anyhow::Result;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub fn validate_repo_layout(gfs_dir: &Path) -> Result<(), RepoError> {
    // Check HEAD file
    tracing::trace!("Checking HEAD file in {}", gfs_dir.display());
    let head_path = gfs_dir.join(HEAD_FILE);
    if !head_path.exists() {
        return Err(RepoError::missing_file(head_path));
    }

    // Check config.toml
    tracing::trace!("Checking config.toml in {}", gfs_dir.display());
    let config_path = gfs_dir.join(CONFIG_FILE);
    if !config_path.exists() {
        return Err(RepoError::missing_file(config_path));
    }
    // Validate config.toml is valid TOML
    tracing::trace!("Validating config.toml in {}", gfs_dir.display());
    let config_content = fs::read_to_string(&config_path).map_err(RepoError::from)?;
    toml::from_str::<toml::Value>(&config_content)
        .map_err(|e| RepoError::InvalidConfig(e.to_string()))?;

    // Check refs/heads/main
    tracing::trace!("Checking refs/heads/main in {}", gfs_dir.display());
    let main_ref = gfs_dir.join(REFS_DIR).join(HEADS_DIR).join(MAIN_BRANCH);
    if !main_ref.exists() {
        return Err(RepoError::missing_file(main_ref));
    }

    // Check objects directory
    tracing::trace!("Checking objects directory in {}", gfs_dir.display());
    let objects_dir = gfs_dir.join(OBJECTS_DIR);
    if !objects_dir.exists() || !objects_dir.is_dir() {
        return Err(RepoError::invalid_layout(
            "objects directory is missing or not a directory".to_string(),
        ));
    }

    // Check workspace data dir for current branch/commit (required after init)
    tracing::trace!(
        "Checking workspace data dir for HEAD in {}",
        gfs_dir.display()
    );
    let repo_path = gfs_dir
        .parent()
        .ok_or_else(|| RepoError::invalid_layout("invalid .gfs path".to_string()))?;
    let workspace_data_dir = get_workspace_data_dir_path(repo_path)?;
    if !workspace_data_dir.exists() || !workspace_data_dir.is_dir() {
        return Err(RepoError::invalid_layout(format!(
            "workspace data directory is missing or not a directory: {}",
            workspace_data_dir.display()
        )));
    }

    Ok(())
}

pub fn init_repo_layout(working_dir: &Path, mount_point: Option<String>) -> Result<(), RepoError> {
    let gfs_dir = working_dir.join(GFS_DIR);

    if gfs_dir.exists() {
        validate_repo_layout(&gfs_dir)?;
        return Err(RepoError::already_initialized(gfs_dir));
    }

    tracing::info!(
        "Initializing new .gfs directory at {}",
        working_dir.display()
    );

    fs::create_dir_all(&gfs_dir).map_err(RepoError::from)?;

    // Create refs/heads directory
    let refs_dir = gfs_dir.join(REFS_DIR).join(HEADS_DIR);
    fs::create_dir_all(&refs_dir).map_err(RepoError::from)?;

    // Create objects directory
    let objects_dir = gfs_dir.join(OBJECTS_DIR);
    fs::create_dir_all(&objects_dir).map_err(RepoError::from)?;

    // Create default workspace for main branch (hosts the database; one persistent dir per branch)
    let workspace_data_dir = gfs_dir
        .join(WORKSPACES_DIR)
        .join(MAIN_BRANCH)
        .join(BRANCH_WORKSPACE_SEGMENT)
        .join(WORKSPACE_DATA_DIR);
    fs::create_dir_all(&workspace_data_dir).map_err(RepoError::from)?;

    // Create base snapshots directory (empty on init; COW folders added via storage impl)
    let snapshots_dir = gfs_dir.join(SNAPSHOTS_DIR);
    fs::create_dir_all(&snapshots_dir).map_err(RepoError::from)?;

    // Create HEAD file
    let head_content = format!("ref: {}/{}/{}", REFS_DIR, HEADS_DIR, MAIN_BRANCH);
    fs::write(gfs_dir.join(HEAD_FILE), head_content).map_err(RepoError::from)?;

    // Create initial main branch reference (0 = no commits yet)
    let main_ref_path = refs_dir.join(MAIN_BRANCH);
    fs::write(&main_ref_path, BRANCH_WORKSPACE_SEGMENT) // Initial commit hash "0"
        .map_err(RepoError::from)?;

    // Record the active workspace data directory.
    //
    // This file is read by the commit use case so it snapshots the directory
    // where the database is actually running, regardless of how many commits
    // have accumulated since init.  It is updated by checkout / branch
    // operations; commit intentionally leaves it unchanged.
    let workspace_file = gfs_dir.join(WORKSPACE_FILE);
    fs::write(
        &workspace_file,
        workspace_data_dir.to_string_lossy().as_ref(),
    )
    .map_err(RepoError::from)?;

    // Create config.toml
    let config = GfsConfig {
        mount_point,
        version: env!("CARGO_PKG_VERSION").to_string(),
        description: "Git For database Systems".to_string(),
        user: None,
        environment: None,
        runtime: None,
        storage: None,
        remote: None,
    };

    let config_path = gfs_dir.join(CONFIG_FILE);
    let content =
        toml::to_string_pretty(&config).map_err(|e| RepoError::InvalidConfig(e.to_string()))?;
    fs::write(&config_path, content).map_err(RepoError::from)?;

    tracing::info!("Successfully initialized .gfs directory");

    // Set new file marker
    let new_marker_path = gfs_dir.join("new");
    fs::write(&new_marker_path, "").map_err(RepoError::from)?;

    Ok(())
}

pub fn is_new_repo(working_dir: &Path) -> bool {
    let new_marker_path = working_dir.join(GFS_DIR).join("new");
    new_marker_path.exists()
}

pub fn remove_new_marker(working_dir: &Path) {
    let new_marker_path = working_dir.join(GFS_DIR).join("new");
    fs::remove_file(&new_marker_path).unwrap();
}

/// Return the path recorded in `.gfs/WORKSPACE` — the directory where the
/// database is currently running.
///
/// Falls back to the workspace for the current HEAD commit when the
/// `WORKSPACE` file does not exist (e.g. repos created before this feature).
pub fn get_active_workspace_data_dir(repo_path: &Path) -> Result<std::path::PathBuf, RepoError> {
    let workspace_file = repo_path.join(GFS_DIR).join(WORKSPACE_FILE);
    if workspace_file.exists() {
        let raw = fs::read_to_string(&workspace_file).map_err(RepoError::from)?;
        let path = std::path::PathBuf::from(raw.trim());
        return Ok(path);
    }
    // Legacy fallback: derive from current HEAD.
    get_workspace_data_dir_for_head(repo_path)
}

/// Overwrite `.gfs/WORKSPACE` with `path`.
///
/// Called by checkout / branch operations to point the active workspace at a
/// different data directory.
pub fn set_active_workspace_data_dir(
    repo_path: &Path,
    path: &std::path::Path,
) -> Result<(), RepoError> {
    let workspace_file = repo_path.join(GFS_DIR).join(WORKSPACE_FILE);
    fs::write(&workspace_file, path.to_string_lossy().as_ref()).map_err(RepoError::from)
}

pub fn get_current_branch(path: &Path) -> Result<String, RepoError> {
    tracing::trace!("Getting current branch from {}", path.display());
    let gfs_dir = path.join(GFS_DIR);
    let head_content = fs::read_to_string(gfs_dir.join(HEAD_FILE)).map_err(RepoError::from)?;

    let trimmed = head_content.trim();
    if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(trimmed.to_string())
    } else if let Some(branch) = trimmed.strip_prefix(&format!("ref: {}/{}/", REFS_DIR, HEADS_DIR))
    {
        Ok(branch.trim().to_string())
    } else {
        Err(RepoError::invalid_layout(format!(
            "Invalid HEAD file content: {}",
            head_content
        )))
    }
}

pub fn update_project_mount_point(repo_path: &Path, mount_point: String) -> Result<(), RepoError> {
    tracing::trace!("Updating project mount point to {}", mount_point);
    let mut config = GfsConfig::load(repo_path)?;
    config.mount_point = Some(mount_point);
    config.save(repo_path)?;
    Ok(())
}

pub fn update_runtime_config(
    repo_path: &Path,
    runtime_config: RuntimeConfig,
) -> Result<(), RepoError> {
    tracing::trace!("Updating runtime config to {}", runtime_config);
    let mut config = GfsConfig::load(repo_path)?;
    config.runtime = Some(runtime_config);
    config.save(repo_path)?;
    Ok(())
}

pub fn update_environment_config(
    repo_path: &Path,
    environment_config: EnvironmentConfig,
) -> Result<(), RepoError> {
    tracing::trace!(
        "Updating environment config: provider={}, version={}",
        environment_config.database_provider,
        environment_config.database_version
    );
    let mut config = GfsConfig::load(repo_path)?;
    config.environment = Some(environment_config);
    config.save(repo_path)?;
    Ok(())
}

pub fn get_runtime_config(repo_path: &Path) -> Result<Option<RuntimeConfig>, RepoError> {
    let config = GfsConfig::load(repo_path)?;
    Ok(config.runtime)
}

pub fn get_mount_point(repo_path: &Path) -> Result<Option<String>, RepoError> {
    let config = GfsConfig::load(repo_path)?;
    Ok(config.mount_point)
}

pub fn get_environment_config(repo_path: &Path) -> Result<Option<EnvironmentConfig>, RepoError> {
    let config = GfsConfig::load(repo_path)?;
    Ok(config.environment)
}

pub fn get_user_config(repo_path: &Path) -> Result<Option<UserConfig>, RepoError> {
    let config = GfsConfig::load(repo_path)?;
    Ok(config.user)
}

/// Read user identity from `git config` (local → global → system).
/// Returns a `UserConfig` whose fields are `None` when git is unavailable
/// or no value is configured.
pub fn get_git_user_config() -> UserConfig {
    UserConfig {
        name: run_git_config("user.name"),
        email: run_git_config("user.email"),
    }
}

fn run_git_config(key: &str) -> Option<String> {
    std::process::Command::new("git")
        .args(["config", key])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            } else {
                None
            }
        })
}

/// Create `.gfs/snapshots/<first 2 chars of hash>/<remaining 62 chars>/` and
/// return the full destination path.
///
/// The parent two-char prefix directory is created if it does not already exist.
/// The returned path is the directory where the COW copy of the workspace data
/// should be written (it must not exist yet when `cp -cRp` is invoked).
pub fn ensure_snapshot_path(repo_path: &Path, hash: &str) -> Result<std::path::PathBuf, RepoError> {
    let (prefix, rest) = hash.split_at(2);
    let prefix_dir = repo_path.join(GFS_DIR).join(SNAPSHOTS_DIR).join(prefix);
    fs::create_dir_all(&prefix_dir).map_err(RepoError::from)?;
    Ok(prefix_dir.join(rest))
}

/// Return the filesystem path of a snapshot directory for the given hash.
/// Does not create the directory; use [`ensure_snapshot_path`] for that.
pub fn snapshot_path(repo_path: &Path, hash: &str) -> PathBuf {
    let (prefix, rest) = hash.split_at(2);
    repo_path
        .join(GFS_DIR)
        .join(SNAPSHOTS_DIR)
        .join(prefix)
        .join(rest)
}

/// Returns the canonical path for the `.needs-repair` marker file associated
/// with the workspace whose data directory is at `workspace_data_dir`.
///
/// The marker is written after a `stream_snapshot` fallback commit (which does
/// not preserve original file ownership) and is consumed by checkout to trigger
/// a pre-start `chown` repair before the database container boots.
///
/// Returns `None` when `workspace_data_dir` has no parent (e.g. `/` or a bare
/// component), so callers skip the marker write rather than placing `.needs-repair`
/// inside the database data directory itself.
///
/// **Always use this function** — never compute `parent().join(".needs-repair")`
/// inline, as the three write/read sites must agree on the exact path.
pub fn repair_marker_path(workspace_data_dir: &Path) -> Option<PathBuf> {
    workspace_data_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join(".needs-repair"))
}

/// Logical size of a directory tree (sum of file lengths in bytes).
pub fn directory_logical_size_bytes(dir: &Path) -> Result<u64, RepoError> {
    let mut total = 0u64;
    for entry in fs::read_dir(dir).map_err(RepoError::from)? {
        let entry = entry.map_err(RepoError::from)?;
        let ty = entry.file_type().map_err(RepoError::from)?;
        if ty.is_dir() {
            total += directory_logical_size_bytes(&entry.path())?;
        } else {
            total += entry.metadata().map_err(RepoError::from)?.len();
        }
    }
    Ok(total)
}

/// Physical (on-disk) size of a directory tree in bytes.
///
/// On Unix this is the sum of allocated 512-byte blocks per file (equivalent to `du -s`).
/// On APFS, COW snapshots share blocks with the source, so volume usage does not increase
/// by this amount until the copy diverges; this value is still useful for reporting
/// "disk usage of this tree" as tools like `du` would show.
#[cfg(unix)]
pub fn directory_physical_size_bytes(dir: &Path) -> Result<u64, RepoError> {
    use std::os::unix::fs::MetadataExt;
    let mut total = 0u64;
    for entry in fs::read_dir(dir).map_err(RepoError::from)? {
        let entry = entry.map_err(RepoError::from)?;
        let ty = entry.file_type().map_err(RepoError::from)?;
        if ty.is_dir() {
            total += directory_physical_size_bytes(&entry.path())?;
        } else {
            let blocks = entry.metadata().map_err(RepoError::from)?.blocks();
            total += blocks * 512;
        }
    }
    Ok(total)
}

#[cfg(not(unix))]
/// Physical size: not available on this platform; returns logical size.
pub fn directory_physical_size_bytes(dir: &Path) -> Result<u64, RepoError> {
    directory_logical_size_bytes(dir)
}

/// Result of comparing two snapshot directories: (added, deleted, modified) relative paths.
pub struct SnapshotDiff {
    pub added: Vec<String>,
    pub deleted: Vec<String>,
    pub modified: Vec<String>,
}

fn collect_file_paths(dir: &Path, rel_prefix: &str) -> Result<Vec<String>, RepoError> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir).map_err(RepoError::from)? {
        let entry = entry.map_err(RepoError::from)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel_path = if rel_prefix.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", rel_prefix, name)
        };
        let ty = entry.file_type().map_err(RepoError::from)?;
        if ty.is_dir() {
            paths.extend(collect_file_paths(&entry.path(), &rel_path)?);
        } else {
            paths.push(rel_path);
        }
    }
    Ok(paths)
}

/// Collect a flattened list of all files under `dir` with metadata (relative path, size, owner, group, permissions).
/// Paths are relative to the given directory (e.g. workspace `data/`). Result is sorted by relative_path.
pub fn collect_file_entries(dir: &Path, rel_prefix: &str) -> Result<Vec<FileEntry>, RepoError> {
    let mut entries = Vec::new();
    collect_file_entries_into(dir, rel_prefix, &mut entries)?;
    entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(entries)
}

fn collect_file_entries_into(
    dir: &Path,
    rel_prefix: &str,
    out: &mut Vec<FileEntry>,
) -> Result<(), RepoError> {
    for entry in fs::read_dir(dir).map_err(RepoError::from)? {
        let entry = entry.map_err(RepoError::from)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel_path = if rel_prefix.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", rel_prefix, name)
        };
        let ty = entry.file_type().map_err(RepoError::from)?;
        if ty.is_dir() {
            collect_file_entries_into(&entry.path(), &rel_path, out)?;
        } else {
            let meta = entry.metadata().map_err(RepoError::from)?;
            let file_size = meta.len();
            let (owner, group, permissions) = file_metadata_owner_group_mode(&meta);
            out.push(FileEntry {
                relative_path: rel_path,
                file_size,
                owner,
                group,
                permissions,
                file_attributes: None,
            });
        }
    }
    Ok(())
}

#[cfg(unix)]
fn file_metadata_owner_group_mode(
    meta: &std::fs::Metadata,
) -> (Option<String>, Option<String>, Option<String>) {
    use std::os::unix::fs::MetadataExt;
    let owner = Some(meta.uid().to_string());
    let group = Some(meta.gid().to_string());
    let permissions = Some(format!("{:o}", meta.mode() & 0o7777));
    (owner, group, permissions)
}

#[cfg(not(unix))]
fn file_metadata_owner_group_mode(
    _meta: &std::fs::Metadata,
) -> (Option<String>, Option<String>, Option<String>) {
    (None, None, None)
}

fn files_content_eq(a: &Path, b: &Path) -> std::io::Result<bool> {
    let aa = fs::read(a)?;
    let bb = fs::read(b)?;
    Ok(aa == bb)
}

/// Compare parent and current snapshot directories. Returns lists of relative paths
/// for added, deleted, and modified files. If `parent_dir` is `None` or does not exist,
/// all files in `current_dir` are considered added.
pub fn snapshot_diff(
    parent_dir: Option<&Path>,
    current_dir: &Path,
) -> Result<SnapshotDiff, RepoError> {
    let current_paths = collect_file_paths(current_dir, "")?;
    let current_set: HashSet<String> = current_paths.iter().cloned().collect();

    let (parent_paths, parent_set) = match parent_dir {
        Some(p) if p.exists() => {
            let paths = collect_file_paths(p, "")?;
            let set: HashSet<String> = paths.iter().cloned().collect();
            (paths, set)
        }
        _ => {
            return Ok(SnapshotDiff {
                added: current_paths,
                deleted: Vec::new(),
                modified: Vec::new(),
            });
        }
    };

    let parent_path = parent_dir.unwrap();
    let added: Vec<String> = current_paths
        .iter()
        .filter(|p| !parent_set.contains(*p))
        .cloned()
        .collect();
    let deleted: Vec<String> = parent_paths
        .iter()
        .filter(|p| !current_set.contains(*p))
        .cloned()
        .collect();
    let modified: Vec<String> = parent_paths
        .iter()
        .filter(|p| {
            current_set.contains(*p)
                && files_content_eq(&parent_path.join(p), &current_dir.join(p))
                    .map(|eq| !eq)
                    .unwrap_or(false)
        })
        .cloned()
        .collect();

    Ok(SnapshotDiff {
        added,
        deleted,
        modified,
    })
}

pub fn get_commit_from_hash(repo_path: &Path, commit_hash: &str) -> Result<Commit, RepoError> {
    tracing::trace!("Getting commit from hash {}", commit_hash);

    let objects_dir = repo_path.join(GFS_DIR).join(OBJECTS_DIR);
    let (dir_part, file_part) = commit_hash.split_at(2);
    let object_path = objects_dir.join(dir_part).join(file_part);
    let commit_json = fs::read_to_string(object_path).map_err(RepoError::from)?;

    let commit: Commit =
        serde_json::from_str(&commit_json).map_err(|e| RepoError::InvalidConfig(e.to_string()))?;
    Ok(commit)
}

/// Walk back N generations from a commit following first-parent history.
/// Returns the ancestor commit hash, or error if path doesn't exist.
///
/// - `start_commit`: Starting commit hash (must be valid)
/// - `generations`: Number of parents to traverse (0 returns start_commit)
///
/// Returns `RepoError::RevisionNotFound` if ancestry path is too short.
pub fn get_ancestor_commit(
    repo_path: &Path,
    start_commit: &str,
    generations: usize,
) -> Result<String, RepoError> {
    if generations == 0 {
        return Ok(start_commit.to_string());
    }

    let mut current = start_commit.to_string();

    for generation_idx in 0..generations {
        // Handle special "0" marker (initial commit has no parent)
        if current == "0" {
            return Err(RepoError::RevisionNotFound(format!(
                "{}~{}: commit has no parent (reached initial commit at generation {})",
                start_commit, generations, generation_idx
            )));
        }

        // Load current commit
        let commit = get_commit_from_hash(repo_path, &current)?;

        // Get first parent
        let parent = commit
            .parents
            .as_ref()
            .and_then(|p| p.first())
            .ok_or_else(|| {
                RepoError::RevisionNotFound(format!(
                    "{}~{}: commit {} has no parent (generation {})",
                    start_commit, generations, current, generation_idx
                ))
            })?;

        // Check if parent is "0" (initial commit marker)
        if parent == "0" {
            return Err(RepoError::RevisionNotFound(format!(
                "{}~{}: reached initial commit at generation {} (parent is 0)",
                start_commit, generations, generation_idx
            )));
        }

        current = parent.clone();
    }

    Ok(current)
}

/// Deterministic bincode config for file entries (must match hash::hash_file_entries).
fn files_bincode_config() -> bincode::config::Configuration {
    bincode::config::standard()
}

/// Write the file entries list as a binary object under `.gfs/objects/<2>/<62>`.
/// Returns the content hash (64-char hex). Same layout as commit objects.
pub fn write_files_object(repo_path: &Path, entries: &[FileEntry]) -> Result<String, RepoError> {
    let bytes = bincode::serde::encode_to_vec(entries, files_bincode_config())
        .map_err(|e| RepoError::InvalidConfig(e.to_string()))?;
    let hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        format!("{:x}", hasher.finalize())
    };
    let (dir_part, file_part) = hash.split_at(2);
    let object_dir = repo_path.join(GFS_DIR).join(OBJECTS_DIR).join(dir_part);
    fs::create_dir_all(&object_dir).map_err(RepoError::from)?;
    fs::write(object_dir.join(file_part), &bytes).map_err(RepoError::from)?;
    Ok(hash)
}

/// Read the file entries list from the object store by its content hash.
pub fn get_file_entries_by_ref(
    repo_path: &Path,
    files_ref: &str,
) -> Result<Vec<FileEntry>, RepoError> {
    let objects_dir = repo_path.join(GFS_DIR).join(OBJECTS_DIR);
    let (dir_part, file_part) = files_ref.split_at(2);
    let object_path = objects_dir.join(dir_part).join(file_part);
    let bytes = fs::read(object_path).map_err(RepoError::from)?;
    bincode::serde::decode_from_slice(&bytes, files_bincode_config())
        .map_err(|e| RepoError::InvalidConfig(e.to_string()))
        .map(|(entries, _)| entries)
}

/// Resolve the file list for a commit. Returns `Some(entries)` if `files_ref` is set, else `None`.
pub fn get_file_entries_for_commit(
    repo_path: &Path,
    commit: &Commit,
) -> Result<Option<Vec<FileEntry>>, RepoError> {
    match &commit.files_ref {
        Some(hash) => get_file_entries_by_ref(repo_path, hash).map(Some),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Schema Object Storage
// ---------------------------------------------------------------------------

/// Write schema object to `.gfs/objects/<2>/<62>/` directory containing:
/// - `schema.json`: Structured metadata from DatasourceMetadata
/// - `schema.sql`: Native DDL dump (pg_dump --schema-only or mysqldump --no-data)
///
/// Returns the 64-char SHA-256 hash computed from schema.json content.
pub fn write_schema_object(
    repo_path: &Path,
    schema_metadata: &crate::model::datasource::DatasourceMetadata,
    schema_sql: &str,
) -> Result<String, RepoError> {
    use sha2::{Digest, Sha256};

    // 1. Serialize schema.json
    let schema_json = serde_json::to_string_pretty(schema_metadata)
        .map_err(|e| RepoError::InvalidConfig(format!("failed to serialize schema: {}", e)))?;

    // 2. Compute SHA-256 hash of schema.json content
    let mut hasher = Sha256::new();
    hasher.update(schema_json.as_bytes());
    let hash = format!("{:x}", hasher.finalize());

    // 3. Create directory: objects/<2>/<62>/
    let (dir_part, file_part) = hash.split_at(2);
    let schema_dir = repo_path
        .join(GFS_DIR)
        .join(OBJECTS_DIR)
        .join(dir_part)
        .join(file_part);
    fs::create_dir_all(&schema_dir).map_err(RepoError::from)?;

    // 4. Write both files
    fs::write(schema_dir.join("schema.json"), schema_json).map_err(RepoError::from)?;
    fs::write(schema_dir.join("schema.sql"), schema_sql).map_err(RepoError::from)?;

    Ok(hash)
}

/// Read schema object from `.gfs/objects/<2>/<62>/`.
/// Returns tuple of (DatasourceMetadata, schema_sql).
pub fn get_schema_by_hash(
    repo_path: &Path,
    schema_hash: &str,
) -> Result<(crate::model::datasource::DatasourceMetadata, String), RepoError> {
    let (dir_part, file_part) = schema_hash.split_at(2);
    let schema_dir = repo_path
        .join(GFS_DIR)
        .join(OBJECTS_DIR)
        .join(dir_part)
        .join(file_part);

    let schema_json = fs::read_to_string(schema_dir.join("schema.json"))
        .map_err(|e| RepoError::InvalidConfig(format!("failed to read schema.json: {}", e)))?;
    let schema_sql = fs::read_to_string(schema_dir.join("schema.sql"))
        .map_err(|e| RepoError::InvalidConfig(format!("failed to read schema.sql: {}", e)))?;

    let metadata: crate::model::datasource::DatasourceMetadata = serde_json::from_str(&schema_json)
        .map_err(|e| RepoError::InvalidConfig(format!("failed to parse schema.json: {}", e)))?;

    Ok((metadata, schema_sql))
}

pub fn get_snapshot_from_branch(repo_path: &Path, branch_name: &str) -> Result<String, RepoError> {
    tracing::trace!("Getting snapshot from branch {}", branch_name);

    let refs_dir = repo_path.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR);
    let branch_path = refs_dir.join(branch_name);
    let commit_hash = fs::read_to_string(branch_path).map_err(RepoError::from)?;

    let commit = get_commit_from_hash(repo_path, &commit_hash)?;

    Ok(commit.snapshot_hash)
}

pub fn get_snapshot_from_commit(repo_path: &Path, commit_hash: &str) -> Result<String, RepoError> {
    tracing::trace!("Getting snapshot from commit {}", commit_hash);

    let commit = get_commit_from_hash(repo_path, commit_hash)?;

    Ok(commit.snapshot_hash)
}

pub fn is_branch(repo_path: &Path, branch_name: &str) -> bool {
    let refs_dir = repo_path.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR);
    let branch_path = refs_dir.join(branch_name);
    branch_path.exists()
}

pub fn is_commit(repo_path: &Path, commit_hash: &str) -> bool {
    let objects_dir = repo_path.join(GFS_DIR).join(OBJECTS_DIR);
    let (dir_part, file_part) = commit_hash.split_at(2);
    let object_path = objects_dir.join(dir_part).join(file_part);
    object_path.exists()
}

pub fn update_head_with_branch(repo_path: &Path, branch_name: &str) -> Result<(), RepoError> {
    tracing::trace!("Updating HEAD to branch '{}'", branch_name);

    let head_path = repo_path.join(GFS_DIR).join(HEAD_FILE);
    fs::write(
        &head_path,
        format!("ref: {}/{}/{}", REFS_DIR, HEADS_DIR, branch_name),
    )
    .map_err(RepoError::from)?;

    Ok(())
}

pub fn update_head_with_commit(repo_path: &Path, commit_hash: &str) -> Result<(), RepoError> {
    tracing::trace!("Updating HEAD to commit '{}'", commit_hash);

    let head_path = repo_path.join(GFS_DIR).join(HEAD_FILE);
    fs::write(&head_path, commit_hash).map_err(RepoError::from)?;

    Ok(())
}

/// Update the ref file for `branch_name` to point to `commit_hash`.
///
/// Writes `refs/heads/<branch_name>` with the given commit hash. This is the
/// operation performed after a new commit is recorded to advance the branch tip.
pub fn update_branch_ref(
    repo_path: &Path,
    branch_name: &str,
    commit_hash: &str,
) -> Result<(), RepoError> {
    tracing::trace!("Updating branch '{}' ref to '{}'", branch_name, commit_hash);
    let ref_path = repo_path
        .join(GFS_DIR)
        .join(REFS_DIR)
        .join(HEADS_DIR)
        .join(branch_name);
    fs::write(&ref_path, commit_hash).map_err(RepoError::from)?;
    Ok(())
}

/// Resolves a human-readable revision to a commit id.
///
/// Supported formats:
/// - `"HEAD"` or empty: current commit
/// - Branch name: tip of branch from `refs/heads/<branch>`
/// - Full 64-char hex: validated commit hash
/// - `"0"`: initial commit marker
/// - `<revision>~<n>`: nth ancestor of revision (e.g., `HEAD~1`, `main~5`)
/// - `<revision>~`: same as `<revision>~1` (first parent)
pub fn rev_parse(repo_path: &Path, revision: &str) -> Result<String, RepoError> {
    let revision = revision.trim();

    // Check for tilde notation: <base>~<n>
    if let Some(tilde_pos) = revision.rfind('~') {
        let base_rev = &revision[..tilde_pos];
        let count_str = &revision[tilde_pos + 1..];

        // Parse generation count (default to 1 if empty or just ~)
        let generations = if count_str.is_empty() {
            1
        } else {
            count_str.parse::<usize>().map_err(|_| {
                RepoError::RevisionNotFound(format!(
                    "invalid tilde syntax '{}': count must be a positive integer",
                    revision
                ))
            })?
        };

        // Resolve base revision first
        let base_commit = rev_parse(repo_path, base_rev)?;

        // Walk back N generations
        return get_ancestor_commit(repo_path, &base_commit, generations);
    }

    // Original logic (unchanged)
    if revision.is_empty() || revision.eq_ignore_ascii_case(HEAD_FILE) {
        return get_current_commit_id(repo_path);
    }
    if revision == "0" {
        return Ok("0".to_string());
    }
    if is_branch(repo_path, revision) {
        let refs_dir = repo_path.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR);
        let branch_path = refs_dir.join(revision);
        let content = fs::read_to_string(branch_path).map_err(RepoError::from)?;
        return Ok(content.trim().to_string());
    }
    // Check for full 64-char hash (fast path)
    if revision.len() == 64
        && revision.chars().all(|c| c.is_ascii_hexdigit())
        && is_commit(repo_path, revision)
    {
        return Ok(revision.to_string());
    }

    // Check for short hash (4-63 chars, all hex)
    if revision.len() >= MIN_SHORT_HASH_LEN
        && revision.len() < 64
        && revision.chars().all(|c| c.is_ascii_hexdigit())
    {
        let matches = find_commits_by_prefix(repo_path, revision)?;

        match matches.len() {
            0 => {
                // No matches - fall through to error
            }
            1 => {
                // Unique match - return it
                return Ok(matches[0].clone());
            }
            _ => {
                // Ambiguous - return error with candidates
                return Err(RepoError::AmbiguousShortHash {
                    prefix: revision.to_string(),
                    matches,
                });
            }
        }
    }

    Err(RepoError::RevisionNotFound(revision.to_string()))
}

/// Find commits matching a short hash prefix.
///
/// Returns list of full 64-char hashes matching the given prefix.
/// Scans `.gfs/objects/` directory structure.
///
/// # Arguments
/// * `repo_path` - Repository root path
/// * `prefix` - Hex string prefix (must be 4-64 chars, all hex digits)
///
/// # Returns
/// * `Ok(Vec<String>)` - List of matching full hashes (empty if none found)
/// * `Err(RepoError)` - If prefix is invalid or I/O error
fn find_commits_by_prefix(repo_path: &Path, prefix: &str) -> Result<Vec<String>, RepoError> {
    // Validate prefix length (4-64 chars)
    if prefix.len() < MIN_SHORT_HASH_LEN || prefix.len() > 64 {
        return Err(RepoError::invalid_layout(format!(
            "short hash must be {}-64 characters, got {}",
            MIN_SHORT_HASH_LEN,
            prefix.len()
        )));
    }

    // Validate all hex characters
    if !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(RepoError::invalid_layout(format!(
            "short hash must contain only hex characters, got '{}'",
            prefix
        )));
    }

    let objects_dir = repo_path.join(GFS_DIR).join(OBJECTS_DIR);
    let mut matches = Vec::new();

    // Hash structure: <2-char-dir>/<62-char-file>
    let (dir_prefix, file_prefix) = if prefix.len() <= 2 {
        (prefix, "")
    } else {
        prefix.split_at(2)
    };

    // Scan object directories
    let entries = fs::read_dir(&objects_dir).map_err(RepoError::from)?;

    for entry in entries {
        let entry = entry.map_err(RepoError::from)?;
        let dir_name = entry.file_name();
        let dir_name_str = dir_name.to_string_lossy();

        // Skip directories that don't match prefix
        if !dir_name_str.starts_with(dir_prefix) {
            continue;
        }

        let dir_path = entry.path();
        if !dir_path.is_dir() {
            continue;
        }

        // Scan files in matching directories
        let file_entries = fs::read_dir(&dir_path).map_err(RepoError::from)?;

        for file_entry in file_entries {
            let file_entry = file_entry.map_err(RepoError::from)?;
            let file_name = file_entry.file_name();
            let file_name_str = file_name.to_string_lossy();

            // Check if file matches prefix
            if file_name_str.starts_with(file_prefix) {
                let full_hash = format!("{}{}", dir_name_str, file_name_str);

                // Verify full hash starts with original prefix
                if full_hash.starts_with(prefix) {
                    matches.push(full_hash);
                }
            }
        }
    }

    Ok(matches)
}

fn collect_branch_refs(dir: &Path, prefix: &str) -> Result<Vec<(String, String)>, RepoError> {
    let mut result = Vec::new();
    let entries = fs::read_dir(dir).map_err(RepoError::from)?;
    for entry in entries {
        let entry = entry.map_err(RepoError::from)?;
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| RepoError::invalid_layout("invalid ref name".to_string()))?;
        let branch_name = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", prefix, name)
        };
        if path.is_file() {
            let tip = fs::read_to_string(&path).map_err(RepoError::from)?;
            result.push((branch_name, tip.trim().to_string()));
        } else if path.is_dir() {
            let sub = collect_branch_refs(&path, &branch_name)?;
            result.extend(sub);
        }
    }
    Ok(result)
}

/// Returns the list of ref names pointing to the given commit hash.
///
/// For each branch in `refs/heads/` whose tip equals `commit_hash`, includes the branch name.
/// If HEAD (symbolic or detached) points to this commit, includes `"HEAD -> <branch>"` or `"HEAD"`.
pub fn get_refs_pointing_to(repo_path: &Path, commit_hash: &str) -> Result<Vec<String>, RepoError> {
    let gfs_dir = repo_path.join(GFS_DIR);
    let refs_dir = gfs_dir.join(REFS_DIR).join(HEADS_DIR);
    let head_path = gfs_dir.join(HEAD_FILE);

    let mut refs = Vec::new();
    let mut head_branch: Option<String> = None;

    let head_content = fs::read_to_string(&head_path).map_err(RepoError::from)?;
    let head_trimmed = head_content.trim();
    if head_trimmed.len() == 64 && head_trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        if head_trimmed == commit_hash {
            refs.push(HEAD_FILE.to_string());
        }
    } else if let Some(branch) =
        head_trimmed.strip_prefix(&format!("ref: {}/{}/", REFS_DIR, HEADS_DIR))
    {
        head_branch = Some(branch.trim().to_string());
    }

    let branch_refs = collect_branch_refs(&refs_dir, "")?;
    for (branch_name, tip) in branch_refs {
        if tip == commit_hash {
            if head_branch.as_deref() == Some(branch_name.as_str()) {
                refs.insert(0, format!("HEAD -> {}", branch_name));
            }
            refs.push(branch_name);
        }
    }

    Ok(refs)
}

/// Resolves the current HEAD to a commit id (hash or initial "0").
/// When HEAD is a branch ref, reads `refs/heads/<branch>`; when HEAD is a direct commit hash, returns it.
pub fn get_current_commit_id(repo_path: &Path) -> Result<String, RepoError> {
    let head = get_current_branch(repo_path)?;
    let gfs_dir = repo_path.join(GFS_DIR);
    if head.len() == 64 && head.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(head)
    } else {
        let ref_path = gfs_dir.join(REFS_DIR).join(HEADS_DIR).join(&head);
        let commit = fs::read_to_string(ref_path).map_err(RepoError::from)?;
        Ok(commit.trim().to_string())
    }
}

/// Short commit id used for workspace directory path segment (avoids long paths on disk).
/// If `commit_id` is longer than `SHORT_COMMIT_ID_LEN`, returns the prefix; otherwise returns as-is (e.g. `"0"`).
pub fn short_commit_id_for_workspace(commit_id: &str) -> String {
    if commit_id.len() <= SHORT_COMMIT_ID_LEN {
        commit_id.to_string()
    } else {
        commit_id
            .chars()
            .take(SHORT_COMMIT_ID_LEN)
            .collect::<String>()
    }
}

/// Format a commit hash for display.
///
/// Uses `DEFAULT_SHORT_HASH_LEN` (7 chars) for short format.
///
/// # Arguments
/// * `hash` - Full commit hash
/// * `full` - If true, return full hash; if false, return first 7 chars
pub fn format_commit_hash(hash: &str, full: bool) -> String {
    if full || hash.len() <= DEFAULT_SHORT_HASH_LEN {
        hash.to_string()
    } else {
        hash.chars().take(DEFAULT_SHORT_HASH_LEN).collect()
    }
}

/// Returns the path of the workspace data directory for the current HEAD.
/// - On a branch: `workspaces/<branch>/0/data` (one persistent workspace per branch).
/// - Detached HEAD: `workspaces/detached/<short_commit_id>/data`.
///
/// Does not create it.
pub fn get_workspace_data_dir_path(repo_path: &Path) -> Result<std::path::PathBuf, RepoError> {
    let branch = get_current_branch(repo_path)?;
    let commit_id = get_current_commit_id(repo_path)?;
    let gfs_dir = repo_path.join(GFS_DIR);
    let (branch_segment, workspace_segment): (String, String) =
        if branch.len() == 64 && branch.chars().all(|c| c.is_ascii_hexdigit()) {
            (
                "detached".to_string(),
                short_commit_id_for_workspace(&commit_id),
            )
        } else {
            (branch.clone(), BRANCH_WORKSPACE_SEGMENT.to_string())
        };
    Ok(gfs_dir
        .join(WORKSPACES_DIR)
        .join(&branch_segment)
        .join(&workspace_segment)
        .join(WORKSPACE_DATA_DIR))
}

/// Returns the workspace data directory for the current branch and commit, creating it if missing.
pub fn get_workspace_data_dir_for_head(repo_path: &Path) -> Result<std::path::PathBuf, RepoError> {
    let workspace_data_dir = get_workspace_data_dir_path(repo_path)?;
    fs::create_dir_all(&workspace_data_dir).map_err(RepoError::from)?;
    Ok(workspace_data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::layout::{
        GFS_DIR, HEAD_FILE, HEADS_DIR, MAIN_BRANCH, OBJECTS_DIR, REFS_DIR, WORKSPACE_DATA_DIR,
        WORKSPACES_DIR,
    };
    use chrono::Utc;
    use std::fs;
    use std::io;
    use tempfile::TempDir;

    #[test]
    fn short_commit_id_for_workspace_keeps_short_and_truncates_long() {
        assert_eq!(short_commit_id_for_workspace("0"), "0");
        assert_eq!(short_commit_id_for_workspace("abc"), "abc");
        let long = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(short_commit_id_for_workspace(long), "0123456789ab");
    }

    fn create_valid_repo_layout(path: &Path) -> io::Result<()> {
        let gfs = path.join(GFS_DIR);
        fs::create_dir_all(&gfs)?;

        fs::write(
            gfs.join(HEAD_FILE),
            format!("ref: {}/{}/{}", REFS_DIR, HEADS_DIR, MAIN_BRANCH),
        )?;

        let config_toml = r#"version = "1"
[repository]
name = "test-repo"
"#;
        fs::write(gfs.join(CONFIG_FILE), config_toml)?;

        let commit_id = "0000000000000000000000000000000000000000";
        fs::create_dir_all(gfs.join(REFS_DIR).join(HEADS_DIR))?;
        fs::write(
            gfs.join(REFS_DIR).join(HEADS_DIR).join(MAIN_BRANCH),
            commit_id,
        )?;

        fs::create_dir(gfs.join(OBJECTS_DIR))?;

        // workspace for current branch: workspaces/main/0/data (one persistent workspace per branch)
        fs::create_dir_all(
            gfs.join(WORKSPACES_DIR)
                .join(MAIN_BRANCH)
                .join(BRANCH_WORKSPACE_SEGMENT)
                .join(WORKSPACE_DATA_DIR),
        )?;

        Ok(())
    }

    #[test]
    fn test_validate_repo_layout_valid() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let repo_dir = temp_dir.path().join("repo");
        fs::create_dir(&repo_dir)?;

        // Create a valid repository layout
        create_valid_repo_layout(&repo_dir)?;

        // Validate the repository layout (.gfs dir)
        let result = validate_repo_layout(&repo_dir.join(GFS_DIR));
        assert!(
            result.is_ok(),
            "Valid repository layout should pass validation"
        );

        Ok(())
    }

    #[test]
    fn directory_logical_and_physical_size_bytes() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let dir = temp.path();
        fs::write(dir.join("a"), "hello")?; // 5 bytes
        fs::write(dir.join("b"), "world")?; // 5 bytes
        fs::create_dir(dir.join("sub"))?;
        fs::write(dir.join("sub").join("c"), "x")?; // 1 byte
        let logical = directory_logical_size_bytes(dir)?;
        assert_eq!(logical, 11, "logical = 5+5+1");
        let physical = directory_physical_size_bytes(dir)?;
        assert!(
            physical >= logical,
            "physical (blocks) should be >= logical"
        );
        Ok(())
    }

    #[test]
    fn test_validate_repo_layout_missing_head() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        fs::create_dir_all(repo_dir.join(GFS_DIR)).unwrap();

        // Create everything except HEAD under .gfs
        let config_toml = r#"version = "1"
[repository]
name = "test-repo"
"#;
        fs::write(repo_dir.join(GFS_DIR).join(CONFIG_FILE), config_toml).unwrap();
        fs::create_dir_all(repo_dir.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR)).unwrap();
        fs::write(
            repo_dir
                .join(GFS_DIR)
                .join(REFS_DIR)
                .join(HEADS_DIR)
                .join(MAIN_BRANCH),
            "0000000000000000000000000000000000000000",
        )
        .unwrap();
        fs::create_dir(repo_dir.join(GFS_DIR).join(OBJECTS_DIR)).unwrap();

        let result = validate_repo_layout(&repo_dir.join(GFS_DIR));
        match result {
            Err(RepoError::MissingFile(path)) => {
                assert_eq!(path, repo_dir.join(GFS_DIR).join(HEAD_FILE));
            }
            _ => panic!("Expected MissingFile error"),
        }
    }

    #[test]
    fn test_validate_repo_layout_invalid_config() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        fs::create_dir_all(repo_dir.join(GFS_DIR)).unwrap();

        fs::write(repo_dir.join(GFS_DIR).join(CONFIG_FILE), "invalid toml [[").unwrap();
        fs::write(
            repo_dir.join(GFS_DIR).join(HEAD_FILE),
            format!("ref: {}/{}/{}", REFS_DIR, HEADS_DIR, MAIN_BRANCH),
        )
        .unwrap();
        fs::create_dir_all(repo_dir.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR)).unwrap();
        fs::write(
            repo_dir
                .join(GFS_DIR)
                .join(REFS_DIR)
                .join(HEADS_DIR)
                .join(MAIN_BRANCH),
            "0000000000000000000000000000000000000000",
        )
        .unwrap();
        fs::create_dir(repo_dir.join(GFS_DIR).join(OBJECTS_DIR)).unwrap();

        let result = validate_repo_layout(&repo_dir.join(GFS_DIR));
        assert!(matches!(result, Err(RepoError::InvalidConfig(_))));
    }

    #[test]
    fn test_validate_repo_layout_missing_workspace() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        fs::create_dir_all(repo_dir.join(GFS_DIR)).unwrap();

        // Create everything except workspaces/main/<commit_id>/data
        let config_toml = r#"version = "1"
[repository]
name = "test-repo"
"#;
        fs::write(repo_dir.join(GFS_DIR).join(CONFIG_FILE), config_toml).unwrap();
        fs::write(
            repo_dir.join(GFS_DIR).join(HEAD_FILE),
            format!("ref: {}/{}/{}", REFS_DIR, HEADS_DIR, MAIN_BRANCH),
        )
        .unwrap();
        fs::create_dir_all(repo_dir.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR)).unwrap();
        fs::write(
            repo_dir
                .join(GFS_DIR)
                .join(REFS_DIR)
                .join(HEADS_DIR)
                .join(MAIN_BRANCH),
            "0000000000000000000000000000000000000000",
        )
        .unwrap();
        fs::create_dir(repo_dir.join(GFS_DIR).join(OBJECTS_DIR)).unwrap();

        let result = validate_repo_layout(&repo_dir.join(GFS_DIR));
        match result {
            Err(RepoError::InvalidLayout(msg)) => {
                assert!(
                    msg.contains("workspace data directory"),
                    "expected message about workspace data directory, got: {}",
                    msg
                );
            }
            _ => panic!("Expected InvalidLayout error, got {:?}", result),
        }
    }

    #[test]
    fn test_get_current_branch() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        let result = get_current_branch(&repo_dir).unwrap();
        assert!(result == MAIN_BRANCH);
    }

    #[test]
    fn init_repo_layout_fails_when_already_initialized() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        let result = init_repo_layout(&repo_dir, None);
        assert!(matches!(result, Err(RepoError::AlreadyInitialized(_))));
    }

    #[test]
    fn test_get_snapshot_from_branch() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        //write to branch the commit hash
        fs::write(
            repo_dir
                .join(GFS_DIR)
                .join(REFS_DIR)
                .join(HEADS_DIR)
                .join(MAIN_BRANCH),
            "1000000000000000000000000000000000000000",
        )
        .unwrap();
        fs::create_dir_all(repo_dir.join(GFS_DIR).join(OBJECTS_DIR).join("10"))
            .map_err(RepoError::from)
            .unwrap();

        let commit = Commit {
            hash: Some("1000000000000000000000000000000000000000".to_string()),
            message: "Initial commit".to_string(),
            timestamp: Utc::now(),
            author: "test".to_string(),
            author_email: None,
            author_date: Utc::now(),
            committer: "test".to_string(),
            committer_email: None,
            committer_date: Utc::now(),
            parents: None,
            snapshot_hash: "0000000000000000000000000000000000000000".to_string(),
            schema_hash: None,
            database_provider: None,
            database_version: None,
            files_added: None,
            files_deleted: None,
            files_modified: None,
            files_renamed: None,
            files_ref: None,
            files_count: None,
            snapshot_size_bytes: None,
            blocks_added: None,
            blocks_deleted: None,
            db_objects_added: None,
            db_objects_deleted: None,
            db_objects_modified: None,
        };

        let commit_path = repo_dir
            .join(GFS_DIR)
            .join(OBJECTS_DIR)
            .join("10")
            .join("00000000000000000000000000000000000000");
        fs::write(&commit_path, serde_json::to_string(&commit).unwrap()).unwrap();

        let result = get_snapshot_from_branch(&repo_dir, MAIN_BRANCH).unwrap();
        assert!(result == "0000000000000000000000000000000000000000");
    }

    #[test]
    fn is_branch_true() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        // Branch "main" should exist
        let exists = is_branch(&repo_dir, MAIN_BRANCH);
        assert!(exists);

        // Branch "develop" should not exist
        let not_exists = is_branch(&repo_dir, "develop");
        assert!(!not_exists);

        // Create "develop" branch
        let heads_dir = repo_dir.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR);
        fs::write(heads_dir.join("develop"), "deadbeef").unwrap();

        // Now "develop" should exist
        let now_exists = is_branch(&repo_dir, "develop");
        assert!(now_exists);
    }

    #[test]
    fn is_branch_long_name_true() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        // Branch "feature/1234" should not exist
        let not_exists = is_branch(&repo_dir, "feature/1234");
        assert!(!not_exists);

        // Create "feature/1234" branch
        let heads_dir = repo_dir.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR);
        fs::create_dir_all(heads_dir.join("feature")).unwrap();
        fs::write(heads_dir.join("feature/1234"), "deadbeef").unwrap();

        // Now "feature/1234" should exist
        let now_exists = is_branch(&repo_dir, "feature/1234");
        assert!(now_exists);
    }

    #[test]
    fn is_branch_false() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        // Branch "feature" should not exist
        assert!(!is_branch(&repo_dir, "feature/123"));

        // Remove "main" branch file
        let heads_dir = repo_dir.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR);
        fs::remove_file(heads_dir.join(MAIN_BRANCH)).unwrap();

        // Now "main" should not exist as a branch
        assert!(!is_branch(&repo_dir, MAIN_BRANCH));
    }

    #[test]
    fn is_commit_true() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        // Create commit with sha256
        let objects_dir = repo_dir.join(GFS_DIR).join(OBJECTS_DIR);
        fs::create_dir_all(objects_dir.join("00")).unwrap();
        fs::write(
            objects_dir
                .join("00")
                .join("0000000000000000000000000000000000000000"),
            "{}",
        )
        .unwrap();

        // Commit "0" should exist
        assert!(is_commit(
            &repo_dir,
            "000000000000000000000000000000000000000000"
        ));
    }

    #[test]
    fn is_commit_false() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        // Commit "0" should not exist
        assert!(!is_commit(
            &repo_dir,
            "000000000000000000000000000000000000000000"
        ));
    }

    #[test]
    fn rev_parse_head_returns_current_commit() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        let result = rev_parse(&repo_dir, HEAD_FILE).unwrap();
        assert_eq!(result, "0");
    }

    #[test]
    fn rev_parse_branch_returns_branch_tip() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        let hash = "ab00000000000000000000000000000000000000000000000000000000000000";
        let (dir_part, file_part) = hash.split_at(2);
        fs::write(
            repo_dir
                .join(GFS_DIR)
                .join(REFS_DIR)
                .join(HEADS_DIR)
                .join(MAIN_BRANCH),
            hash,
        )
        .unwrap();
        fs::create_dir_all(repo_dir.join(GFS_DIR).join(OBJECTS_DIR).join(dir_part)).unwrap();
        let commit = Commit {
            hash: Some(hash.to_string()),
            message: "test".to_string(),
            timestamp: Utc::now(),
            author: "test".to_string(),
            author_email: None,
            author_date: Utc::now(),
            committer: "test".to_string(),
            committer_email: None,
            committer_date: Utc::now(),
            parents: None,
            snapshot_hash: "00".to_string(),
            schema_hash: None,
            database_provider: None,
            database_version: None,
            files_added: None,
            files_deleted: None,
            files_modified: None,
            files_renamed: None,
            files_ref: None,
            files_count: None,
            snapshot_size_bytes: None,
            blocks_added: None,
            blocks_deleted: None,
            db_objects_added: None,
            db_objects_deleted: None,
            db_objects_modified: None,
        };
        fs::write(
            repo_dir
                .join(GFS_DIR)
                .join(OBJECTS_DIR)
                .join(dir_part)
                .join(file_part),
            serde_json::to_string(&commit).unwrap(),
        )
        .unwrap();

        let result = rev_parse(&repo_dir, MAIN_BRANCH).unwrap();
        assert_eq!(result, hash);
    }

    #[test]
    fn rev_parse_full_hash_returns_hash() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        let hash = "05b7f94787d35209e01292a9da8c844cb59ff907758ba81f1f60eba3b437b2f1";
        let (dir_part, file_part) = hash.split_at(2);
        fs::create_dir_all(repo_dir.join(GFS_DIR).join(OBJECTS_DIR).join(dir_part)).unwrap();
        let commit = Commit {
            hash: Some(hash.to_string()),
            message: "test".to_string(),
            timestamp: Utc::now(),
            author: "test".to_string(),
            author_email: None,
            author_date: Utc::now(),
            committer: "test".to_string(),
            committer_email: None,
            committer_date: Utc::now(),
            parents: None,
            snapshot_hash: "00".to_string(),
            schema_hash: None,
            database_provider: None,
            database_version: None,
            files_added: None,
            files_deleted: None,
            files_modified: None,
            files_renamed: None,
            files_ref: None,
            files_count: None,
            snapshot_size_bytes: None,
            blocks_added: None,
            blocks_deleted: None,
            db_objects_added: None,
            db_objects_deleted: None,
            db_objects_modified: None,
        };
        let object_path = repo_dir
            .join(GFS_DIR)
            .join(OBJECTS_DIR)
            .join(dir_part)
            .join(file_part);
        fs::write(&object_path, serde_json::to_string(&commit).unwrap()).unwrap();

        let result = rev_parse(&repo_dir, hash).unwrap();
        assert_eq!(result, hash);
    }

    #[test]
    fn rev_parse_zero_returns_zero() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        let result = rev_parse(&repo_dir, "0").unwrap();
        assert_eq!(result, "0");
    }

    #[test]
    fn rev_parse_invalid_returns_error() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        let result = rev_parse(&repo_dir, "unknown-branch");
        assert!(matches!(result, Err(RepoError::RevisionNotFound(_))));
    }

    #[test]
    fn get_refs_pointing_to_branch_tip() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        let hash = "ef00000000000000000000000000000000000000000000000000000000000000";
        let (dir_part, file_part) = hash.split_at(2);
        fs::write(
            repo_dir
                .join(GFS_DIR)
                .join(REFS_DIR)
                .join(HEADS_DIR)
                .join(MAIN_BRANCH),
            hash,
        )
        .unwrap();
        fs::create_dir_all(repo_dir.join(GFS_DIR).join(OBJECTS_DIR).join(dir_part)).unwrap();
        let commit = Commit {
            hash: Some(hash.to_string()),
            message: "test".to_string(),
            timestamp: Utc::now(),
            author: "test".to_string(),
            author_email: None,
            author_date: Utc::now(),
            committer: "test".to_string(),
            committer_email: None,
            committer_date: Utc::now(),
            parents: None,
            snapshot_hash: "00".to_string(),
            schema_hash: None,
            database_provider: None,
            database_version: None,
            files_added: None,
            files_deleted: None,
            files_modified: None,
            files_renamed: None,
            files_ref: None,
            files_count: None,
            snapshot_size_bytes: None,
            blocks_added: None,
            blocks_deleted: None,
            db_objects_added: None,
            db_objects_deleted: None,
            db_objects_modified: None,
        };
        fs::write(
            repo_dir
                .join(GFS_DIR)
                .join(OBJECTS_DIR)
                .join(dir_part)
                .join(file_part),
            serde_json::to_string(&commit).unwrap(),
        )
        .unwrap();

        let refs = get_refs_pointing_to(&repo_dir, hash).unwrap();
        assert!(refs.contains(&"HEAD -> main".to_string()));
        assert!(refs.contains(&MAIN_BRANCH.to_string()));
    }

    #[test]
    fn get_refs_pointing_to_detached_head() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        let hash = "12abcd0000000000000000000000000000000000000000000000000000000000";
        let (dir_part, file_part) = hash.split_at(2);
        fs::write(repo_dir.join(GFS_DIR).join(HEAD_FILE), hash).unwrap();
        fs::write(
            repo_dir
                .join(GFS_DIR)
                .join(REFS_DIR)
                .join(HEADS_DIR)
                .join(MAIN_BRANCH),
            "0",
        )
        .unwrap();
        fs::create_dir_all(repo_dir.join(GFS_DIR).join(OBJECTS_DIR).join(dir_part)).unwrap();
        let commit = Commit {
            hash: Some(hash.to_string()),
            message: "test".to_string(),
            timestamp: Utc::now(),
            author: "test".to_string(),
            author_email: None,
            author_date: Utc::now(),
            committer: "test".to_string(),
            committer_email: None,
            committer_date: Utc::now(),
            parents: None,
            snapshot_hash: "00".to_string(),
            schema_hash: None,
            database_provider: None,
            database_version: None,
            files_added: None,
            files_deleted: None,
            files_modified: None,
            files_renamed: None,
            files_ref: None,
            files_count: None,
            snapshot_size_bytes: None,
            blocks_added: None,
            blocks_deleted: None,
            db_objects_added: None,
            db_objects_deleted: None,
            db_objects_modified: None,
        };
        fs::write(
            repo_dir
                .join(GFS_DIR)
                .join(OBJECTS_DIR)
                .join(dir_part)
                .join(file_part),
            serde_json::to_string(&commit).unwrap(),
        )
        .unwrap();

        let refs = get_refs_pointing_to(&repo_dir, hash).unwrap();
        assert_eq!(refs, vec![HEAD_FILE.to_string()]);
    }

    #[test]
    fn test_get_ancestor_commit_zero_generations() {
        let (_temp, repo, commits) = setup_test_repo_with_commits(3);
        let result = get_ancestor_commit(&repo, &commits[2], 0).unwrap();
        assert_eq!(result, commits[2]);
    }

    #[test]
    fn test_get_ancestor_commit_one_generation() {
        let (_temp, repo, commits) = setup_test_repo_with_commits(3);
        let result = get_ancestor_commit(&repo, &commits[2], 1).unwrap();
        assert_eq!(result, commits[1]);
    }

    #[test]
    fn test_get_ancestor_commit_multiple_generations() {
        let (_temp, repo, commits) = setup_test_repo_with_commits(5);
        let result = get_ancestor_commit(&repo, &commits[4], 3).unwrap();
        assert_eq!(result, commits[1]);
    }

    #[test]
    fn test_get_ancestor_commit_all_the_way_back() {
        let (_temp, repo, commits) = setup_test_repo_with_commits(5);
        let result = get_ancestor_commit(&repo, &commits[4], 4).unwrap();
        assert_eq!(result, commits[0]);
    }

    #[test]
    fn test_get_ancestor_commit_too_far_returns_error() {
        let (_temp, repo, commits) = setup_test_repo_with_commits(3);
        let result = get_ancestor_commit(&repo, &commits[2], 5);
        assert!(matches!(result, Err(RepoError::RevisionNotFound(_))));
    }

    #[test]
    fn test_rev_parse_head_tilde_one() {
        let (_temp, repo, commits) = setup_test_repo_with_commits(3);
        let result = rev_parse(&repo, "HEAD~1").unwrap();
        assert_eq!(result, commits[1]);
    }

    #[test]
    fn test_rev_parse_head_tilde_two() {
        let (_temp, repo, commits) = setup_test_repo_with_commits(5);
        let result = rev_parse(&repo, "HEAD~2").unwrap();
        assert_eq!(result, commits[2]);
    }

    #[test]
    fn test_rev_parse_branch_tilde_notation() {
        let (_temp, repo, commits) = setup_test_repo_with_commits(4);
        let result = rev_parse(&repo, "main~2").unwrap();
        assert_eq!(result, commits[1]);
    }

    #[test]
    fn test_rev_parse_hash_tilde_notation() {
        let (_temp, repo, commits) = setup_test_repo_with_commits(5);
        let hash = &commits[4];
        let result = rev_parse(&repo, &format!("{}~3", hash)).unwrap();
        assert_eq!(result, commits[1]);
    }

    #[test]
    fn test_rev_parse_tilde_without_number_defaults_to_one() {
        let (_temp, repo, commits) = setup_test_repo_with_commits(3);
        let result = rev_parse(&repo, "HEAD~").unwrap();
        assert_eq!(result, commits[1]);
    }

    #[test]
    fn test_rev_parse_tilde_zero_returns_same_commit() {
        let (_temp, repo, commits) = setup_test_repo_with_commits(3);
        let result = rev_parse(&repo, "HEAD~0").unwrap();
        assert_eq!(result, commits[2]);
    }

    #[test]
    fn test_rev_parse_invalid_tilde_count() {
        let (_temp, repo, _commits) = setup_test_repo_with_commits(3);
        let result = rev_parse(&repo, "HEAD~abc");
        assert!(matches!(result, Err(RepoError::RevisionNotFound(_))));
    }

    #[test]
    fn test_rev_parse_tilde_exceeds_history() {
        let (_temp, repo, _commits) = setup_test_repo_with_commits(3);
        let result = rev_parse(&repo, "HEAD~10");
        assert!(matches!(result, Err(RepoError::RevisionNotFound(_))));
    }

    fn setup_test_repo_with_commits(commit_count: usize) -> (TempDir, PathBuf, Vec<String>) {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        let mut commits = Vec::new();
        let mut parent = None;

        for i in 0..commit_count {
            let hash = format!("{:0>64}", i); // Fake hash for testing
            let gfs_dir = repo_dir.join(GFS_DIR);
            let objects_dir = gfs_dir.join(OBJECTS_DIR);
            let (dir, file) = hash.split_at(2);
            fs::create_dir_all(objects_dir.join(dir)).unwrap();

            let commit = Commit {
                hash: Some(hash.clone()),
                message: format!("Commit {}", i),
                timestamp: Utc::now(),
                parents: parent.as_ref().map(|p: &String| vec![p.clone()]),
                snapshot_hash: format!("snap{}", i),
                author: "test".into(),
                author_email: None,
                author_date: Utc::now(),
                committer: "test".into(),
                committer_email: None,
                committer_date: Utc::now(),
                schema_hash: None,
                database_provider: None,
                database_version: None,
                files_added: None,
                files_deleted: None,
                files_modified: None,
                files_renamed: None,
                files_ref: None,
                files_count: None,
                snapshot_size_bytes: None,
                blocks_added: None,
                blocks_deleted: None,
                db_objects_added: None,
                db_objects_deleted: None,
                db_objects_modified: None,
            };

            let commit_json = serde_json::to_string_pretty(&commit).unwrap();
            fs::write(objects_dir.join(dir).join(file), commit_json).unwrap();

            commits.push(hash.clone());
            parent = Some(hash);
        }

        // Update main branch to point to last commit
        let main_ref = repo_dir
            .join(GFS_DIR)
            .join(REFS_DIR)
            .join(HEADS_DIR)
            .join(MAIN_BRANCH);
        fs::write(main_ref, commits.last().unwrap()).unwrap();

        // Update HEAD to point to main branch
        let head_path = repo_dir.join(GFS_DIR).join(HEAD_FILE);
        fs::write(
            head_path,
            format!("ref: {}/{}/{}", REFS_DIR, HEADS_DIR, MAIN_BRANCH),
        )
        .unwrap();

        (temp_dir, repo_dir, commits)
    }

    // -------------------------------------------------------------------------
    // Short Hash Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_find_commits_by_prefix_unique_match() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        // Create commits with distinct prefixes
        let hash1 = "a1b2c3d4e5f60000000000000000000000000000000000000000000000000000";
        let hash2 = "b1b2c3d4e5f60000000000000000000000000000000000000000000000000000";

        create_test_commit_with_hash(&repo_dir, hash1, None);
        create_test_commit_with_hash(&repo_dir, hash2, Some(hash1));

        // Search with unique prefix for hash2
        let matches = find_commits_by_prefix(&repo_dir, "b1b2c3").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], hash2);
    }

    #[test]
    fn test_find_commits_by_prefix_no_match() {
        let (_temp, repo, _commits) = setup_test_repo_with_commits(3);
        // Search for non-existent prefix
        let matches = find_commits_by_prefix(&repo, "ffffffff").unwrap();
        assert_eq!(matches.len(), 0);
    }

    #[test]
    fn test_find_commits_by_prefix_too_short() {
        let (_temp, repo, _commits) = setup_test_repo_with_commits(3);
        // Try 3-char prefix (minimum is 4)
        let result = find_commits_by_prefix(&repo, "abc");
        assert!(matches!(result, Err(RepoError::InvalidLayout(_))));
    }

    #[test]
    fn test_find_commits_by_prefix_invalid_chars() {
        let (_temp, repo, _commits) = setup_test_repo_with_commits(3);
        // Try prefix with 'g' (not a hex digit)
        let result = find_commits_by_prefix(&repo, "abc12g");
        assert!(matches!(result, Err(RepoError::InvalidLayout(_))));
    }

    #[test]
    fn test_rev_parse_short_hash_unique() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        // Create commits with distinct prefixes
        let hash = "a1b2c3d4e5f60000000000000000000000000000000000000000000000000000";
        create_test_commit_with_hash(&repo_dir, hash, None);

        // Use 7-char prefix (Git-like)
        let result = rev_parse(&repo_dir, "a1b2c3d").unwrap();
        assert_eq!(result, hash);
    }

    #[test]
    fn test_rev_parse_short_hash_minimum_length() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        // Create commit with distinct prefix
        let hash = "abcd1234e5f60000000000000000000000000000000000000000000000000000";
        create_test_commit_with_hash(&repo_dir, hash, None);

        // Use 4-char prefix (minimum length)
        let result = rev_parse(&repo_dir, "abcd").unwrap();
        assert_eq!(result, hash);
    }

    #[test]
    fn test_rev_parse_short_hash_with_tilde() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        // Create commits with distinct prefixes
        let hash1 = "a1111111111111111111111111111111111111111111111111111111111111111";
        let hash2 = "b2222222222222222222222222222222222222222222222222222222222222222";
        let hash3 = "c3333333333333333333333333333333333333333333333333333333333333333";

        create_test_commit_with_hash(&repo_dir, hash1, None);
        create_test_commit_with_hash(&repo_dir, hash2, Some(hash1));
        create_test_commit_with_hash(&repo_dir, hash3, Some(hash2));

        // Update main branch to point to hash3
        let main_ref = repo_dir
            .join(GFS_DIR)
            .join(REFS_DIR)
            .join(HEADS_DIR)
            .join(MAIN_BRANCH);
        fs::write(main_ref, hash3).unwrap();

        // Use short hash with tilde notation
        let result = rev_parse(&repo_dir, "c333333~2").unwrap();
        assert_eq!(result, hash1);
    }

    #[test]
    fn test_rev_parse_short_hash_not_found() {
        let (_temp, repo, _commits) = setup_test_repo_with_commits(3);
        // Try non-existent short hash
        let result = rev_parse(&repo, "ffffffff");
        assert!(matches!(result, Err(RepoError::RevisionNotFound(_))));
    }

    #[test]
    fn test_format_commit_hash_short() {
        let hash = "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678";
        let result = format_commit_hash(hash, false);
        assert_eq!(result, "a1b2c3d");
        assert_eq!(result.len(), DEFAULT_SHORT_HASH_LEN);
    }

    #[test]
    fn test_format_commit_hash_full() {
        let hash = "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678";
        let result = format_commit_hash(hash, true);
        assert_eq!(result, hash);
        assert_eq!(result.len(), 64);
    }

    #[test]
    fn test_format_commit_hash_already_short() {
        let hash = "abc123";
        let result = format_commit_hash(hash, false);
        assert_eq!(result, hash);
    }

    // Helper function to create commits with specific hash prefixes
    fn create_test_commit_with_hash(repo_dir: &Path, hash: &str, parent: Option<&str>) {
        let gfs_dir = repo_dir.join(GFS_DIR);
        let objects_dir = gfs_dir.join(OBJECTS_DIR);
        let (dir, file) = hash.split_at(2);
        fs::create_dir_all(objects_dir.join(dir)).unwrap();

        let commit = Commit {
            hash: Some(hash.to_string()),
            message: "Test commit".to_string(),
            timestamp: Utc::now(),
            parents: parent.map(|p| vec![p.to_string()]),
            snapshot_hash: "snap".to_string(),
            author: "test".into(),
            author_email: None,
            author_date: Utc::now(),
            committer: "test".into(),
            committer_email: None,
            committer_date: Utc::now(),
            schema_hash: None,
            database_provider: None,
            database_version: None,
            files_added: None,
            files_deleted: None,
            files_modified: None,
            files_renamed: None,
            files_ref: None,
            files_count: None,
            snapshot_size_bytes: None,
            blocks_added: None,
            blocks_deleted: None,
            db_objects_added: None,
            db_objects_deleted: None,
            db_objects_modified: None,
        };

        let commit_json = serde_json::to_string_pretty(&commit).unwrap();
        fs::write(objects_dir.join(dir).join(file), commit_json).unwrap();
    }

    #[test]
    fn test_rev_parse_ambiguous_short_hash() {
        let temp_dir = TempDir::new().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        init_repo_layout(&repo_dir, None).unwrap();

        // Create two commits with same prefix
        let hash1 = "abc1234567890000000000000000000000000000000000000000000000000000";
        let hash2 = "abc1234567890111111111111111111111111111111111111111111111111111";

        create_test_commit_with_hash(&repo_dir, hash1, None);
        create_test_commit_with_hash(&repo_dir, hash2, Some(hash1));

        // Try to resolve with ambiguous prefix
        let result = rev_parse(&repo_dir, "abc12345");
        assert!(matches!(result, Err(RepoError::AmbiguousShortHash { .. })));

        // Check error message contains both hashes
        if let Err(RepoError::AmbiguousShortHash { prefix, matches }) = result {
            assert_eq!(prefix, "abc12345");
            assert_eq!(matches.len(), 2);
            assert!(matches.contains(&hash1.to_string()));
            assert!(matches.contains(&hash2.to_string()));
        }
    }

    #[test]
    fn repair_marker_path_normal_returns_sibling_dotfile() {
        let data_dir = std::path::Path::new("/foo/bar/data");
        let marker = repair_marker_path(data_dir);
        assert_eq!(
            marker,
            Some(std::path::PathBuf::from("/foo/bar/.needs-repair"))
        );
    }

    #[test]
    fn repair_marker_path_root_returns_none() {
        let marker = repair_marker_path(std::path::Path::new("/"));
        assert_eq!(marker, None);
    }

    #[test]
    fn repair_marker_path_bare_component_returns_none() {
        let marker = repair_marker_path(std::path::Path::new("data"));
        assert_eq!(marker, None);
    }
}
