//! `gfs destroy` — tear down a repository's compute instance and remove its
//! `.gfs` store.
//!
//! On the Docker runtime the per-commit snapshot files under `.gfs` are written
//! by the container's postgres UID with mode `0700`, so a normal host user (in
//! particular on Docker Desktop for macOS, where the bind mount surfaces the
//! container UID) cannot `rm -rf` them. This command removes them the same way
//! GFS wrote them: from inside an ephemeral root container that bind-mounts the
//! repository. On Kubernetes there are no host data files — `remove_instance`
//! reclaims the PVCs and per-commit VolumeSnapshots — so only the on-disk `.gfs`
//! metadata is removed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use gfs_db_providers as containers;
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::model::config::GfsConfig;
use gfs_domain::ports::compute::{Compute, InstanceId};
use gfs_domain::ports::database_provider::{
    DatabaseProviderRegistry, InMemoryDatabaseProviderRegistry,
};
use gfs_domain::ports::repository::Repository;

use super::compute_support::compute_for_repo;
use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, green, yellow};

pub async fn destroy(path: Option<PathBuf>, yes: bool) -> Result<()> {
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let gfs_dir = repo_path.join(".gfs");
    if !gfs_dir.exists() {
        bail!("no GFS repository at {}", repo_path.display());
    }
    let config = GfsConfig::load(&repo_path).map_err(|e| anyhow::anyhow!("{e}"))?;
    let is_k8s = config
        .runtime
        .as_ref()
        .map(|r| r.runtime_provider.trim().eq_ignore_ascii_case("kubernetes"))
        .unwrap_or(false);

    if !yes {
        eprint!(
            "This removes the compute instance and deletes {}. Continue? [y/N] ",
            gfs_dir.display()
        );
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            eprintln!("aborted");
            return Ok(());
        }
    }

    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute = compute_for_repo(&repository, &repo_path).await?;

    // 1. Tear down the compute instance. On k8s this reclaims the PVCs and the
    //    per-commit VolumeSnapshots; on Docker it removes the container.
    if let Some(runtime) = config.runtime.as_ref() {
        let name = runtime.container_name.trim();
        if !name.is_empty() {
            let id = InstanceId(name.to_string());
            let _ = compute.stop(&id).await;
            match compute.remove_instance(&id).await {
                Ok(()) => println!("  {} removed compute instance {}", green("✓"), cyan(name)),
                Err(e) => {
                    eprintln!(
                        "  {} could not remove instance {name}: {e} (continuing)",
                        yellow("!")
                    )
                }
            }
        }
    }

    // 2. Remove the on-disk `.gfs` store. The per-commit snapshots are made
    //    read-only (dirs 0500), so their entries can't be unlinked until write is
    //    restored — a plain removal fails.
    if std::fs::remove_dir_all(&gfs_dir).is_err() && gfs_dir.exists() {
        // Restore owner write/traverse on the host. This succeeds whenever the
        // current user owns the files — the usual case, including Docker Desktop
        // on macOS, where the bind mount surfaces snapshot files as the host user
        // (so a container, even as root, cannot chmod them, but we can).
        let _ = std::process::Command::new("chmod")
            .arg("-R")
            .arg("u+rwX")
            .arg(&gfs_dir)
            .status();
        if std::fs::remove_dir_all(&gfs_dir).is_err() && gfs_dir.exists() && !is_k8s {
            // Files owned by a different UID (e.g. a Linux container's postgres):
            // remove them as root from inside an ephemeral container.
            remove_gfs_as_root(&compute, &config, &repo_path).await?;
            let _ = std::fs::remove_dir_all(&gfs_dir);
        }
    }

    if gfs_dir.exists() {
        bail!(
            "could not fully remove {0} — remove it manually (e.g. sudo rm -rf {0})",
            gfs_dir.display()
        );
    }
    println!(
        "  {} destroyed GFS repository at {}",
        green("✓"),
        cyan(repo_path.display().to_string())
    );
    Ok(())
}

/// Remove `<repo>/.gfs` as root via an ephemeral container that bind-mounts the
/// repository — the Docker runtime writes snapshot files as the container's
/// postgres UID (mode 0700), which the host user cannot delete directly.
async fn remove_gfs_as_root(
    compute: &Arc<dyn Compute>,
    config: &GfsConfig,
    repo_path: &Path,
) -> Result<()> {
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(registry.as_ref())
        .map_err(|e| anyhow::anyhow!("register providers: {e}"))?;
    let provider_name = config
        .environment
        .as_ref()
        .map(|e| e.database_provider.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("postgres");
    let provider = registry
        .get(provider_name)
        .ok_or_else(|| anyhow::anyhow!("unknown database provider '{provider_name}'"))?;

    // Reuse the repo's own engine image (guaranteed present locally): re-tag the
    // provider default with the configured major version.
    let mut definition = provider.definition();
    if let Some(env) = config.environment.as_ref()
        && !env.database_version.is_empty()
        && let Some((base, _)) = definition.image.clone().rsplit_once(':')
    {
        definition.image = format!("{base}:{}", env.database_version);
    }
    definition.host_data_dir = Some(repo_path.to_path_buf());
    definition.data_dir = PathBuf::from("/work");
    definition.user = Some("0:0".to_string());
    definition.ports = vec![];

    // Restore owner write/traverse first: snapshots are made read-only (dirs
    // 0500), so their entries can't be unlinked until the write bit is back —
    // `rm` alone fails even as root.
    let out = compute
        .run_task(
            &definition,
            "chmod -R u+rwX /work/.gfs && rm -rf /work/.gfs",
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("root cleanup task: {e}"))?;
    if out.exit_code != 0 {
        bail!(
            "root cleanup failed (exit {}): {}",
            out.exit_code,
            out.stderr.trim()
        );
    }
    Ok(())
}
