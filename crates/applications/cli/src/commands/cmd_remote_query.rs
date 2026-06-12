use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;

use super::remote_support::{console_client_for_repo, require_remote_config};
use crate::cli_utils::get_repo_dir;

pub async fn run(
    path: Option<PathBuf>,
    _database: Option<String>,
    query: Option<String>,
    json_output: bool,
) -> Result<()> {
    let sql = query.context("remote query requires SQL (pass as argument or --query)")?;
    let repo_path = path.unwrap_or_else(get_repo_dir);
    let client = console_client_for_repo(&repo_path)?;
    let (_cfg, remote) = require_remote_config(&repo_path)?;
    let result = client.query(&remote, &sql).await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    print_query_human(&result);
    Ok(())
}

fn print_query_human(value: &Value) {
    if let Some(output) = value.get("output").and_then(|v| v.as_str()) {
        print!("{output}");
        if !output.ends_with('\n') {
            println!();
        }
        return;
    }

    let Some(rows) = value.get("rows").and_then(|v| v.as_array()) else {
        println!("{value}");
        return;
    };
    let columns = value
        .get("columns")
        .and_then(|v| v.as_array())
        .map(|cols| {
            cols.iter()
                .filter_map(|c| c.as_str())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if !columns.is_empty() {
        println!("{}", columns.join("\t"));
    }
    for row in rows {
        match row {
            Value::Array(cells) => {
                let line: Vec<String> = cells
                    .iter()
                    .map(|c| match c {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .collect();
                println!("{}", line.join("\t"));
            }
            other => println!("{other}"),
        }
    }
}
