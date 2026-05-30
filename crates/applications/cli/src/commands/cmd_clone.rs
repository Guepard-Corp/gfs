//! `gfs clone` — lazily clone a read-only remote database (copy-on-read).
//!
//! Scope (RFC 008, v1): PostgreSQL only. The command initialises a local GFS
//! repository with a fresh local Postgres, then bootstraps the foreign-data
//! link and mixed-partition tables so data is fetched on first read.

use std::path::PathBuf;
use std::sync::Arc;

use gfs_compute_docker::DockerCompute;
use gfs_compute_docker::containers;
use gfs_domain::ports::compute::Compute;
use gfs_domain::ports::database_provider::{InMemoryDatabaseProviderRegistry, RemoteSource};
use gfs_domain::usecases::repository::clone_repo_usecase::CloneRepoUseCase;
use serde_json::json;

use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, dimmed, green};

type CmdError = Box<dyn std::error::Error + Send + Sync>;

#[allow(clippy::too_many_arguments)]
pub async fn clone(
    from: String,
    path: Option<PathBuf>,
    database_version: Option<String>,
    image: Option<String>,
    platform: Option<String>,
    port: Option<u16>,
    json_output: bool,
) -> Result<(), CmdError> {
    let remote = parse_postgres_url(&from)?;
    let target_path = path.unwrap_or_else(get_repo_dir);

    let compute: Arc<dyn Compute> =
        Arc::new(DockerCompute::new().map_err(|e| std::io::Error::other(e.to_string()))?);
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(registry.as_ref())?;
    let use_case = CloneRepoUseCase::new(compute, registry);

    // An explicit --image pins its own version, so version detection is skipped.
    // Otherwise provision the local engine with the SAME major version as the
    // remote (an explicit --database-version overrides; else probe the remote).
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
                    eprintln!("gfs: could not detect remote version ({e}); defaulting to 17");
                    Some("17".to_string())
                }
            },
        }
    };

    // 1. Initialise the local repo and provision the matching local Postgres.
    //    (Reuses the standard init path, which also prints repo/connection info.)
    //    Mark the container as a clone (+ its remote) via labels, overriding the
    //    default gfs.role=source, so the warming proxy can discover it.
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
        image,
        platform,
        labels,
    )
    .await?;

    // 2. Bootstrap the lazy clone against the freshly provisioned local DB.
    let output = use_case.run(&target_path, remote).await?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "path": target_path.display().to_string(),
                "remote": output.remote,
                "mode": "lazy-clone",
            }))?
        );
    } else {
        println!();
        println!(
            "  {} Lazy clone ready from {}",
            green("✓"),
            cyan(output.remote)
        );
        println!(
            "    {:<16} {}",
            dimmed("Mode"),
            "copy-on-read (data fetched on first read)"
        );
    }
    if !output.stderr.is_empty() {
        eprintln!("{}", output.stderr.trim_end());
    }

    Ok(())
}

/// Parse a `postgres://user:password@host:port/dbname[?schema=...]` URL into a
/// [`RemoteSource`]. Keeps parsing intentionally simple (no percent-decoding):
/// credentials with reserved characters should be passed already decoded.
fn parse_postgres_url(url: &str) -> Result<RemoteSource, CmdError> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or("remote URL must start with postgres:// or postgresql://")?;

    // Split off an optional query string (?schema=...).
    let (rest, query) = match rest.split_once('?') {
        Some((r, q)) => (r, Some(q)),
        None => (rest, None),
    };

    // Split userinfo from the host part at the last '@' (passwords may contain '@'... rarely;
    // we use the last '@' so the host segment is unambiguous).
    let (userinfo, hostpart) = match rest.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, rest),
    };

    let (user, password) = match userinfo {
        Some(ui) => match ui.split_once(':') {
            Some((u, p)) => (u.to_string(), p.to_string()),
            None => (ui.to_string(), String::new()),
        },
        None => return Err("remote URL must include credentials (user[:password]@)".into()),
    };

    // host[:port]/dbname
    let (hostport, dbname) = hostpart
        .split_once('/')
        .ok_or("remote URL must include a database name (.../dbname)")?;
    if dbname.is_empty() {
        return Err("remote URL must include a database name (.../dbname)".into());
    }

    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| format!("invalid port: '{p}'"))?,
        ),
        None => (hostport.to_string(), 5432),
    };
    if host.is_empty() {
        return Err("remote URL must include a host".into());
    }

    // schemas from query string (?schema=a,b or ?schemas=a,b). Absent → empty,
    // meaning "all non-system schemas" (resolved at bootstrap time).
    let schemas = query
        .and_then(|q| {
            q.split('&').find_map(|kv| {
                kv.strip_prefix("schema=")
                    .or_else(|| kv.strip_prefix("schemas="))
            })
        })
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(RemoteSource {
        host,
        port,
        dbname: dbname.to_string(),
        user,
        password,
        schemas,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_url() {
        let r = parse_postgres_url("postgres://alice:s3cret@db.example.com:6543/shop").unwrap();
        assert_eq!(r.user, "alice");
        assert_eq!(r.password, "s3cret");
        assert_eq!(r.host, "db.example.com");
        assert_eq!(r.port, 6543);
        assert_eq!(r.dbname, "shop");
        assert!(r.schemas.is_empty()); // none specified → all schemas
    }

    #[test]
    fn defaults_port_and_parses_schemas() {
        let r = parse_postgres_url("postgresql://bob@localhost/analytics?schema=reporting,staging")
            .unwrap();
        assert_eq!(r.port, 5432);
        assert_eq!(r.password, "");
        assert_eq!(r.schemas, vec!["reporting".to_string(), "staging".to_string()]);
        assert_eq!(r.dbname, "analytics");
    }

    #[test]
    fn rejects_missing_scheme() {
        assert!(parse_postgres_url("mysql://x@h/db").is_err());
    }

    #[test]
    fn rejects_missing_dbname() {
        assert!(parse_postgres_url("postgres://x:y@h:5432").is_err());
        assert!(parse_postgres_url("postgres://x:y@h:5432/").is_err());
    }

    #[test]
    fn rejects_missing_credentials() {
        assert!(parse_postgres_url("postgres://host:5432/db").is_err());
    }
}
