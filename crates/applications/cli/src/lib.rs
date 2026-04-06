//! `gfs` – Guepard data-plane CLI library.
//!
//! Provides a programmatic interface to run the CLI. Use `run()` for testing or embedding.

mod cli_utils;
mod commands;
pub mod output;

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Result;
use clap::error::ErrorKind;
use clap::{Parser, Subcommand};
use gfs_domain::ports::storage::{CloneOptions, SnapshotId, SnapshotOptions, VolumeId};
use gfs_telemetry::TelemetryClient;
use serde_json::json;

use crate::output::ColorMode;

// ---------------------------------------------------------------------------
// Schema subcommands (used by commands)
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum SchemaAction {
    /// Extract schema from the running database (default action)
    Extract {
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        compact: bool,
    },
    /// Show schema from a specific commit
    Show {
        commit: String,
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long)]
        metadata_only: bool,
        #[arg(long)]
        ddl_only: bool,
    },
    /// Compare schemas between two commits
    Diff {
        commit1: String,
        commit2: String,
        #[arg(long)]
        path: Option<PathBuf>,

        /// Use human-readable pretty format with colors and visual tree
        #[arg(long)]
        pretty: bool,

        /// Output structured JSON format (cannot be used with --pretty)
        #[arg(long)]
        json: bool,

        /// Disable color output (also respects NO_COLOR env var)
        #[arg(long)]
        no_color: bool,
    },
}

// ---------------------------------------------------------------------------
// Compute subcommands (used by commands)
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum ComputeAction {
    Status {
        #[arg(long)]
        id: Option<String>,
    },
    Start {
        #[arg(long)]
        id: Option<String>,
    },
    Stop {
        #[arg(long)]
        id: Option<String>,
    },
    Restart {
        #[arg(long)]
        id: Option<String>,
    },
    Pause {
        #[arg(long)]
        id: Option<String>,
    },
    Unpause {
        #[arg(long)]
        id: Option<String>,
    },
    Logs {
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        tail: Option<usize>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_missing_value = "true", num_args = 0..=1)]
        stdout: Option<bool>,
        #[arg(long, default_missing_value = "true", num_args = 0..=1)]
        stderr: Option<bool>,
    },
    /// Read or write a compute config value (e.g. db.port)
    Config {
        /// Config key (currently only `db.port` is supported)
        key: String,
        /// Value to set
        value: String,
    },
}

// ---------------------------------------------------------------------------
// MCP daemon subcommands (used by commands)
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum McpAction {
    Start,
    Stop,
    Restart,
    Status,
    Stdio,
    Web {
        #[arg(long, default_value = "3000")]
        port: u16,
    },
    Version,
}

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "gfs",
    about = "Git For database Systems CLI",
    version,
    propagate_version = true
)]
struct Cli {
    /// When to use colors: always, auto (default), or never
    #[arg(long, global = true, default_value_t = ColorMode::Auto, value_enum)]
    color: ColorMode,

    #[command(subcommand)]
    command: TopLevel,
}

#[derive(Subcommand)]
enum TopLevel {
    /// Initialize a new GFS environment at the given path
    Init {
        /// Path where to initialize the .gfs repo (default: current directory)
        path: Option<PathBuf>,

        /// Database provider to deploy (e.g. postgres). If set, the repo is initialized and the database is provisioned and started. Requires --database-version.
        #[arg(long)]
        database_provider: Option<String>,

        /// Database version (e.g. 17 for postgres). Required when --database-provider is set.
        #[arg(long)]
        database_version: Option<String>,

        /// Host port to bind for the database container (e.g. 5432). Default: Docker auto-assigns.
        #[arg(long)]
        port: Option<u16>,
    },

    /// Record a commit of the current repository state
    Commit {
        /// Commit message (required)
        #[arg(short = 'm', long)]
        message: String,

        /// Path to the GFS repository root (default: current directory)
        #[arg(long)]
        path: Option<PathBuf>,

        /// Override the author name (falls back to user.name in repo config)
        #[arg(long)]
        author: Option<String>,

        /// Override the author e-mail (falls back to user.email in repo config)
        #[arg(long)]
        author_email: Option<String>,
    },

