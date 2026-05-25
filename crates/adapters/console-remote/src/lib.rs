//! HTTP client for GFS VCS and deploy operations via guepard-console `apps/server`.
//!
//! Laptop workflows use this instead of `KUBECONFIG` → k3s API.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use gfs_domain::model::config::RemoteConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(130);

#[derive(Debug, Clone)]
pub struct ConsoleAuth {
    pub access_token: String,
}

#[derive(Debug, Clone)]
pub struct ConsoleClient {
    base_url: String,
    auth: ConsoleAuth,
    http: reqwest::Client,
}

#[derive(Debug, Deserialize)]
pub struct DeployResponse {
    pub deployment: Value,
    pub engine: Value,
}

#[derive(Debug, Deserialize)]
struct SupabaseTokenResponse {
    access_token: String,
}

impl ConsoleClient {
    pub fn from_env() -> Result<Self> {
        let auth = auth_from_env()?;
        let base = std::env::var("GUEPARD_CONSOLE_URL")
            .context("GUEPARD_CONSOLE_URL is not set (e.g. http://<cp-host>:32298)")?;
        Self::new(base, auth)
    }

    pub fn new(base_url: String, auth: ConsoleAuth) -> Result<Self> {
        let base = base_url.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .context("reqwest client")?;
        Ok(Self {
            base_url: base,
            auth,
            http,
        })
    }

    fn api_engine(&self) -> String {
        format!("{}/api/engine", self.base_url)
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.auth.access_token)
    }

    async fn post_json(&self, url: &str, body: &impl Serialize) -> Result<Value> {
        let res = self
            .http
            .post(url)
            .header("Authorization", self.bearer())
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .context("console API request")?;
        let status = res.status();
        let text = res.text().await.context("read response body")?;
        if !status.is_success() {
            bail!("console API {url} failed ({status}): {text}");
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).context("parse JSON response")
    }

    async fn get_json(&self, url: &str) -> Result<Value> {
        let res = self
            .http
            .get(url)
            .header("Authorization", self.bearer())
            .send()
            .await
            .context("console API request")?;
        let status = res.status();
        let text = res.text().await.context("read response body")?;
        if !status.is_success() {
            bail!("console API {url} failed ({status}): {text}");
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).context("parse JSON response")
    }

    pub async fn deploy_database(
        &self,
        engine_node_id: &str,
        provider: &str,
        version: &str,
        name: Option<&str>,
        project: &str,
    ) -> Result<DeployResponse> {
        let url = format!("{}/deployments", self.api_engine());
        let body = serde_json::json!({
            "engineNodeId": engine_node_id,
            "provider": provider,
            "version": version,
            "name": name,
            "project": project,
        });
        let val = self.post_json(&url, &body).await?;
        serde_json::from_value(val).context("parse deploy response")
    }

    pub async fn commit(&self, remote: &RemoteConfig, message: &str) -> Result<Value> {
        let url = format!(
            "{}/nodes/{}/databases/{}/commit",
            self.api_engine(),
            remote.node_id,
            remote.database_id
        );
        self.post_json(&url, &serde_json::json!({ "message": message }))
            .await
    }

    pub async fn log(&self, remote: &RemoteConfig, n: usize) -> Result<Value> {
        let url = format!(
            "{}/nodes/{}/databases/{}/log?n={n}",
            self.api_engine(),
            remote.node_id,
            remote.database_id
        );
        self.get_json(&url).await
    }

    pub async fn checkout(
        &self,
        remote: &RemoteConfig,
        revision: &str,
        create_branch: Option<&str>,
    ) -> Result<Value> {
        let url = format!(
            "{}/nodes/{}/databases/{}/checkout",
            self.api_engine(),
            remote.node_id,
            remote.database_id
        );
        let mut body = serde_json::json!({ "revision": revision });
        if let Some(b) = create_branch {
            body["create_branch"] = Value::String(b.to_string());
        }
        self.post_json(&url, &body).await
    }

    pub async fn query(&self, remote: &RemoteConfig, sql: &str) -> Result<Value> {
        let url = format!(
            "{}/nodes/{}/databases/{}/query",
            self.api_engine(),
            remote.node_id,
            remote.database_id
        );
        self.post_json(&url, &serde_json::json!({ "sql": sql }))
            .await
    }
}

/// Sign in with Supabase password grant; store token in credentials file.
pub async fn login_with_password(
    supabase_url: &str,
    anon_key: &str,
    email: &str,
    password: &str,
) -> Result<()> {
    let url = format!(
        "{}/auth/v1/token?grant_type=password",
        supabase_url.trim_end_matches('/')
    );
    let client = reqwest::Client::new();
    let res = client
        .post(&url)
        .header("apikey", anon_key)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "email": email, "password": password }))
        .send()
        .await
        .context("supabase auth")?;
    let status = res.status();
    let text = res.text().await?;
    if !status.is_success() {
        bail!("supabase login failed ({status}): {text}");
    }
    let parsed: SupabaseTokenResponse =
        serde_json::from_str(&text).context("parse supabase token response")?;
    save_credentials(&CredentialsFile {
        access_token: Some(parsed.access_token),
        ..Default::default()
    })?;
    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CredentialsFile {
    access_token: Option<String>,
    supabase_url: Option<String>,
    supabase_anon_key: Option<String>,
}

fn credentials_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .context("cannot determine home directory")?;
    Ok(home.join(".config").join("guepard").join("credentials.toml"))
}

pub fn save_credentials(creds: &CredentialsFile) -> Result<()> {
    let path = credentials_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = toml::to_string_pretty(creds).context("serialize credentials")?;
    std::fs::write(path, content)?;
    Ok(())
}

fn load_credentials_file() -> Option<CredentialsFile> {
    let path = credentials_path().ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    toml::from_str(&content).ok()
}

pub fn auth_from_env() -> Result<ConsoleAuth> {
    if let Ok(token) = std::env::var("GUEPARD_ACCESS_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(ConsoleAuth {
                access_token: token.trim().to_string(),
            });
        }
    }

    if let Some(file) = load_credentials_file() {
        if let Some(token) = file.access_token.filter(|t| !t.trim().is_empty()) {
            return Ok(ConsoleAuth { access_token: token });
        }
    }

    bail!(
        "no console auth: set GUEPARD_ACCESS_TOKEN or run `gfs login` (stores ~/.config/guepard/credentials.toml)"
    )
}

pub fn block_direct_kubernetes_env() -> Result<()> {
    if std::env::var("KUBECONFIG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .is_some()
    {
        bail!(
            "KUBECONFIG must not be set for GFS remote/console mode. Unset KUBECONFIG and use GUEPARD_CONSOLE_URL instead."
        );
    }
    Ok(())
}
