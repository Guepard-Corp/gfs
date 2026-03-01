use crate::model::errors::CommitError;
use crate::model::layout::{GFS_DIR, HEAD_FILE, HEADS_DIR, REFS_DIR};
use std::fs;
use std::path::Path;

pub fn get_head(working_dir: &Path) -> Result<String, CommitError> {
    let head_path = working_dir.join(GFS_DIR).join(HEAD_FILE);
    let head = fs::read_to_string(head_path)?;
    let trimmed_head = head.trim();

    // Check if it's a SHA-256 commit hash (64-character hex string)
    let is_commit_hash =
        trimmed_head.len() == 64 && trimmed_head.chars().all(|c| c.is_ascii_hexdigit());
    if is_commit_hash {
        return Ok(trimmed_head.to_string());
    }

    // If not a commit hash, try to extract branch name from ref format
    let ref_prefix = format!("ref: {}/{}/", REFS_DIR, HEADS_DIR);
    if let Some(branch_name) = trimmed_head.strip_prefix(&ref_prefix) {
        Ok(branch_name.trim().to_string())
    } else {
        // If it's neither a commit hash nor a ref, return as is
        Ok(trimmed_head.to_string())
    }
}

pub fn get_head_commit_hash(working_dir: &Path) -> Result<Option<String>, CommitError> {
    let head = get_head(working_dir)?;
    let commit_path = working_dir
        .join(GFS_DIR)
        .join(REFS_DIR)
        .join(HEADS_DIR)
        .join(head);
    let commit = fs::read_to_string(commit_path)?;
    let trimmed_commit = commit.trim();
    if trimmed_commit.len() == 64 && trimmed_commit.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(Some(trimmed_commit.to_string()))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
pub fn set_head(working_dir: &Path, reference: &str) -> Result<(), CommitError> {
    let head_path = working_dir.join(GFS_DIR).join(HEAD_FILE);
    let trimmed_ref = reference.trim();

    // Check if the input is a SHA-256 commit hash (64-character hex string)
    let is_commit_hash =
        trimmed_ref.len() == 64 && trimmed_ref.chars().all(|c| c.is_ascii_hexdigit());

    let head_content = if is_commit_hash {
        trimmed_ref.to_string()
    } else {
        format!("ref: {}/{}/{}", REFS_DIR, HEADS_DIR, trimmed_ref)
    };

    fs::write(head_path, head_content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_get_head_with_branch() {
        let temp_dir = TempDir::new().unwrap();
        let working_dir = temp_dir.path().to_path_buf();

        // Create .gfs directory
        fs::create_dir_all(working_dir.join(GFS_DIR)).unwrap();

        // Write HEAD file with branch reference
        fs::write(
            working_dir.join(GFS_DIR).join(HEAD_FILE),
            format!(
                "ref: {}/{}/{}",
                REFS_DIR,
                HEADS_DIR,
                crate::model::layout::MAIN_BRANCH
            ),
        )
        .unwrap();

        let result = get_head(&working_dir).unwrap();
        assert_eq!(result, "main");
    }

    #[test]
    fn test_get_head_with_commit_hash() {
        let temp_dir = TempDir::new().unwrap();
        let working_dir = temp_dir.path().to_path_buf();

        // Create .gfs directory
        fs::create_dir_all(working_dir.join(GFS_DIR)).unwrap();

        // Write HEAD file with commit hash
        let commit_hash = "0b51b9238d8f2e2150472622668d672096bf506f3eb0372f77f0c9aabab8266c";
        fs::write(working_dir.join(GFS_DIR).join(HEAD_FILE), commit_hash).unwrap();

        let result = get_head(&working_dir).unwrap();
        assert_eq!(result, commit_hash);
    }

    #[test]
    fn test_set_head_with_branch() {
        let temp_dir = TempDir::new().unwrap();
        let working_dir = temp_dir.path().to_path_buf();

        // Create .gfs directory
        fs::create_dir_all(working_dir.join(GFS_DIR)).unwrap();

        // Set HEAD to a branch
        set_head(&working_dir, "feature-branch").unwrap();

        let content = fs::read_to_string(working_dir.join(GFS_DIR).join(HEAD_FILE)).unwrap();
        assert_eq!(
            content,
            format!("ref: {}/{}/feature-branch", REFS_DIR, HEADS_DIR)
        );
    }

    #[test]
    fn test_set_head_with_commit_hash() {
        let temp_dir = TempDir::new().unwrap();
        let working_dir = temp_dir.path().to_path_buf();

        // Create .gfs directory
        fs::create_dir_all(working_dir.join(GFS_DIR)).unwrap();

        // Set HEAD to a commit hash
        let commit_hash = "0b51b9238d8f2e2150472622668d672096bf506f3eb0372f77f0c9aabab8266c";
        set_head(&working_dir, commit_hash).unwrap();

        let content = fs::read_to_string(working_dir.join(GFS_DIR).join(HEAD_FILE)).unwrap();
        assert_eq!(content, commit_hash);
    }

    #[test]
    fn test_get_head_neither_ref_nor_hash() {
        let temp_dir = TempDir::new().unwrap();
        let working_dir = temp_dir.path().to_path_buf();
        fs::create_dir_all(working_dir.join(GFS_DIR)).unwrap();
        fs::write(working_dir.join(GFS_DIR).join(HEAD_FILE), "ref: refs/heads").unwrap();
        let result = get_head(&working_dir).unwrap();
        assert_eq!(result, "ref: refs/heads");
    }

    #[test]
    fn test_get_head_commit_hash_returns_none_for_non_hash() {
        let temp_dir = TempDir::new().unwrap();
        let working_dir = temp_dir.path().to_path_buf();
        fs::create_dir_all(working_dir.join(GFS_DIR)).unwrap();
        fs::write(
            working_dir.join(GFS_DIR).join(HEAD_FILE),
            format!("ref: {}/{}/main", REFS_DIR, HEADS_DIR),
        )
        .unwrap();
        fs::create_dir_all(working_dir.join(GFS_DIR).join(REFS_DIR).join(HEADS_DIR)).unwrap();
        fs::write(
            working_dir
                .join(GFS_DIR)
                .join(REFS_DIR)
                .join(HEADS_DIR)
                .join("main"),
            "0",
        )
        .unwrap();
        let result = get_head_commit_hash(&working_dir).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_get_head_error() {
        let temp_dir = TempDir::new().unwrap();
        let working_dir = temp_dir.path().to_path_buf();

        // Don't create .gfs directory to test error case
        let result = get_head(&working_dir);
        assert!(result.is_err());
    }

    #[test]
    fn test_set_head_error() {
        let temp_dir = TempDir::new().unwrap();
        let working_dir = temp_dir.path().to_path_buf();

        // Create .gfs directory but make it read-only to test error case
        fs::create_dir_all(working_dir.join(GFS_DIR)).unwrap();
        let head_path = working_dir.join(GFS_DIR).join(HEAD_FILE);
        fs::write(&head_path, "").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&head_path, fs::Permissions::from_mode(0o444)).unwrap();
        }

        let result = set_head(&working_dir, "main");
        assert!(result.is_err());
    }
}
