//! HTTP client for GFS VCS and deploy operations via guepard-console `apps/server`.
//!
//! Laptop workflows use this instead of `KUBECONFIG` → k3s API.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use gfs_domain::model::config::RemoteConfig;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(130);
const DEPLOY_POLL_INTERVAL: Duration = Duration::from_secs(3);
const DEPLOY_READY_TIMEOUT: Duration = Duration::from_secs(300);

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

#[derive(Debug, Clone)]
pub struct DeploymentReady {
    pub deployment_id: String,
    pub cp_database_id: String,
    pub connection: Value,
    pub status: Value,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CredentialsFile {
    pub access_token: Option<String>,
    pub console_url: Option<String>,
    pub supabase_url: Option<String>,
    pub supabase_anon_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SupabaseTokenResponse {
    access_token: String,
}

impl ConsoleClient {
    pub fn from_env() -> Result<Self> {
        let auth = auth_from_env()?;
        let base = resolve_console_url()?;
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

    fn api_databases(&self) -> String {
        format!("{}/api/databases", self.base_url)
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.auth.access_token)
    }

    async fn request_json(
        &self,
        method: Method,
        url: &str,
        body: Option<&impl Serialize>,
    ) -> Result<(reqwest::StatusCode, Value)> {
        let mut req = self
            .http
            .request(method, url)
            .header("Authorization", self.bearer());
        if let Some(b) = body {
            req = req
                .header("Content-Type", "application/json")
                .json(b);
        }
        let res = req.send().await.context("console API request")?;
        let status = res.status();
        let text = res.text().await.context("read response body")?;
        if !status.is_success() {
            bail!("console API {url} failed ({status}): {text}");
        }
        let val = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).context("parse JSON response")?
        };
        Ok((status, val))
    }

    async fn post_json(&self, url: &str, body: &impl Serialize) -> Result<Value> {
        let (_, val) = self.request_json(Method::POST, url, Some(body)).await?;
        Ok(val)
    }

    async fn get_json(&self, url: &str) -> Result<Value> {
        let (_, val) = self.request_json(Method::GET, url, None::<&()>).await?;
        Ok(val)
    }

    async fn delete(&self, url: &str) -> Result<()> {
        let (status, _) = self.request_json(Method::DELETE, url, None::<&()>).await?;
        if !status.is_success() {
            bail!("console API DELETE {url} failed ({status})");
        }
        Ok(())
    }

    pub async fn list_nodes(&self) -> Result<Value> {
        self.get_json(&format!("{}/nodes", self.api_engine())).await
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
        let mut body = serde_json::json!({
            "engineNodeId": engine_node_id,
            "provider": provider,
            "version": version,
            "project": project,
        });
        if let Some(name) = name.filter(|s| !s.trim().is_empty()) {
            body["name"] = Value::String(name.to_string());
        }
        let (status, val) = self.request_json(Method::POST, &url, Some(&body)).await?;
        if !status.is_success() && status != reqwest::StatusCode::ACCEPTED {
            bail!("deploy failed ({status})");
        }
        serde_json::from_value(val).context("parse deploy response")
    }

    pub async fn get_deployment(&self, deployment_id: &str) -> Result<Value> {
        self.get_json(&format!("{}/{}", self.api_databases(), deployment_id))
            .await
    }

    pub async fn deployment_status(&self, deployment_id: &str) -> Result<Value> {
        self.get_json(&format!(
            "{}/deployments/{deployment_id}/status",
            self.api_engine()
        ))
        .await
    }

    pub async fn deployment_connection(&self, deployment_id: &str) -> Result<Value> {
        self.get_json(&format!(
            "{}/deployments/{deployment_id}/connection",
            self.api_engine()
        ))
        .await
    }

    pub async fn wait_deployment_ready(
        &self,
        deployment_id: &str,
        timeout: Duration,
    ) -> Result<DeploymentReady> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if std::time::Instant::now() > deadline {
                bail!(
                    "deployment {deployment_id} not ready within {}s",
                    timeout.as_secs()
                );
            }

            let status = self.deployment_status(deployment_id).await?;
            let compute = status
                .get("computeStatus")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let db = self.get_deployment(deployment_id).await?;
            let cp_database_id = db
                .get("cpDatabaseId")
                .or_else(|| db.get("cp_database_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let conn = self.deployment_connection(deployment_id).await?;
            let connection_info = conn.get("connection_info");
            let has_connection = connection_info.is_some_and(|v| !v.is_null());

            if compute == "running" && has_connection {
                if let Some(cp_id) = cp_database_id {
                    return Ok(DeploymentReady {
                        deployment_id: deployment_id.to_string(),
                        cp_database_id: cp_id,
                        connection: conn,
                        status,
                    });
                }
            }

            tokio::time::sleep(DEPLOY_POLL_INTERVAL).await;
        }
    }

