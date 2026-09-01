use std::path::PathBuf;
use std::sync::Arc;

use gfs_compute_docker::DockerCompute;
use gfs_compute_docker::containers;
use gfs_compute_kubernetes::KubernetesCompute;
use gfs_domain::ports::compute::Compute;
use gfs_domain::ports::database_provider::InMemoryDatabaseProviderRegistry;
use gfs_domain::usecases::repository::clone_repo_usecase::CloneRepoUseCase;
use gfs_domain::utils::remote_source::parse_postgres_url;
use serde_json::json;

use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, dimmed, green};

type CmdError = Box<dyn std::error::Error + Send + Sync>;

async fn runtime_compute() -> Result<Arc<dyn Compute>, CmdError> {
    let k8s = std::env::var("GFS_RUNTIME_PROVIDER")
        .map(|v| {
            let v = v.to_ascii_lowercase();
            v == "kubernetes" || v == "k8s" || v == "k3s"
        })
        .unwrap_or(false);
    if k8s {
        Ok(Arc::new(
            KubernetesCompute::new(None)
                .await
                .map_err(|e| std::io::Error::other(e.to_string()))?,
        ))
    } else {
        Ok(Arc::new(
            DockerCompute::new().map_err(|e| std::io::Error::other(e.to_string()))?,
        ))
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn clone(
    from: String,
    path: Option<PathBuf>,
    database_version: Option<String>,
    image: Option<String>,
    platform: Option<String>,
    port: Option<u16>,
    snapshot: bool,
    max_bytes: Option<u64>,
    force: bool,
    json_output: bool,
) -> Result<(), CmdError> {
    let remote = parse_postgres_url(&from).map_err(|e| e.to_string())?;
    let target_path = path.unwrap_or_else(get_repo_dir);

    let compute = runtime_compute().await?;
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(registry.as_ref())?;
    let use_case = CloneRepoUseCase::new(compute.clone(), registry);

    let version = if image.is_some() {
        None
    } else {
        match database_version {
            Some(v) => Some(v),
            None => match use_case.detect_remote_version(&remote).await {
                Ok(v) => {
                    if !json_output {
                        println!("  {} Detected remote version {}", green("✓"), cyan(&v));
                    }
                    Some(v)
                }
                Err(e) => {
                    // Fail loudly rather than guessing a major version. A wrong
                    // guess provisions a mismatched local engine whose pg_dump
                    // then refuses the source ("server version mismatch"), so a
                    // transient probe failure would silently corrupt the clone.
                    return Err(format!(
                        "could not detect remote PostgreSQL version ({e}); \
                         retry, or pass --database-version <major> (e.g. --database-version 17)"
                    )
                    .into());
                }
            },
        }
    };

    let labels = std::collections::BTreeMap::from([
        ("gfs.role".to_string(), "clone".to_string()),
        (
            "gfs.remote".to_string(),
            format!("{}:{}", remote.host, remote.port),
        ),
    ]);
    crate::commands::cmd_init::init(
        Some(target_path.clone()),
        Some("postgres".to_string()),
        version,
        port,
        Default::default(),
        json_output,
        // No explicit `--image`: pass `None` so `init` channels the resolved
        // `version` onto the provider's default base (`gfs-postgres:<version>`).
        // Hardcoding an image here pins the tag and bypasses channeling, so a
        // clone of a v17 remote would wrongly provision pg16.
        image,
        platform,
        labels,
    )
    .await?;

    let output = use_case.run(&target_path, remote).await?;

    if json_output && !snapshot {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "path": target_path.display().to_string(),
                "remote": output.remote,
                "mode": "lazy-clone",
            }))?
        );
    } else if !json_output {
        println!();
        println!(
            "  {} Lazy clone ready from {}",
            green("✓"),
            cyan(output.remote)
        );
        println!(
            "    {:<16} copy-on-read (data fetched on first read)",
            dimmed("Mode")
        );
    }
    if !output.stderr.is_empty() {
        eprintln!("{}", output.stderr.trim_end());
    }

    // #132: `--snapshot` = clone + freeze, the exact `gfs freeze` a user could
    // run later. The clone above is COMPLETE either way: if the freeze is
    // refused (copy budget) or fails, the repository stays a working LAZY
    // clone, and the error says so instead of pretending the clone broke.
    if snapshot {
        if !json_output {
            println!();
            println!("  {} taking the snapshot (gfs freeze)...", dimmed("\u{b7}"));
        }
        if let Err(e) = crate::commands::cmd_freeze::freeze(
            Some(target_path.clone()),
            max_bytes,
            force,
            json_output,
        )
        .await
        {
            return Err(format!(
                "the clone succeeded and remains a working LAZY clone, \
                 but the snapshot step failed: {e:#}"
            )
            .into());
        }
        if !json_output {
            println!(
                "  {} Snapshot clone ready {}",
                green("\u{2713}"),
                dimmed("(a point in time, detached from its source)")
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use gfs_domain::utils::remote_source::parse_postgres_url;

    #[test]
    fn parses_full_url() {
        let r = parse_postgres_url("postgres://alice:s3cret@db.example.com:6543/shop").unwrap();
        assert_eq!(r.user, "alice");
        assert_eq!(r.dbname, "shop");
    }
}
