//! Create a new, independent database from one snapshot of an existing one.
//!
//! The source is another gfs repository. Its chosen snapshot is copied into the
//! new repository's workspace BEFORE the new database first starts, so the
//! database never runs empty and is never swapped underneath. The new
//! repository records no history of the source: it starts with that data and
//! nothing else, and its first commit is the caller's to make.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

use crate::model::config::{GfsConfig, RepoCredentials};
use crate::ports::database_provider::DatabaseProviderRegistry;
use crate::ports::repository::{Repository, RepositoryError};
use crate::repo_utils::repo_layout;
use crate::usecases::repository::init_repo_usecase::{
    DatabaseCredentials, InitRepoError, InitRepositoryUseCase, ProvisionSpec,
};

#[derive(Debug, Error)]
pub enum CloneFromSnapshotError {
    #[error("'{0}' is not a gfs repository")]
    SourceNotARepository(String),

    #[error("'{0}' has no database configured, so there is nothing to clone")]
    SourceHasNoDatabase(String),

    #[error("'{0}' has no snapshots yet; commit one first")]
    SourceHasNoSnapshots(String),

    #[error("snapshot or branch '{0}' not found in the source")]
    SnapshotNotFound(String),

    #[error(
        "snapshot {snapshot} is recorded but its data is missing at '{path}'; \
         if the source was copied, copy .gfs/snapshots as well"
    )]
    SnapshotDataMissing { snapshot: String, path: String },

    #[error("'{0}' is already a gfs repository")]
    TargetExists(String),

    #[error("repository error: {0}")]
    Repository(#[from] RepositoryError),

    #[error(transparent)]
    Init(#[from] InitRepoError),
}

/// What to clone and where.
#[derive(Debug, Clone)]
pub struct CloneFromSnapshotRequest {
    pub source: PathBuf,
    pub target: PathBuf,
    /// Snapshot id (full or short), branch, or anything `checkout` accepts.
    /// `None` = the source's latest snapshot (HEAD).
    pub revision: Option<String>,
    pub database_port: Option<u16>,
    /// A source started from a custom image (an extension the default image
    /// lacks) needs the same image; gfs does not record which one it was.
    pub image: Option<String>,
    /// Credentials to start with. `None` = the source's saved credentials,
    /// which are the ones the copied data was initialised with.
    pub credentials: Option<DatabaseCredentials>,
    pub provision: ProvisionSpec,
}

/// What was cloned.
#[derive(Debug, Clone)]
pub struct ClonedDatabase {
    /// Full commit hash in the source that the data comes from.
    pub source_commit: String,
    pub database_provider: String,
    pub database_version: String,
}

pub struct CloneFromSnapshotUseCase<R: DatabaseProviderRegistry> {
    repository: Arc<dyn Repository>,
    init: InitRepositoryUseCase<R>,
}

impl<R: DatabaseProviderRegistry> CloneFromSnapshotUseCase<R> {
    pub fn new(repository: Arc<dyn Repository>, init: InitRepositoryUseCase<R>) -> Self {
        Self { repository, init }
    }

    /// Everything that can be refused is checked before the target is created,
    /// so a refusal leaves nothing behind.
    pub async fn run(
        &self,
        request: CloneFromSnapshotRequest,
    ) -> Result<ClonedDatabase, CloneFromSnapshotError> {
        let source = request.source.as_path();
        if !source.join(".gfs").is_dir() {
            return Err(CloneFromSnapshotError::SourceNotARepository(display(
                source,
            )));
        }
        if request.target.join(".gfs").exists() {
            return Err(CloneFromSnapshotError::TargetExists(display(
                &request.target,
            )));
        }

        let environment = repo_layout::get_environment_config(source)
            .ok()
            .flatten()
            .filter(|env| !env.database_provider.trim().is_empty())
            .ok_or_else(|| CloneFromSnapshotError::SourceHasNoDatabase(display(source)))?;

        let revision = request.revision.clone().unwrap_or_else(|| "HEAD".into());
        let commit_hash = self
            .repository
            .rev_parse(source, &revision)
            .await
            .map_err(|_| CloneFromSnapshotError::SnapshotNotFound(revision.clone()))?;
        if commit_hash == "0" {
            return Err(CloneFromSnapshotError::SourceHasNoSnapshots(display(
                source,
            )));
        }
        let commit = repo_layout::get_commit_from_hash(source, &commit_hash)
            .map_err(|_| CloneFromSnapshotError::SnapshotNotFound(revision.clone()))?;
        let snapshot_dir = repo_layout::snapshot_path(source, &commit.snapshot_hash);
        if !snapshot_dir.is_dir() {
            return Err(CloneFromSnapshotError::SnapshotDataMissing {
                snapshot: short(&commit_hash),
                path: display(&snapshot_dir),
            });
        }

        let credentials = request.credentials.clone().unwrap_or_else(|| {
            let saved = RepoCredentials::load(source);
            DatabaseCredentials {
                user: saved.user,
                password: saved.password,
                name: saved.name,
            }
        });

        let target = request.target.as_path();
        self.init
            .create_repository(
                target,
                None,
                Some(&environment.database_provider),
                Some(&environment.database_version),
            )
            .await?;
        copy_compute_params(source, target);
        self.repository
            .seed_head_workspace(target, &snapshot_dir)
            .await?;
        self.init
            .start_database(
                target,
                environment.database_provider.clone(),
                Some(environment.database_version.clone()),
                request.database_port,
                credentials,
                environment.display_name.clone(),
                request.image.clone(),
                request.provision.clone(),
            )
            .await?;

        Ok(ClonedDatabase {
            source_commit: commit_hash,
            database_provider: environment.database_provider,
            database_version: environment.database_version,
        })
    }
}

/// The source's database settings (`[compute].params`) carry over: the copied
/// data was written under them. Best effort, like the other config records init
/// keeps -- a clone without them still starts, on the provider's defaults.
fn copy_compute_params(source: &Path, target: &Path) {
    let params = GfsConfig::load_compute_params(source);
    if params.is_empty() {
        return;
    }
    if let Ok(mut config) = GfsConfig::load(target) {
        config.compute.get_or_insert_with(Default::default).params = params;
        let _ = config.save(target);
    }
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

fn short(hash: &str) -> String {
    hash[..7.min(hash.len())].to_string()
}