    /// Read or write repository or global config (e.g. user.name, user.email)
    Config {
        /// Apply to global config (~/.gfs/config.toml) instead of repo-local .gfs/config.toml
        #[arg(long, short = 'g')]
        global: bool,

        /// Path to the GFS repository root (default: current directory); ignored with --global
        #[arg(long)]
        path: Option<PathBuf>,

        /// Config key (e.g. user.name, user.email)
        key: String,

        /// Value to set; omit to read
        value: Option<String>,
    },

    /// Switch branch or checkout a commit (detached HEAD). Use -b to create a new branch.
    Checkout {
        /// Path to the GFS repository root (default: current directory)
        #[arg(long)]
        path: Option<PathBuf>,

        /// Create a new branch and switch to it (optional start revision defaults to HEAD)
        #[arg(short = 'b', long = "branch")]
        create_branch: Option<String>,

        /// Branch name or full 64-char commit hash; or start revision when using -b
        revision: Option<String>,
    },

    /// List, create, or delete branches
    Branch {
        /// Name of the branch to create (omit to list all branches)
        name: Option<String>,

        /// Commit or branch to start the new branch from (default: HEAD)
        start_point: Option<String>,

        /// Delete the named branch
        #[arg(short = 'd', long)]
        delete: Option<String>,

        /// Switch to the new branch after creating it (like checkout -b)
        #[arg(short = 'c', long)]
        checkout: bool,

        /// Path to the GFS repository root (default: current directory)
        #[arg(long)]
        path: Option<PathBuf>,
    },

    /// Export data from the running database instance to a file
    Export {
        /// Path to the GFS repository root (default: current directory)
        #[arg(long)]
        path: Option<PathBuf>,

        /// Directory where the export file will be written (created if absent)
        /// Defaults to .gfs/exports/ if not provided
        #[arg(long)]
        output_dir: Option<PathBuf>,

        /// Export format (e.g. sql, custom)
        #[arg(long)]
        format: String,

        /// Container name or id override (defaults to repo runtime.container_name)
        #[arg(long)]
        id: Option<String>,
    },

    /// Import data into the running database instance from a file
    Import {
        /// Path to the GFS repository root (default: current directory)
        #[arg(long)]
        path: Option<PathBuf>,

        /// Path to the dump file to import
        #[arg(long)]
        file: PathBuf,

        /// Import format (e.g. sql, custom); inferred from file extension when omitted
        #[arg(long)]
        format: Option<String>,

        /// Container name or id override (defaults to repo runtime.container_name)
        #[arg(long)]
        id: Option<String>,
    },

    /// List database providers and their supported versions. Pass a provider name for details.
    Providers {
        /// Provider name to show details for (e.g. postgres). Omit to list all providers.
        #[arg()]
        provider: Option<String>,
    },

    /// Display commit history
    Log {
        /// Path to the GFS repository root (default: current directory)
        #[arg(long)]
        path: Option<PathBuf>,

        /// Limit the number of commits to display
        #[arg(short = 'n', long)]
        max_count: Option<usize>,

        /// Start traversal at this revision (branch name or full hash)
        #[arg(long)]
        from: Option<String>,

        /// Stop before this revision (exclusive)
        #[arg(long)]
        until: Option<String>,

        /// Show full 64-character commit hashes
        #[arg(long)]
        full_hash: bool,
    },

    /// Show repository and compute status (current branch, container state, connection string)
    Status {
        /// Path to the GFS repository root (default: current directory)
        #[arg(long)]
        path: Option<PathBuf>,

        /// Output format: table (default) or json
        #[arg(long, default_value = "table", value_parser = ["table", "json"])]
        output: String,
    },