    pub async fn wait_deployment_ready_default(&self, deployment_id: &str) -> Result<DeploymentReady> {
        self.wait_deployment_ready(deployment_id, DEPLOY_READY_TIMEOUT)
            .await
    }

    pub async fn start_deployment(&self, deployment_id: &str) -> Result<Value> {
        let url = format!(
            "{}/deployments/{deployment_id}/start",
            self.api_engine()
        );
        self.post_json(&url, &serde_json::json!({})).await
    }

    pub async fn stop_deployment(&self, deployment_id: &str) -> Result<Value> {
        let url = format!(
            "{}/deployments/{deployment_id}/stop",
            self.api_engine()
        );
        self.post_json(&url, &serde_json::json!({})).await
    }

    pub async fn destroy_deployment(&self, deployment_id: &str) -> Result<()> {
        let url = format!("{}/{}", self.api_databases(), deployment_id);
        self.delete(&url).await
    }

    pub async fn commit(&self, remote: &RemoteConfig, message: &str) -> Result<Value> {
        let url = format!(
            "{}/deployments/{}/commit",
            self.api_engine(),
            remote.deployment_id()
        );
        self.post_json(&url, &serde_json::json!({ "message": message }))
            .await
    }

    pub async fn log(&self, remote: &RemoteConfig, n: usize) -> Result<Value> {
        let url = format!(
            "{}/deployments/{}/log?n={n}",
            self.api_engine(),
            remote.deployment_id()
        );
        self.get_json(&url).await
    }

    pub async fn graph(&self, remote: &RemoteConfig, n: Option<usize>) -> Result<Value> {
        let mut url = format!(
            "{}/nodes/{}/databases/{}/graph",
            self.api_engine(),
            remote.node_id,
            remote.cp_database_id()
        );
        if let Some(n) = n {
            url.push_str(&format!("?n={n}"));
        }
        self.get_json(&url).await
    }

    pub async fn checkout(
        &self,
        remote: &RemoteConfig,
        revision: &str,
        create_branch: Option<&str>,
    ) -> Result<Value> {
        if create_branch.is_some() {
            let db = self.get_deployment(remote.deployment_id()).await?;
            let deployment_type = db
                .get("deploymentType")
                .and_then(|v| v.as_str())
                .unwrap_or("repository");
            if deployment_type == "repository" {
                bail!(
                    "branch create is not supported on primary deployments (linear history); create a clone deployment first"
                );
            }
        }

        // CP rejects checkout while the row is transitional (409 conflict).
        self.wait_compute_running(remote, DEPLOY_READY_TIMEOUT).await?;

        let url = format!(
            "{}/deployments/{}/checkout",
            self.api_engine(),
            remote.deployment_id()
        );
        let mut body = serde_json::json!({ "revision": revision });
        if let Some(b) = create_branch {
            body["create_branch"] = Value::String(b.to_string());
        }

        let val = self.post_json(&url, &body).await?;
        if val.get("commit").and_then(|v| v.as_str()).is_none() {
            bail!("checkout response missing commit hash");
        }

        // Checkout reprovisions the instance; wait until serving again.
        self.wait_compute_running(remote, DEPLOY_READY_TIMEOUT).await?;
        Ok(val)
    }

    async fn wait_compute_running(
        &self,
        remote: &RemoteConfig,
        timeout: Duration,
    ) -> Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if std::time::Instant::now() > deadline {
                bail!("timed out waiting for compute to reach running");
            }
            let status = self.deployment_status(remote.deployment_id()).await?;
            let compute = status
                .get("computeStatus")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let cp = status
                .get("cpStatus")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if compute == "running" && cp.eq_ignore_ascii_case("running") {
                return Ok(());
            }
            tokio::time::sleep(DEPLOY_POLL_INTERVAL).await;
        }
    }

    pub async fn query(&self, remote: &RemoteConfig, sql: &str) -> Result<Value> {
        let url = format!(
            "{}/nodes/{}/databases/{}/query",
            self.api_engine(),
            remote.node_id,
            remote.cp_database_id()
        );
        self.post_json(&url, &serde_json::json!({ "sql": sql }))
            .await
    }

    pub async fn schema_show(&self, remote: &RemoteConfig) -> Result<Value> {
        let url = format!(
            "{}/nodes/{}/databases/{}/schema/show",
            self.api_engine(),
            remote.node_id,
            remote.cp_database_id()
        );
        self.post_json(&url, &serde_json::json!({})).await
    }

    pub async fn schema_diff(
        &self,
        remote: &RemoteConfig,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<Value> {
        let url = format!(
            "{}/nodes/{}/databases/{}/schema/diff",
            self.api_engine(),
            remote.node_id,
            remote.cp_database_id()
        );
        let mut body = serde_json::json!({});
        if let Some(f) = from {
            body["from"] = Value::String(f.to_string());
        }
        if let Some(t) = to {
            body["to"] = Value::String(t.to_string());
        }
        self.post_json(&url, &body).await
    }
}

