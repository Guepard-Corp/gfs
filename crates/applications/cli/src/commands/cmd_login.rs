use anyhow::{Context, Result};

use gfs_console_remote::{login_with_password, login_with_token, resolve_supabase_config};

pub async fn run(
    email: Option<String>,
    password: Option<String>,
    token: Option<String>,
) -> Result<()> {
    if let Some(token) = token {
        login_with_token(&token)?;
        eprintln!("token saved to ~/.config/guepard/credentials.toml");
        return Ok(());
    }

    let supabase = resolve_supabase_config()?;
    let email = email
        .or_else(|| std::env::var("GUEPARD_LOGIN_EMAIL").ok())
        .context("pass --email or set GUEPARD_LOGIN_EMAIL")?;
    let password = password
        .or_else(|| std::env::var("GUEPARD_LOGIN_PASSWORD").ok())
        .context("pass --password or set GUEPARD_LOGIN_PASSWORD")?;

    login_with_password(&supabase.url, &supabase.anon_key, &email, &password).await?;
    eprintln!("logged in; token saved to ~/.config/guepard/credentials.toml");
    Ok(())
}
