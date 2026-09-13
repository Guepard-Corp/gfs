//! Which volume holds which branch's live data.
//!
//! On a runtime where a branch's database lives in a directory, this mapping is
//! implicit: the directory path is derived from the branch name and the
//! directory simply persists. On Kubernetes it cannot be implicit, because the
//! volume is a PVC — a named cluster object that a pod references by
//! `claimName` — and nothing about a PVC records the branch it belongs to.
//!
//! Without a record, the only way to put a different branch's data in front of
//! the database is to destroy the one PVC and clone a new one into the same
//! name. That is what checkout used to do, and it is why switching branches
//! discarded everything not yet committed: the volume holding it was deleted.
//!
//! This file is that record. It is deliberately generic over the volume name —
//! nothing here knows what a PVC is — so the same mapping can carry a directory
//! path, a ZFS dataset, or anything else a future runtime calls a volume.
//!
//! It lives in the SHARED `.gfs` directory. Volumes belong to the repository, so
//! every reader of a repository must see
//! the same answer to "which volume is branch X's", or they would each clone
//! their own and silently diverge.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::errors::RepoError;
use crate::model::layout::GFS_DIR;

/// File name inside the shared `.gfs` directory.
const FILE_NAME: &str = "branch-volumes.toml";

/// The persisted branch → volume mapping.
///
/// Serialised as a single `[volumes]` table. Branch names are used verbatim as
/// keys — TOML quotes them, so `feat/thing` round-trips without mangling, which
/// matters because the mangled form is not reversible.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchVolumes {
    #[serde(default)]
    volumes: BTreeMap<String, String>,
}

impl BranchVolumes {
    /// Path of the mapping file for a repository.
    pub fn path_for(repo_path: &Path) -> Result<PathBuf, RepoError> {
        // Fallible on purpose. The join itself cannot fail, but resolving a
        // repository's `.gfs` directory is the kind of thing that acquires a
        // failure mode later, and returning a `Result` now keeps that from
        // becoming a signature change rippling through every caller.
        Ok(repo_path.join(GFS_DIR).join(FILE_NAME))
    }

    /// Load the mapping, or an empty one when the file does not exist yet.
    ///
    /// A file that exists but does not parse is a hard error rather than a
    /// silent empty map: treating it as empty would re-clone every branch and
    /// orphan every volume it named, which is exactly the data loss this record
    /// exists to prevent.
    pub fn load(repo_path: &Path) -> Result<Self, RepoError> {
        let path = Self::path_for(repo_path)?;
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(RepoError::IoError(e)),
        };
        toml::from_str(&raw)
            .map_err(|e| RepoError::InvalidConfig(format!("{}: {e}", path.display())))
    }

    /// The volume recorded for `branch`, if any.
    pub fn get(&self, branch: &str) -> Option<&str> {
        self.volumes.get(branch.trim()).map(String::as_str)
    }

    /// Record `branch -> volume`, replacing any previous entry.
    ///
    /// Empty names are ignored: an empty branch or volume key would later look
    /// like a valid record and send a checkout at a volume that cannot exist.
    pub fn set(&mut self, branch: &str, volume: &str) {
        let branch = branch.trim();
        let volume = volume.trim();
        if branch.is_empty() || volume.is_empty() {
            return;
        }
        self.volumes.insert(branch.to_string(), volume.to_string());
    }

    /// Record `branch -> volume` only if `branch` has no entry yet.
    ///
    /// This is the adoption case: a repository that predates this record already
    /// has a volume in use, and it belongs to whichever branch is checked out at
    /// the moment we first look. Overwriting an existing entry here would be
    /// wrong — the existing one was written deliberately.
    ///
    /// Returns whether anything was recorded.
    pub fn adopt(&mut self, branch: &str, volume: &str) -> bool {
        if self.get(branch).is_some() {
            return false;
        }
        let before = self.volumes.len();
        self.set(branch, volume);
        self.volumes.len() != before
    }

    /// Forget `branch`'s volume. Returns the volume that was recorded.
    pub fn remove(&mut self, branch: &str) -> Option<String> {
        self.volumes.remove(branch.trim())
    }

    /// Every recorded volume name, deduplicated, in a stable order.
    ///
    /// Two branches can legitimately name the same volume (a branch created and
    /// never switched away from shares the parent's), so callers reclaiming
    /// volumes must not assume one name per branch.
    pub fn volume_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.volumes.values().cloned().collect();
        names.sort();
        names.dedup();
        names
    }

    /// Branches recorded, in a stable order.
    pub fn branches(&self) -> Vec<&str> {
        self.volumes.keys().map(String::as_str).collect()
    }

    /// Whether nothing is recorded.
    pub fn is_empty(&self) -> bool {
        self.volumes.is_empty()
    }

    /// Write the mapping out, atomically.
    ///
    /// Rename-over-temp rather than truncate-and-write: this file decides which
    /// volume a checkout mounts, and a half-written one read back by the next
    /// command would fail to parse and refuse the repository.
    pub fn save(&self, repo_path: &Path) -> Result<(), RepoError> {
        let path = Self::path_for(repo_path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(RepoError::IoError)?;
        }
        let body = toml::to_string_pretty(self)
            .map_err(|e| RepoError::InvalidConfig(format!("serialize branch volumes: {e}")))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body).map_err(RepoError::IoError)?;
        std::fs::rename(&tmp, &path).map_err(RepoError::IoError)
    }
}