    /// Storage operations (mount, unmount, snapshot, clone, status, quota)
    Storage {
        #[command(subcommand)]
        action: StorageAction,
    },

    /// Compute instance management (Docker containers)
    Compute {
        /// Path to the GFS repository root (default: current directory). When set, --id may be omitted and the container name is read from .gfs/config.toml (runtime.container_name).
        #[arg(long)]
        path: Option<PathBuf>,

        #[command(subcommand)]
        action: ComputeAction,
    },

    /// MCP server (start stdio, daemon, or utilities). Defaults to stdio if no subcommand given.
    Mcp {
        /// Path to the GFS repository root (default: current directory). The daemon will use this repo.
        #[arg(long)]
        path: Option<PathBuf>,

        #[command(subcommand)]
        action: Option<McpAction>,
    },

    /// Execute a SQL query or open an interactive database terminal
    Query {
        /// Path to the GFS repository root (default: current directory)
        #[arg(long)]
        path: Option<PathBuf>,

        /// Database name to query (overrides the default from container config)
        #[arg(long)]
        database: Option<String>,

        /// SQL query to execute (omit to open interactive terminal)
        query: Option<String>,
    },

    /// Database schema operations (extract, show, diff)
    Schema {
        #[command(subcommand)]
        action: SchemaAction,
    },

    /// Print the CLI version
    Version,
}

// ---------------------------------------------------------------------------
// Storage subcommands
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
enum StorageAction {
    /// Mount a volume at the given path
    Mount {
        #[arg(long)]
        id: String,
        #[arg(long)]
        mount_point: PathBuf,
    },
    Unmount {
        #[arg(long)]
        id: String,
    },
    Snapshot {
        #[arg(long)]
        id: String,
        #[arg(long)]
        label: Option<String>,
    },
    Clone {
        #[arg(long)]
        source: String,
        #[arg(long)]
        target: String,
        #[arg(long)]
        from_snapshot: Option<String>,
    },
    Status {
        #[arg(long)]
        id: String,
    },
    Quota {
        #[arg(long)]
        id: String,
    },
}

// ---------------------------------------------------------------------------
// Run entry point
// ---------------------------------------------------------------------------

fn command_name(cmd: &TopLevel) -> &'static str {
    match cmd {
        TopLevel::Init { .. } => "init",
        TopLevel::Commit { .. } => "commit",
        TopLevel::Config { .. } => "config",
        TopLevel::Checkout { .. } => "checkout",
        TopLevel::Branch { .. } => "branch",
        TopLevel::Export { .. } => "export",
        TopLevel::Import { .. } => "import",
        TopLevel::Providers { .. } => "providers",
        TopLevel::Log { .. } => "log",
        TopLevel::Status { .. } => "status",
        TopLevel::Query { .. } => "query",
        TopLevel::Schema { .. } => "schema",
        TopLevel::Storage { .. } => "storage",
        TopLevel::Compute { .. } => "compute",
        TopLevel::Mcp { .. } => "mcp",
        TopLevel::Version => "version",
    }
}

