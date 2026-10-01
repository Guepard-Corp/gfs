pub const GFS_DIR: &str = ".gfs";
pub const HEAD_FILE: &str = "HEAD";
/// Stores the absolute path of the currently active workspace data directory.
///
/// Written by `gfs init` (pointing to `workspaces/main/0/data`) and updated
/// by `gfs checkout` when switching branches or commits.  The commit use case
/// reads this file so it always snapshots the directory where the database is
/// actually running, regardless of how many commits have been made since init.
pub const WORKSPACE_FILE: &str = "WORKSPACE";
pub const REFS_DIR: &str = "refs";
pub const HEADS_DIR: &str = "heads";
/// Holds branch refs that `gfs branch -d` moved aside instead of unlinking, so a
/// deleted branch can be restored by name. Entries live at
/// `refs/deleted/<unix_millis>/<branch path>`; see `repo_layout::soft_delete_branch_ref`
/// for why the timestamp is the outer segment.
pub const DELETED_REFS_DIR: &str = "deleted";
pub const MAIN_BRANCH: &str = "main";
pub const CONFIG_FILE: &str = "config.toml";
pub const OBJECTS_DIR: &str = "objects";
pub const SNAPSHOTS_DIR: &str = "snapshots";
pub const WORKSPACES_DIR: &str = "workspaces";
pub const WORKSPACE_DATA_DIR: &str = "data";
pub const OPERATIONS_DIR: &str = "operations";

/// Length of commit hash used in workspace directory paths on disk (e.g. `workspaces/detached/<short>/data`).
/// Full hash remains in refs and HEAD; only the path segment is shortened to keep paths readable.
pub const SHORT_COMMIT_ID_LEN: usize = 12;

/// Minimum length for short commit hash (git-compatible)
pub const MIN_SHORT_HASH_LEN: usize = 4;

/// Default display length for short hashes (git-compatible)
pub const DEFAULT_SHORT_HASH_LEN: usize = 7;

/// Directory segment for a branch's single persistent workspace (e.g. `workspaces/main/0/data`).
/// One workspace per branch so the database state in that directory is preserved across checkouts.
pub const BRANCH_WORKSPACE_SEGMENT: &str = "0";
