use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Metadata for one file in the snapshot data directory (flattened file list in a commit).
/// Attributes captured from the same stat that yields size and mode.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Default)]
pub struct FileAttrs {
    /// Modification time in nanoseconds since the Unix epoch.
    ///
    /// This answers "written since the snapshot", not "differs from it": a file
    /// rewritten back to identical bytes still reports changed. For a gate that
    /// refuses rather than overwrites, that is the safe direction.
    #[serde(default)]
    pub mtime_ns: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Path relative to the workspace `data/` folder.
    pub relative_path: String,
    /// File size in bytes.
    pub file_size: u64,
    /// Owner (e.g. uid or username); `None` when not available (e.g. on some platforms).
    pub owner: Option<String>,
    /// Group (e.g. gid or group name); `None` when not available.
    pub group: Option<String>,
    /// File mode / permissions (e.g. octal "0600"); `None` when not available.
    pub permissions: Option<String>,
    /// Optional per-file attributes.
    ///
    /// Typed, not `serde_json::Value`: file entries are serialized with bincode,
    /// which encodes a `Value` but cannot decode one (`AnyNotSupported`), so a
    /// populated free-form field produced an object that could never be read
    /// back. `None` is a single zero byte in bincode whatever the inner type is,
    /// so objects written before this field was typed still decode.
    #[serde(default)]
    pub file_attributes: Option<FileAttrs>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Commit {
    // Hash is contained in the object file, not in the commit object. This used only for logging/printing.
    pub hash: Option<String>,
    pub message: String,
    pub timestamp: DateTime<Utc>,

    // DAG
    pub parents: Option<Vec<String>>,

    // Snapshot
    pub snapshot_hash: String,

    // Identification
    pub author: String,
    pub author_email: Option<String>,
    pub author_date: DateTime<Utc>,
    pub committer: String,
    pub committer_email: Option<String>,
    pub committer_date: DateTime<Utc>,

    // Metadata
    pub schema_hash: Option<String>,
    pub database_provider: Option<String>,
    pub database_version: Option<String>,

    // Stats
    pub files_added: Option<usize>,
    pub files_deleted: Option<usize>,
    pub files_modified: Option<usize>,
    pub files_renamed: Option<usize>,
    /// Hash of the binary object containing the file list (stored under `.gfs/objects/<2>/<62>`).
    pub files_ref: Option<String>,
    /// Total number of files in the snapshot (computed when files_ref is set).
    pub files_count: Option<usize>,
    /// Physical (on-disk) size of the snapshot directory in bytes (sum of allocated blocks).
    pub snapshot_size_bytes: Option<u64>,

    pub blocks_added: Option<usize>,
    pub blocks_deleted: Option<usize>,

    pub db_objects_added: Option<usize>,
    pub db_objects_deleted: Option<usize>,
    pub db_objects_modified: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CommitWithRefs {
    pub commit: Commit,
    pub refs: Vec<String>, // List of refs (branches, HEAD) pointing to this commit
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NewCommit {
    pub message: String,
    pub timestamp: DateTime<Utc>,

    // Identification
    pub author: String,
    pub author_email: Option<String>,
    pub author_date: DateTime<Utc>,
    pub committer: String,
    pub committer_email: Option<String>,
    pub committer_date: DateTime<Utc>,

    // Snapshot
    pub snapshot_hash: String,

    // DAG
    pub parents: Option<Vec<String>>,

    // Schema
    pub schema_hash: Option<String>,
}

/// Diff stats derived from the file list: (files_added, files_deleted, files_modified).
/// Modified = same relative_path in both but different file_size or permissions.
pub fn file_entry_diff_stats(
    current: &[FileEntry],
    parent: Option<&[FileEntry]>,
) -> (usize, usize, usize) {
    let parent = match parent {
        Some(p) if !p.is_empty() => p,
        _ => {
            return (current.len(), 0, 0);
        }
    };
    let current_paths: std::collections::HashSet<&str> =
        current.iter().map(|e| e.relative_path.as_str()).collect();
    let parent_by_path: std::collections::HashMap<&str, &FileEntry> = parent
        .iter()
        .map(|e| (e.relative_path.as_str(), e))
        .collect();
    let parent_paths: std::collections::HashSet<&str> = parent_by_path.keys().copied().collect();

    let added = current
        .iter()
        .filter(|e| !parent_paths.contains(e.relative_path.as_str()))
        .count();
    let deleted = parent
        .iter()
        .filter(|e| !current_paths.contains(e.relative_path.as_str()))
        .count();
    let modified = current
        .iter()
        .filter(|e| {
            parent_by_path
                .get(e.relative_path.as_str())
                .is_some_and(|p| p.file_size != e.file_size || p.permissions != e.permissions)
        })
        .count();

    (added, deleted, modified)
}

impl Commit {
    /// Build a persisted [`Commit`] from a [`NewCommit`] and its computed hash.
    ///
    /// Optional metadata (schema, database_*, files_*, blocks_*, db_objects_*) is left as `None`
    /// and can be populated by callers that have richer context.
    pub fn from_new_commit(new: &NewCommit, hash: String) -> Self {
        Self {
            hash: Some(hash),
            message: new.message.clone(),
            timestamp: new.timestamp,
            parents: new.parents.clone(),
            snapshot_hash: new.snapshot_hash.clone(),
            author: new.author.clone(),
            author_email: new.author_email.clone(),
            author_date: new.author_date,
            committer: new.committer.clone(),
            committer_email: new.committer_email.clone(),
            committer_date: new.committer_date,
            schema_hash: new.schema_hash.clone(),
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
        }
    }
}

impl NewCommit {
    pub fn new(
        message: String,
        author: String,
        author_email: Option<String>,
        committer: String,
        committer_email: Option<String>,
        snapshot_hash: String,
        parents: Option<Vec<String>>,
    ) -> Self {
        let now = chrono::Utc::now();

        Self {
            message,
            timestamp: now,
            author,
            author_email,
            author_date: now,
            committer,
            committer_email,
            committer_date: now,
            snapshot_hash,
            parents,
            schema_hash: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FileEntry, file_entry_diff_stats};

    fn entry(path: &str, size: u64, perms: Option<&str>) -> FileEntry {
        FileEntry {
            relative_path: path.into(),
            file_size: size,
            owner: None,
            group: None,
            permissions: perms.map(String::from),
            file_attributes: None,
        }
    }

    #[test]
    fn file_entry_diff_stats_no_parent() {
        let current = vec![entry("a", 1, None), entry("b", 2, None)];
        let (added, deleted, modified) = file_entry_diff_stats(&current, None);
        assert_eq!(added, 2);
        assert_eq!(deleted, 0);
        assert_eq!(modified, 0);
    }

    #[test]
    fn file_entry_diff_stats_empty_parent() {
        let current = vec![entry("a", 1, None)];
        let parent: Vec<FileEntry> = vec![];
        let (added, deleted, modified) = file_entry_diff_stats(&current, Some(&parent));
        assert_eq!(added, 1);
        assert_eq!(deleted, 0);
        assert_eq!(modified, 0);
    }

    #[test]
    fn file_entry_diff_stats_added_deleted_modified() {
        let parent = vec![entry("keep", 10, Some("644")), entry("gone", 5, None)];
        let current = vec![entry("keep", 10, Some("644")), entry("new", 3, None)];
        let (added, deleted, modified) = file_entry_diff_stats(&current, Some(&parent));
        assert_eq!(added, 1, "new is added");
        assert_eq!(deleted, 1, "gone is deleted");
        assert_eq!(modified, 0, "keep unchanged");
    }

    #[test]
    fn file_entry_diff_stats_modified_by_size_or_permissions() {
        let parent = vec![entry("f1", 10, Some("600")), entry("f2", 20, Some("644"))];
        let current = vec![entry("f1", 11, Some("600")), entry("f2", 20, Some("600"))];
        let (added, deleted, modified) = file_entry_diff_stats(&current, Some(&parent));
        assert_eq!(added, 0);
        assert_eq!(deleted, 0);
        assert_eq!(modified, 2);
    }
}
