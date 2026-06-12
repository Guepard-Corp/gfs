use anyhow::Result;
use serde_json::Value;

use super::remote_support::{console_client_for_repo, require_remote_config};
use crate::cli_utils::get_repo_dir;
use crate::output::{cyan, dimmed};

use super::cmd_log::LogArgs;

pub async fn log(args: LogArgs) -> Result<()> {
    let repo_path = args.path.unwrap_or_else(get_repo_dir);
    let client = console_client_for_repo(&repo_path)?;
    let (_cfg, remote) = require_remote_config(&repo_path)?;
    let n = args.max_count.unwrap_or(10);

    let result = if args.graph || args.all {
        client.graph(&remote, Some(n)).await?
    } else {
        client.log(&remote, n).await?
    };

    if args.json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    if args.graph || args.all {
        print_graph_human(&result);
    } else {
        print_log_human(&result);
    }
    Ok(())
}

fn print_log_human(value: &Value) {
    let entries = value
        .get("commits")
        .and_then(|v| v.as_array())
        .or_else(|| value.as_array());
    let Some(arr) = entries else {
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

fn print_graph_human(value: &Value) {
    println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
}
