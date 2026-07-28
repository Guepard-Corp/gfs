use std::path::{Path, PathBuf};

use anyhow::Result;
use gfs_domain::model::layout::{GFS_DIR, HEADS_DIR, REFS_DIR};

/// Returns the current working directory as the default repo path.
pub fn get_repo_dir() -> PathBuf {
    std::env::current_dir().expect("current directory not available")
}

/// Collect branch tips: Vec<(branch_name, commit_hash)>.
///
/// When `missing_ok` is true and the refs directory doesn't exist, returns an empty list.
pub fn list_branch_tips(repo_path: &Path, missing_ok: bool) -> Result<Vec<(String, String)>> {
    let refs_dir = repo_path.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR);
    if !refs_dir.exists() {
        if missing_ok {
            return Ok(Vec::new());
        }
        anyhow::bail!("not a GFS repository (no refs directory)");
    }
    collect_refs(&refs_dir, "")
}

fn collect_refs(dir: &Path, prefix: &str) -> Result<Vec<(String, String)>> {
    let mut result = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let branch_name = if prefix.is_empty() {
            name
        } else {
            format!("{}/{}", prefix, name)
        };
        if path.is_file() {
            let tip = std::fs::read_to_string(&path)?.trim().to_string();
            result.push((branch_name, tip));
        } else if path.is_dir() {
            result.extend(collect_refs(&path, &branch_name)?);
        }
    }
    Ok(result)
}

/// Return a path relative to the repo root (e.g. `.gfs/workspaces/dev/.../data`).
/// If relativization fails, returns the original path string.
pub fn relativize_to_repo(repo_path: &Path, full_path: &str) -> String {
    let repo = match repo_path.canonicalize() {
        Ok(p) => p,
        Err(_) => repo_path.to_path_buf(),
    };
    let full = PathBuf::from(full_path);
    let full_canon = match full.canonicalize() {
        Ok(p) => p,
        Err(_) => full,
    };
    full_canon
        .strip_prefix(&repo)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| full_path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_ref(root: &Path, branch: &str, hash: &str) {
        let p = root
            .join(GFS_DIR)
            .join(REFS_DIR)
            .join(HEADS_DIR)
            .join(branch);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, format!("{hash}\n")).unwrap();
    }

    #[test]
    fn list_branch_tips_collects_flat_and_nested_refs() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let (a, b, c) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        write_ref(root, "main", &a);
        write_ref(root, "feature", &b);
        write_ref(root, "team/alpha", &c); // nested ref → "team/alpha"

        let mut tips = list_branch_tips(root, false).unwrap();
        tips.sort();
        assert_eq!(
            tips,
            vec![
                ("feature".to_string(), b),
                ("main".to_string(), a),
                ("team/alpha".to_string(), c),
            ]
        );
    }

    #[test]
    fn list_branch_tips_honors_missing_ok() {
        let tmp = tempdir().unwrap();
        // No refs dir: missing_ok=true → empty; missing_ok=false → error.
        assert!(list_branch_tips(tmp.path(), true).unwrap().is_empty());
        assert!(list_branch_tips(tmp.path(), false).is_err());
    }

    #[test]
    fn relativize_to_repo_strips_repo_prefix() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let sub = root.join(".gfs/workspaces/main/0/data");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(
            relativize_to_repo(root, sub.to_str().unwrap()),
            ".gfs/workspaces/main/0/data"
        );
    }

    #[test]
    fn relativize_to_repo_returns_original_when_outside_repo() {
        let tmp = tempdir().unwrap();
        // A path outside the repo (and non-existent) is returned unchanged.
        assert_eq!(
            relativize_to_repo(tmp.path(), "/some/other/path"),
            "/some/other/path"
        );
    }
}