/// Load, mutate, save. Convenience for the common single-edit case.
pub fn update<F>(repo_path: &Path, edit: F) -> Result<BranchVolumes, RepoError>
where
    F: FnOnce(&mut BranchVolumes),
{
    let mut volumes = BranchVolumes::load(repo_path)?;
    edit(&mut volumes);
    volumes.save(repo_path)?;
    Ok(volumes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repo whose `.gfs` is a real directory.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".gfs")).unwrap();
        dir
    }

    #[test]
    fn missing_file_loads_as_empty_not_an_error() {
        let dir = repo();
        let volumes = BranchVolumes::load(dir.path()).unwrap();
        assert!(volumes.is_empty());
    }

    #[test]
    fn a_branch_name_with_a_slash_round_trips() {
        let dir = repo();
        let mut volumes = BranchVolumes::load(dir.path()).unwrap();
        // The reason keys are quoted TOML rather than a sanitized form: the
        // sanitized form is not reversible, so `feat/x` and `feat-x` would read
        // back as the same branch and share one volume.
        volumes.set("feat/thing", "pvc-a");
        volumes.set("feat-thing", "pvc-b");
        volumes.save(dir.path()).unwrap();

        let reloaded = BranchVolumes::load(dir.path()).unwrap();
        assert_eq!(reloaded.get("feat/thing"), Some("pvc-a"));
        assert_eq!(reloaded.get("feat-thing"), Some("pvc-b"));
        assert_eq!(reloaded, volumes);
    }

    #[test]
    fn adopt_does_not_overwrite_a_deliberate_record() {
        let mut volumes = BranchVolumes::default();
        assert!(volumes.adopt("main", "legacy-data"));
        // Second adoption must lose: the first is the volume actually in use.
        assert!(!volumes.adopt("main", "something-else"));
        assert_eq!(volumes.get("main"), Some("legacy-data"));
        // An explicit set still wins — that is a deliberate rebind.
        volumes.set("main", "something-else");
        assert_eq!(volumes.get("main"), Some("something-else"));
    }

    #[test]
    fn empty_names_are_not_recorded() {
        let mut volumes = BranchVolumes::default();
        volumes.set("", "pvc-a");
        volumes.set("main", "   ");
        assert!(
            volumes.is_empty(),
            "an empty key would look like a valid record"
        );
    }

    #[test]
    fn volume_names_deduplicates_shared_volumes() {
        let mut volumes = BranchVolumes::default();
        volumes.set("main", "pvc-a");
        // A freshly created branch has not diverged yet and shares the volume.
        volumes.set("feat", "pvc-a");
        volumes.set("other", "pvc-b");
        assert_eq!(volumes.volume_names(), vec!["pvc-a", "pvc-b"]);
    }

    #[test]
    fn a_corrupt_file_is_refused_rather_than_read_as_empty() {
        let dir = repo();
        std::fs::write(
            BranchVolumes::path_for(dir.path()).unwrap(),
            "this is not toml {",
        )
        .unwrap();
        // Reading it as empty would re-clone every branch and orphan every
        // volume it named — the exact loss this record prevents.
        assert!(BranchVolumes::load(dir.path()).is_err());
    }

    #[test]
    fn update_loads_mutates_and_persists() {
        let dir = repo();
        update(dir.path(), |v| v.set("main", "pvc-a")).unwrap();
        assert_eq!(
            BranchVolumes::load(dir.path()).unwrap().get("main"),
            Some("pvc-a")
        );
    }

    #[test]
    fn remove_forgets_the_branch() {
        let mut volumes = BranchVolumes::default();
        volumes.set("main", "pvc-a");
        assert_eq!(volumes.remove("main"), Some("pvc-a".to_string()));
        assert_eq!(volumes.get("main"), None);
    }
}
