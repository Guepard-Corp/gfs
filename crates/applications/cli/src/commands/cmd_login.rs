use anyhow::{Context, Result};

use gfs_console_remote::login_with_password;

pub async fn run(email: Option<String>, password: Option<String>) -> Result<()> {
    let supabase_url = std::env::var("GUEPARD_SUPABASE_URL")
        .or_else(|_| std::env::var("VITE_SUPABASE_URL"))
        .context("set GUEPARD_SUPABASE_URL or VITE_SUPABASE_URL")?;
    let anon_key = std::env::var("GUEPARD_SUPABASE_ANON_KEY")
        .or_else(|_| std::env::var("VITE_SUPABASE_ANON_KEY"))
        .or_else(|_| std::env::var("SUPABASE_ANON_KEY"))
        .context("set GUEPARD_SUPABASE_ANON_KEY or VITE_SUPABASE_ANON_KEY")?;
    let email = email
        .or_else(|| std::env::var("GUEPARD_LOGIN_EMAIL").ok())
        .context("pass --email or set GUEPARD_LOGIN_EMAIL")?;
    let password = password
        .or_else(|| std::env::var("GUEPARD_LOGIN_PASSWORD").ok())
        .context("pass --password or set GUEPARD_LOGIN_PASSWORD")?;

    login_with_password(&supabase_url, &anon_key, &email, &password).await?;
    println!("logged in; token saved to ~/.config/guepard/credentials.toml");
    Ok(())
}
