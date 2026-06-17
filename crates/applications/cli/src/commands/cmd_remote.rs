use anyhow::Result;
use gfs_console_remote::{ConsoleClient, auth_from_env, remote_config_show, resolve_console_url};

pub async fn show(json_output: bool) -> Result<()> {
    let creds = remote_config_show()?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&creds)?);
    } else {
        println!("console_url: {}", creds.console_url.unwrap_or_default());
        println!("supabase_url: {}", creds.supabase_url.unwrap_or_default());
        println!(
            "access_token: {}",
            creds.access_token.unwrap_or_else(|| "<unset>".into())
        );
    }
    Ok(())
}

pub async fn nodes(json_output: bool) -> Result<()> {
    let auth = auth_from_env()?;
    let base = resolve_console_url()?;
    let client = ConsoleClient::new(base, auth)?;
    let nodes = client.list_nodes().await?;
    #[allow(clippy::if_same_then_else)]
    if json_output {
        println!("{}", serde_json::to_string_pretty(&nodes)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&nodes)?);
    }
    Ok(())
}
