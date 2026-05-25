use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;

use super::remote_support::{console_client_for_repo, require_remote_config};
use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, dimmed};

use super::cmd_log::LogArgs;

pub async fn log(args: LogArgs) -> Result<()> {
    if args.graph || args.all {
        anyhow::bail!("--graph/--all not supported for guepard remote repos; use plain log");
    }
    let repo_path = args.path.unwrap_or_else(get_repo_dir);
    let client = console_client_for_repo(&repo_path)?;
    let (_cfg, remote) = require_remote_config(&repo_path)?;
    let n = args.max_count.unwrap_or(10);
    let result = client.log(&remote, n).await?;

    if args.json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    print_log_human(&result);
    Ok(())
}

fn print_log_human(value: &Value) {
    let Some(arr) = value.as_array() else {
        println!("{value}");
        return;
    };
    for entry in arr {
        let hash = entry
            .get("hash")
            .or_else(|| entry.get("commit"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let short = if hash.len() >= 7 { &hash[..7] } else { hash };
        let msg = entry
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        println!("{} {}", cyan(short), dimmed(msg));
    }
}