/// Run the CLI with the given arguments. Returns `Ok(exit_code)` on success (0 for most
/// commands; 1 or 2 for `schema diff` when there are changes). Returns `Err` on failure.
/// Use for programmatic invocation and unit tests.
pub async fn run<I, T>(args: I) -> Result<i32>
where
    I: IntoIterator<Item = T>,
    T: AsRef<str>,
{
    let args_os: Vec<OsString> = args
        .into_iter()
        .map(|a| OsString::from(a.as_ref().to_string()))
        .collect();

    // Init color early so error messages (e.g. parse errors) can use it
    ColorMode::Auto.init();
    let cli = match Cli::try_parse_from(args_os) {
        Ok(c) => c,
        Err(e) if e.kind() == ErrorKind::DisplayVersion || e.kind() == ErrorKind::DisplayHelp => {
            e.print().expect("writing version/help to stdout/stderr");
            return Ok(0);
        }
        Err(e) => return Err(e.into()),
    };
    cli.color.init();

    // Skip telemetry for Version and Mcp (MCP tracks its own events)
    let skip_telemetry = matches!(cli.command, TopLevel::Version | TopLevel::Mcp { .. });
    let cmd_name = command_name(&cli.command);
    let telemetry = TelemetryClient::new();
    let source = gfs_telemetry::detect_source();
    let version = env!("CARGO_PKG_VERSION");
    let os = std::env::consts::OS;

    // Capture color before moving cli.command (ColorMode is Copy)
    let color = cli.color;

    let result: Result<i32> = async move {
        match cli.command {
            TopLevel::Init {
                path,
                database_provider,
                database_version,
                port,
            } => {
                commands::cmd_init::init(path, database_provider, database_version, port)
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                Ok(0)
            }
            TopLevel::Commit {
                message,
                path,
                author,
                author_email,
            } => {
                commands::cmd_commit::commit(path, message, author, author_email).await?;
                Ok(0)
            }
            TopLevel::Config {
                path,
                key,
                value,
                global,
            } => {
                commands::cmd_config::run(path, key, value, global)
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                Ok(0)
            }
            TopLevel::Checkout {
                path,
                create_branch,
                revision,
            } => {
                commands::cmd_checkout::checkout(path, revision, create_branch).await?;
                Ok(0)
            }
            TopLevel::Branch {
                name,
                start_point,
                delete,
                checkout,
                path,
            } => {
                commands::cmd_branch::run(path, name, start_point, delete, checkout).await?;
                Ok(0)
            }
            TopLevel::Export {
                path,
                output_dir,
                format,
                id,
            } => {
                commands::cmd_export::run(path, output_dir, format, id).await?;
                Ok(0)
            }
            TopLevel::Import {
                path,
                file,
                format,
                id,
            } => {
                commands::cmd_import::run(path, file, format, id).await?;
                Ok(0)
            }
            TopLevel::Providers { provider } => {
                commands::cmd_providers::run(provider).map_err(|e| anyhow::anyhow!("{}", e))?;
                Ok(0)
            }
            TopLevel::Log {
                path,
                max_count,
                from,
                until,
                full_hash,
            } => {
                commands::cmd_log::log(path, max_count, from, until, full_hash).await?;
                Ok(0)
            }
            TopLevel::Status { path, output } => {
                commands::cmd_status::run(path, output).await?;
                Ok(0)
            }
            TopLevel::Query {
                path,
                database,
                query,
            } => {
                commands::cmd_query::run(path, database, query).await?;
                Ok(0)
            }
            TopLevel::Schema { action } => match action {
                SchemaAction::Extract {
                    path,
                    output,
                    compact,
                } => {
                    commands::cmd_schema::run_extract(path, output, compact).await?;
                    Ok(0)
                }
                SchemaAction::Show {
                    commit,
                    path,
                    metadata_only,
                    ddl_only,
                } => {
                    commands::cmd_schema::run_show(commit, path, metadata_only, ddl_only).await?;
                    Ok(0)
                }
                SchemaAction::Diff {
                    commit1,
                    commit2,
                    path,
                    pretty,
                    json,
                    no_color,
                } => {
                    let no_color = no_color || color == ColorMode::Never;
                    commands::cmd_schema::run_diff(commit1, commit2, path, pretty, json, no_color)
                        .await
                }
            },
            TopLevel::Storage { action } => {
                run_storage(action).await?;
                Ok(0)
            }
            TopLevel::Compute { path, action } => {
                run_compute(path, action).await?;
                Ok(0)
            }
            TopLevel::Mcp { path, action } => {
                let action = action.unwrap_or(McpAction::Stdio);
                commands::cmd_mcp::run(path, action).await?;
                Ok(0)
            }
            TopLevel::Version => {
                commands::cmd_version::run();
                Ok(0)
            }
        }
    }
    .await;

    if !skip_telemetry {
        if let Err(ref e) = result {
            telemetry.track(
                "command_failed",
                vec![
                    ("command", json!(cmd_name)),
                    ("source", json!(source)),
                    ("version", json!(version)),
                    ("os", json!(os)),
                    ("error_category", json!(gfs_telemetry::error_category(e))),
                ],
            );
        } else {
            telemetry.track(
                "command_executed",
                vec![
                    ("command", json!(cmd_name)),
                    ("source", json!(source)),
                    ("version", json!(version)),
                    ("os", json!(os)),
                ],
            );
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Storage dispatch
// ---------------------------------------------------------------------------

async fn run_storage(action: StorageAction) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use gfs_storage_apfs::ApfsStorage;
        let storage = ApfsStorage::new();
        dispatch_storage(&storage, action).await
    }

    #[cfg(not(target_os = "macos"))]
    {
        use gfs_storage_file::FileStorage;
        let storage = FileStorage::new();
        dispatch_storage(&storage, action).await
    }
}

async fn dispatch_storage(
    storage: &impl gfs_domain::ports::storage::StoragePort,
    action: StorageAction,
) -> Result<()> {
    match action {
        StorageAction::Mount { id, mount_point } => {
            storage
                .mount(&VolumeId(id), &mount_point)
                .await
                .map_err(anyhow::Error::from)?;
            println!("mounted");
        }
        StorageAction::Unmount { id } => {
            storage
                .unmount(&VolumeId(id))
                .await
                .map_err(anyhow::Error::from)?;
            println!("unmounted");
        }
        StorageAction::Snapshot { id, label } => {
            let snap = storage
                .snapshot(&VolumeId(id), SnapshotOptions { label })
                .await
                .map_err(anyhow::Error::from)?;
            println!("snapshot id  : {}", snap.id);
            println!("volume       : {}", snap.volume_id);
            println!("created_at   : {}", snap.created_at);
            if let Some(lbl) = &snap.label {
                println!("label        : {lbl}");
            }
        }
        StorageAction::Clone {
            source,
            target,
            from_snapshot,
        } => {
            let opts = CloneOptions {
                from_snapshot: from_snapshot.map(SnapshotId),
            };
            let status = storage
                .clone(&VolumeId(source), VolumeId(target), opts)
                .await
                .map_err(anyhow::Error::from)?;
            print_volume_status(&status);
        }
        StorageAction::Status { id } => {
            let status = storage
                .status(&VolumeId(id))
                .await
                .map_err(anyhow::Error::from)?;
            print_volume_status(&status);
        }
        StorageAction::Quota { id } => {
            let quota = storage
                .quota(&VolumeId(id))
                .await
                .map_err(anyhow::Error::from)?;
            println!("volume      : {}", quota.volume_id);
            println!("limit_bytes : {}", quota.limit_bytes);
            println!("used_bytes  : {}", quota.used_bytes);
            println!("free_bytes  : {}", quota.free_bytes);
        }
    }
    Ok(())
}

async fn run_compute(path: Option<PathBuf>, action: ComputeAction) -> Result<()> {
    commands::cmd_compute::run(path, action).await
}

fn print_volume_status(s: &gfs_domain::ports::storage::VolumeStatus) {
    println!("id          : {}", s.id);
    println!(
        "mount_point : {}",
        s.mount_point
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "-".to_owned())
    );
    println!("status      : {:?}", s.status);
    println!("size_bytes  : {}", s.size_bytes);
    println!("used_bytes  : {}", s.used_bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_version_succeeds() {
        let result = run(["gfs", "version"]).await;
        assert!(
            result.is_ok(),
            "gfs version should succeed: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn run_providers_succeeds() {
        let result = run(["gfs", "providers"]).await;
        assert!(
            result.is_ok(),
            "gfs providers should succeed: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn run_unknown_command_fails() {
        let result = run(["gfs", "nonexistent-subcommand"]).await;
        assert!(result.is_err(), "unknown command should fail");
    }
}