pub fn credentials_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .context("cannot determine home directory")?;
    Ok(home.join(".config").join("guepard").join("credentials.toml"))
}

pub fn load_credentials_file() -> Option<CredentialsFile> {
    let path = credentials_path().ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    toml::from_str(&content).ok()
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

pub fn merge_env_into_credentials(file: &mut CredentialsFile) {
    if let Ok(url) = std::env::var("GUEPARD_CONSOLE_URL") {
        if !url.trim().is_empty() {
            file.console_url = Some(url.trim_end_matches('/').to_string());
        }
    }
    if let Ok(url) = std::env::var("GUEPARD_SUPABASE_URL") {
        if !url.trim().is_empty() {
            file.supabase_url = Some(url.trim_end_matches('/').to_string());
        }
    }
    if let Ok(key) = std::env::var("GUEPARD_SUPABASE_ANON_KEY") {
        if !key.trim().is_empty() {
            file.supabase_anon_key = Some(key.trim().to_string());
        }
    }
}

pub fn resolve_console_url() -> Result<String> {
    if let Ok(url) = std::env::var("GUEPARD_CONSOLE_URL") {
        if !url.trim().is_empty() {
            return Ok(url.trim_end_matches('/').to_string());
        }
    }
    if let Some(file) = load_credentials_file() {
        if let Some(url) = file.console_url.filter(|s| !s.trim().is_empty()) {
            return Ok(url.trim_end_matches('/').to_string());
        }
    }
    bail!(
        "console URL not set: export GUEPARD_CONSOLE_URL or run `gfs config --global remote.console_url <url>`"
    )
}

pub struct SupabaseConfig {
    pub url: String,
    pub anon_key: String,
}

pub fn resolve_supabase_config() -> Result<SupabaseConfig> {
    let url = std::env::var("GUEPARD_SUPABASE_URL")
        .or_else(|_| std::env::var("VITE_SUPABASE_URL"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            load_credentials_file().and_then(|f| {
                f.supabase_url
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.trim_end_matches('/').to_string())
            })
        })
        .context("set GUEPARD_SUPABASE_URL or `gfs config --global remote.supabase_url`")?;

    let anon_key = std::env::var("GUEPARD_SUPABASE_ANON_KEY")
        .or_else(|_| std::env::var("VITE_SUPABASE_ANON_KEY"))
        .or_else(|_| std::env::var("SUPABASE_ANON_KEY"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            load_credentials_file().and_then(|f| {
                f.supabase_anon_key
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string())
            })
        })
        .context("set GUEPARD_SUPABASE_ANON_KEY or `gfs config --global remote.supabase_anon_key`")?;

    Ok(SupabaseConfig {
        url: url.trim_end_matches('/').to_string(),
        anon_key,
    })
}

pub fn login_with_token(token: &str) -> Result<()> {
    let mut file = load_credentials_file().unwrap_or_default();
    file.access_token = Some(token.trim().to_string());
    merge_env_into_credentials(&mut file);
    save_credentials(&file)
}

pub fn set_remote_config_value(key: &str, value: &str) -> Result<()> {
    let mut file = load_credentials_file().unwrap_or_default();
    match key {
        "remote.console_url" => file.console_url = Some(value.trim_end_matches('/').to_string()),
        "remote.supabase_url" => file.supabase_url = Some(value.trim_end_matches('/').to_string()),
        "remote.supabase_anon_key" => file.supabase_anon_key = Some(value.to_string()),
        _ => bail!("unsupported remote config key: {key}"),
    }
    save_credentials(&file)
}

pub fn get_remote_config_value(key: &str) -> Result<Option<String>> {
    match key {
        "remote.console_url" => Ok(resolve_console_url().ok()),
        "remote.supabase_url" => Ok(resolve_supabase_config().ok().map(|c| c.url)),
        "remote.supabase_anon_key" => Ok(resolve_supabase_config()
            .ok()
            .map(|c| c.anon_key)),
        _ => bail!("unsupported remote config key: {key}"),
    }
}

pub fn remote_config_show() -> Result<CredentialsFile> {
    let mut file = load_credentials_file().unwrap_or_default();
    merge_env_into_credentials(&mut file);
    if file.access_token.is_some() {
        file.access_token = Some("<set>".to_string());
    }
    Ok(file)
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
    let mut file = CredentialsFile {
        access_token: Some(parsed.access_token),
        supabase_url: Some(supabase_url.trim_end_matches('/').to_string()),
        supabase_anon_key: Some(anon_key.to_string()),
        ..Default::default()
    };
    merge_env_into_credentials(&mut file);
    save_credentials(&file)?;
    Ok(())
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
