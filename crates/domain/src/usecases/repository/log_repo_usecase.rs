use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;

use crate::model::commit::CommitWithRefs;
use crate::ports::repository::{LogOptions, Repository, RepositoryError};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum LogRepoError {
    #[error("repository error: {0}")]
    Repository(#[from] RepositoryError),
}

// ---------------------------------------------------------------------------
// Use case
// ---------------------------------------------------------------------------

/// Use case for displaying commit history reachable from HEAD (or a given revision).
///
/// Delegates to the [`Repository::log`] port. No direct I/O; all I/O is in the adapter.
pub struct LogRepoUseCase {
    repository: Arc<dyn Repository>,
}

impl LogRepoUseCase {
    pub fn new(repository: Arc<dyn Repository>) -> Self {
        Self { repository }
    }

    /// Return the commit history at `path` according to `options`.
    pub async fn run(
        &self,
        path: PathBuf,
        options: LogOptions,
    ) -> std::result::Result<Vec<CommitWithRefs>, LogRepoError> {
        self.repository
            .log(&path, options)
            .await
            .map_err(LogRepoError::from)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::Arc;

    use async_trait::async_trait;

    use crate::model::commit::CommitWithRefs;
    use crate::model::config::{EnvironmentConfig, RuntimeConfig};
    use crate::ports::repository::{LogOptions, Repository};

    struct MockRepository {
        commits: Vec<CommitWithRefs>,
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
            _: EnvironmentConfig,
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
            _: crate::model::commit::NewCommit,
        ) -> crate::ports::repository::Result<String> {
            Ok(String::new())
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
            Ok(self.commits.clone())
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
            _: crate::ports::repository::RemoteOptions,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn pull(
            &self,
            _: &std::path::Path,
            _: crate::ports::repository::RemoteOptions,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn fetch(
            &self,
            _: &std::path::Path,
            _: crate::ports::repository::RemoteOptions,
        ) -> crate::ports::repository::Result<()> {
            Ok(())
        }
        async fn get_current_branch(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<String> {
            Ok("main".into())
        }
        async fn get_current_commit_id(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<String> {
            Ok("0".into())
        }
        async fn get_runtime_config(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<RuntimeConfig>> {
            Ok(None)
        }
        async fn get_mount_point(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<String>> {
            Ok(None)
        }
        async fn get_environment_config(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<EnvironmentConfig>> {
            Ok(None)
        }
        async fn get_user_config(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<Option<crate::model::config::UserConfig>> {
            Ok(None)
        }
        async fn ensure_snapshot_path(
            &self,
            _: &std::path::Path,
            _: &str,
        ) -> crate::ports::repository::Result<PathBuf> {
            Ok(PathBuf::from("/tmp/snap"))
        }
        async fn get_active_workspace_data_dir(
            &self,
            _: &std::path::Path,
        ) -> crate::ports::repository::Result<PathBuf> {
            Ok(PathBuf::from("/workspace/data"))
        }
    }

    #[tokio::test]
    async fn log_returns_commits_from_repository() {
        let new_commit = crate::model::commit::NewCommit::new(
            "commit 1".into(),
            "user".into(),
            None,
            "user".into(),
            None,
            "hash1".into(),
            None,
        );
        let commit = crate::model::commit::Commit::from_new_commit(&new_commit, "abc123".into());
        let repo = MockRepository {
            commits: vec![CommitWithRefs {
                commit,
                refs: vec!["refs/heads/main".into()],
            }],
        };
        let usecase = LogRepoUseCase::new(Arc::new(repo));
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(dir.path().to_path_buf(), LogOptions::default())
            .await;
        assert!(result.is_ok());
        let commits = result.unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].commit.hash.as_deref(), Some("abc123"));
    }

    #[tokio::test]
    async fn log_with_options() {
        let repo = MockRepository { commits: vec![] };
        let usecase = LogRepoUseCase::new(Arc::new(repo));
        let dir = tempfile::tempdir().unwrap();
        let result = usecase
            .run(
                dir.path().to_path_buf(),
                LogOptions {
                    from: Some("HEAD".into()),
                    until: None,
                    limit: Some(10),
                },
            )
            .await;
        assert!(result.is_ok());
    }
}
