use std::path::{Path, PathBuf};

use async_trait::async_trait;
use thiserror::Error;

use crate::model::commit::{CommitWithRefs, NewCommit};
use crate::model::config::UserConfig;
use crate::model::config::{EnvironmentConfig, RuntimeConfig};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("repository not found at '{0}'")]
    NotFound(String),

    #[error("revision not found: '{0}'")]
    RevisionNotFound(String),

    #[error("branch already exists: '{0}'")]
    BranchAlreadyExists(String),

    #[error("merge conflict: {0}")]
    Conflict(String),

    #[error("repository already initialized at '{0}'")]
    AlreadyInitialized(String),

    #[error("remote error ({remote}): {message}")]
    Remote { remote: String, message: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal error: {0}")]
    Internal(String),

    #[error(
        "cannot commit: HEAD is detached at commit '{0}'; create and switch to a branch here first (gfs checkout -b <name>), then commit"
    )]
    DetachedHead(String),
}

pub type Result<T> = std::result::Result<T, RepositoryError>;

// ---------------------------------------------------------------------------
// Supporting types
// ---------------------------------------------------------------------------

/// Options controlling the `log` operation.
#[derive(Debug, Default)]
pub struct LogOptions {
    /// Start traversal from this revision instead of `HEAD`.
    pub from: Option<String>,

    /// Stop traversal at this revision (exclusive).
    pub until: Option<String>,

    /// Maximum number of commits to return. `None` means unbounded.
    pub limit: Option<usize>,
}

/// Options controlling the `push` / `pull` / `fetch` operations.
#[derive(Debug, Default)]
pub struct RemoteOptions {
    /// Remote name (e.g. `"origin"`). Defaults to the tracked remote.
    pub remote: Option<String>,

    /// Refspec to transfer (e.g. `"refs/heads/main"`).
    /// Defaults to the current branch's upstream.
    pub refspec: Option<String>,
}

// ---------------------------------------------------------------------------
// Port
// ---------------------------------------------------------------------------

/// Port that abstracts git-like version-control operations over a repository.
///
/// Implementations may target a local GFS store, a libgit2-backed git repo,
/// or any other VCS backend.
///
/// All methods return `Send` futures so they can be driven by a multi-threaded
/// async runtime. Implement the trait with `#[async_trait]` to use as `dyn Repository`.
#[async_trait]
pub trait Repository: Send + Sync {
    /// Initialise a new, empty repository at `path`, with an optional mount point written to config.
    async fn init(&self, path: &Path, mount_point: Option<String>) -> Result<()>;

    /// Return the workspace data directory for the current HEAD commit.
    /// Used to provide the host-side bind-mount path when provisioning a database.
    async fn get_workspace_data_dir_for_head(&self, repo: &Path) -> Result<PathBuf>;

    /// Return the **active** workspace data directory — the directory where the
    /// database is actually running right now.
    ///
    /// This is the value stored in `.gfs/WORKSPACE`, which is written by
    /// `gfs init` and updated by `gfs checkout`.  Unlike
    /// `get_workspace_data_dir_for_head`, this path stays constant across
    /// commits so the commit use case always snapshots the correct directory.
    async fn get_active_workspace_data_dir(&self, repo: &Path) -> Result<PathBuf>;

    /// Persist the environment section (database provider / version) into the repo config.
    async fn update_environment_config(&self, repo: &Path, config: EnvironmentConfig)
    -> Result<()>;

    /// Persist the runtime section (e.g. container name/id) into the repo config.
    /// Called after provisioning a database so that commit can pause/unpause the container.
    async fn update_runtime_config(&self, repo: &Path, config: RuntimeConfig) -> Result<()>;

    /// Clone the repository at `url` into `target`.
    async fn clone_repo(&self, url: &str, target: &Path) -> Result<()>;

    /// Record a new commit and return its full OID (hash).
    async fn commit(&self, repo: &Path, new_commit: NewCommit) -> Result<String>;

    /// Check out `revision` (branch name, tag, or OID) in `repo`.
    async fn checkout(&self, repo: &Path, revision: &str) -> Result<()>;

    /// Create a new branch `name` pointing at `commit_hash`. Fails with
    /// `BranchAlreadyExists` if `refs/heads/<name>` already exists.
    async fn create_branch(&self, repo: &Path, name: &str, commit_hash: &str) -> Result<()>;

    /// Return the commit history reachable from `HEAD`, ordered newest-first.
    async fn log(&self, repo: &Path, options: LogOptions) -> Result<Vec<CommitWithRefs>>;

    /// Resolve a human-readable `revision` (branch name, tag, `HEAD`, short
    /// hash, …) to its full object ID.
    async fn rev_parse(&self, repo: &Path, revision: &str) -> Result<String>;

    /// Push refs to a remote repository.
    async fn push(&self, repo: &Path, options: RemoteOptions) -> Result<()>;

    /// Fetch from a remote and merge into the current branch.
    async fn pull(&self, repo: &Path, options: RemoteOptions) -> Result<()>;

    /// Download objects and refs from a remote without merging.
    async fn fetch(&self, repo: &Path, options: RemoteOptions) -> Result<()>;

    /// Return the name of the current branch (or the commit hash when HEAD is detached).
    async fn get_current_branch(&self, repo: &Path) -> Result<String>;

    /// Resolve HEAD to the current commit id (hash or "0" for initial state).
    async fn get_current_commit_id(&self, repo: &Path) -> Result<String>;

    /// `Some(commit)` when HEAD is detached (points at a raw commit hash rather
    /// than a branch), else `None`. Unambiguous — unlike [`Repository::get_current_branch`],
    /// a branch whose name happens to be 64 hex chars is not misreported as detached.
    ///
    /// The default returns `None` (attached). This powers only the *early* refusal in
    /// the commit use case — an optimization that avoids pausing/snapshotting the
    /// database for a commit that would be rejected anyway. It is NOT the guarantee:
    /// refusing a detached-HEAD commit is enforced by each `commit` implementation
    /// itself. An implementation whose HEAD can detach should override this so the
    /// early refusal fires; even if it does not, its `commit` must still refuse.
    async fn detached_head_commit(&self, _repo: &Path) -> Result<Option<String>> {
        Ok(None)
    }

    /// Return the runtime config stored in the repo config, if present.
    async fn get_runtime_config(&self, repo: &Path) -> Result<Option<RuntimeConfig>>;

    /// Return the mount point stored in the repo config, if present.
    async fn get_mount_point(&self, repo: &Path) -> Result<Option<String>>;

    /// Return the environment config (database provider / version) from the repo config, if present.
    async fn get_environment_config(&self, repo: &Path) -> Result<Option<EnvironmentConfig>>;

    /// Return the user config (author name / email) from the repo config, if present.
    async fn get_user_config(&self, repo: &Path) -> Result<Option<UserConfig>>;

    /// Create `.gfs/snapshots/<2>/<62>/` for the given `hash` and return its path.
    ///
    /// The two-char prefix subdirectory is created if it does not exist. The
    /// returned path is the destination the storage adapter should copy into.
    async fn ensure_snapshot_path(&self, repo: &Path, hash: &str) -> Result<PathBuf>;
}
