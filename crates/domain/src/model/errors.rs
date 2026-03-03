use bincode;
use serde_json;
use std::io;
use std::path::PathBuf;

/// General domain error type for operations not specific to commits or repositories.
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("Resource not found: {0}")]
    NotFound(String),

    #[error("Resource already exists: {0}")]
    AlreadyExists(String),

    #[error("Invalid input: {0}")]
    InvalidInput(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("Failed to create commit: {0}")]
    CreationError(String),

    #[error("Failed to write commit object: {0}")]
    IOError(#[from] std::io::Error),

    #[error("Failed to serialize commit: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("Failed to decode commit: {0}")]
    DecodeError(#[from] bincode::error::DecodeError),

    #[error("Failed to encode commit: {0}")]
    EncodeError(#[from] bincode::error::EncodeError),
    /*
    #[error("Invalid commit message: {0}")]
    InvalidMessage(String),

    #[error("Failed to generate commit hash: {0}")]
    HashError(String),

    #[error("Failed to update branch reference: {0}")]
    BranchUpdateError(String),
     */
}

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("No .gfs repository found in {0} or any parent directory")]
    NoRepoFound(PathBuf),
    #[error("IO error while searching for repository: {0}")]
    IoError(#[from] io::Error),
    #[error("Invalid repository layout: {0}")]
    InvalidLayout(String),
    #[error("Missing required file: {0}")]
    MissingFile(PathBuf),
    #[error("Invalid config.toml: {0}")]
    InvalidConfig(String),
    #[error("revision not found: '{0}'")]
    RevisionNotFound(String),
    #[error("short hash '{prefix}' is ambiguous\nPossible matches:\n{}", format_matches(.matches))]
    AmbiguousShortHash {
        prefix: String,
        matches: Vec<String>,
    },
}

fn format_matches(matches: &[String]) -> String {
    matches
        .iter()
        .take(10) // Limit to 10 for display
        .map(|h| format!("  {}", h))
        .collect::<Vec<_>>()
        .join("\n")
}

impl RepoError {
    pub fn no_repo_found(path: PathBuf) -> Self {
        RepoError::NoRepoFound(path)
    }

    pub fn invalid_layout(msg: String) -> Self {
        RepoError::InvalidLayout(msg)
    }

    pub fn missing_file(path: PathBuf) -> Self {
        RepoError::MissingFile(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_error_no_repo_found_display() {
        let err = RepoError::no_repo_found(PathBuf::from("/tmp"));
        assert!(err.to_string().contains(".gfs"));
        assert!(err.to_string().contains("/tmp"));
    }

    #[test]
    fn repo_error_invalid_layout_display() {
        let err = RepoError::invalid_layout("bad layout".into());
        assert!(err.to_string().contains("Invalid"));
        assert!(err.to_string().contains("bad layout"));
    }

    #[test]
    fn repo_error_missing_file_display() {
        let err = RepoError::MissingFile(PathBuf::from(".gfs/HEAD"));
        assert!(err.to_string().contains("Missing"));
        assert!(err.to_string().contains("HEAD"));
    }

    #[test]
    fn repo_error_revision_not_found_display() {
        let err = RepoError::RevisionNotFound("main".into());
        assert!(err.to_string().contains("revision"));
        assert!(err.to_string().contains("main"));
    }

    #[test]
    fn commit_error_creation_error_display() {
        let err = CommitError::CreationError("test".into());
        assert!(err.to_string().contains("create commit"));
        assert!(err.to_string().contains("test"));
    }
}
