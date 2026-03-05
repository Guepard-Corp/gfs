//! MCP tool implementations: thin adapter over domain use cases.

use std::path::PathBuf;
use std::sync::Arc;

use gfs_compute_docker::DockerCompute;
use gfs_compute_docker::containers;
use gfs_domain::adapters::gfs_repository::GfsRepository;
use gfs_domain::model::config::{GfsConfig, RuntimeConfig};
use gfs_domain::model::datasource::diff::compute_schema_diff;
use gfs_domain::model::datasource::diff_formatter::JsonFormatter;
use gfs_domain::ports::compute::{Compute, InstanceId, InstanceState, InstanceStatus, LogsOptions};
use gfs_domain::ports::database_provider::{
    ConnectionParams, DatabaseProviderRegistry, InMemoryDatabaseProviderRegistry,
};
use gfs_domain::ports::repository::{LogOptions, Repository};
use gfs_domain::repo_utils::repo_layout;
use gfs_domain::usecases::repository::{
    checkout_repo_usecase::CheckoutRepoUseCase, commit_repo_usecase::CommitRepoUseCase,
    export_repo_usecase::ExportRepoUseCase, extract_schema_usecase::ExtractSchemaUseCase,
    import_repo_usecase::ImportRepoUseCase, init_repo_usecase::InitRepositoryUseCase,
    log_repo_usecase::LogRepoUseCase, status_repo_usecase::StatusRepoUseCase,
};
use gfs_telemetry::TelemetryClient;
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde_json::json;

fn to_error_data(msg: impl Into<std::borrow::Cow<'static, str>>) -> McpError {
    McpError::internal_error(msg, None)
}

/// Default repo path: env GFS_REPO_PATH or current directory.
fn default_repo_path() -> PathBuf {
    std::env::var("GFS_REPO_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().expect("current directory not available"))
}

fn repo_path_from_value(value: &serde_json::Value) -> PathBuf {
    value
        .as_object()
        .and_then(|o| o.get("path"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(default_repo_path)
}

fn json_ok(value: serde_json::Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
    )]))
}

fn json_err(message: &str, code: Option<&str>) -> Result<CallToolResult, McpError> {
    let mut obj = json!({ "message": message });
    if let Some(c) = code {
        obj["code"] = json!(c);
    }
    Ok(CallToolResult::error(vec![Content::text(
        serde_json::to_string(&obj).unwrap_or_else(|_| message.to_string()),
    )]))
}

// --- Request structs for each tool ---

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct ListProvidersRequest {}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct StatusRequest {
    #[schemars(description = "repo root path")]
    pub path: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CommitRequest {
    #[schemars(description = "commit message")]
    pub message: String,
    #[schemars(description = "repo root path")]
    pub path: Option<String>,
    pub author: Option<String>,
    pub author_email: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct LogRequest {
    #[schemars(description = "repo root path")]
    pub path: Option<String>,
    #[schemars(description = "max number of commits")]
    pub max_count: Option<u64>,
    #[schemars(description = "from revision")]
    pub from: Option<String>,
    #[schemars(description = "until revision")]
    pub until: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct CheckoutRequest {
    #[schemars(description = "branch or 64-char commit hash")]
    pub revision: Option<String>,
    #[schemars(description = "new branch name when creating")]
    pub create_branch: Option<String>,
    #[schemars(description = "repo root path")]
    pub path: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct InitRequest {
    #[schemars(description = "repo root path")]
    pub path: Option<String>,
    #[schemars(description = "database provider e.g. postgres, mysql")]
    pub database_provider: Option<String>,
    #[schemars(
        description = "database version e.g. 17 for postgres, 8.0 for mysql; required when database_provider is set"
    )]
    pub database_version: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ComputeRequest {
    #[schemars(description = "action: status, start, stop, restart, pause, unpause, logs")]
    pub action: String,
    #[schemars(description = "repo root path")]
    pub path: Option<String>,
    #[schemars(description = "container id override")]
    pub id: Option<String>,
    pub logs_tail: Option<u64>,
    pub logs_since: Option<String>,
    pub logs_no_stdout: Option<bool>,
    pub logs_no_stderr: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ExportRequest {
    #[schemars(description = "repo root path")]
    pub path: Option<String>,
    #[schemars(description = "host directory where the export file will be written")]
    pub output_dir: Option<String>,
    #[schemars(description = "export format: sql or custom")]
    pub format: String,
    #[schemars(description = "container id override")]
    pub id: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ImportRequest {
    #[schemars(description = "repo root path")]
    pub path: Option<String>,
    #[schemars(description = "absolute path to the dump file to import")]
    pub file: String,
    #[schemars(
        description = "import format: sql or custom; inferred from file extension when omitted"
    )]
    pub format: Option<String>,
    #[schemars(description = "container id override")]
    pub id: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct QueryRequest {
    #[schemars(description = "repo root path")]
    pub path: Option<String>,
    #[schemars(description = "database name to query (overrides default from container config)")]
    pub database: Option<String>,
    #[schemars(
        description = "SQL query to execute. Omit to return connection info for interactive use."
    )]
    pub query: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct ExtractSchemaRequest {
    #[schemars(description = "repo root path")]
    pub path: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ShowSchemaRequest {
    #[schemars(description = "commit hash or reference (HEAD, main, etc.)")]
    pub commit: String,
    #[schemars(description = "repo root path")]
    pub path: Option<String>,
    #[schemars(description = "return only metadata (JSON), not DDL")]
    pub metadata_only: Option<bool>,
    #[schemars(description = "return only DDL (SQL), not metadata")]
    pub ddl_only: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DiffSchemaRequest {
    #[schemars(description = "first commit hash or reference")]
    pub commit1: String,
    #[schemars(description = "second commit hash or reference")]
    pub commit2: String,
    #[schemars(description = "repo root path")]
    pub path: Option<String>,
}

// --- Telemetry source for MCP: detect cursor/claude_code/ci, fallback to "mcp" ---
fn mcp_source() -> &'static str {
    let s = gfs_telemetry::detect_source();
    if s == "cli" { "mcp" } else { s }
}

// --- Handler ---

#[derive(Debug, Clone)]
pub struct GfsMcpHandler {
    tool_router: ToolRouter<Self>,
    telemetry: TelemetryClient,
}

impl Default for GfsMcpHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl GfsMcpHandler {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            telemetry: TelemetryClient::new(),
        }
    }

    #[tool(
        description = "List supported database providers (e.g. postgres, mysql) and their versions and features. Use when choosing or checking which databases this GFS server can run. Equivalent to gfs providers."
    )]
    async fn list_providers(
        &self,
        _: Parameters<ListProvidersRequest>,
    ) -> Result<CallToolResult, McpError> {
        let result = do_list_providers().await;
        self.track_mcp("list_providers", &result);
        result
    }

    #[tool(
        description = "Return the current state of the GFS repository and its compute instance (database container). Includes repository branch/HEAD and database container status, connection string when running. Optional: path (string) - repo root. Equivalent to gfs status."
    )]
    async fn status(
        &self,
        Parameters(req): Parameters<StatusRequest>,
    ) -> Result<CallToolResult, McpError> {
        let args = json!({
            "path": req.path,
        });
        let result = do_status(&args).await;
        self.track_mcp("status", &result);
        result
    }

    #[tool(
        description = "Create a new commit in the database-backed repository. Required: message (string). Optional: path, author, author_email. Equivalent to gfs commit -m <message>."
    )]
    async fn commit(
        &self,
        Parameters(req): Parameters<CommitRequest>,
    ) -> Result<CallToolResult, McpError> {
        let args = json!({
            "message": req.message,
            "path": req.path,
            "author": req.author,
            "author_email": req.author_email,
        });
        let result = do_commit(&args).await;
        self.track_mcp("commit", &result);
        result
    }

    #[tool(
        description = "Return commit history from the repository (database-backed). Optional: path, max_count (number), from (revision), until (revision). Equivalent to gfs log."
    )]
    async fn log(
        &self,
        Parameters(req): Parameters<LogRequest>,
    ) -> Result<CallToolResult, McpError> {
        let args = json!({
            "path": req.path,
            "max_count": req.max_count,
            "from": req.from,
            "until": req.until,
        });
        let result = do_log(&args).await;
        self.track_mcp("log", &result);
        result
    }

    #[tool(
        description = "Switch branch or checkout commit in the database-backed repository. Required: revision (branch or 64-char hash). Optional: path, create_branch (new branch name). Equivalent to gfs checkout."
    )]
    async fn checkout(
        &self,
        Parameters(req): Parameters<CheckoutRequest>,
    ) -> Result<CallToolResult, McpError> {
        let args = json!({
            "revision": req.revision,
            "create_branch": req.create_branch,
            "path": req.path,
        });
        let result = do_checkout(&args).await;
        self.track_mcp("checkout", &result);
        result
    }

    #[tool(
        description = "Initialize a new GFS repository backed by a database. Optional: path. If database_provider is set (e.g. postgres, mysql), database_version is required (e.g. 17 for postgres). Creates repo metadata and can start the database container. Equivalent to gfs init."
    )]
    async fn init(
        &self,
        Parameters(req): Parameters<InitRequest>,
    ) -> Result<CallToolResult, McpError> {
        let args = json!({
            "path": req.path,
            "database_provider": req.database_provider,
            "database_version": req.database_version,
        });
        let result = do_init(&args).await;
        self.track_mcp("init", &result);
        result
    }

    #[tool(
        description = "Database compute lifecycle: status, start, stop, restart, pause, unpause, logs for the database container. Required: action (string). Optional: path, id (container), logs_tail, logs_since, logs_no_stdout, logs_no_stderr. Equivalent to gfs compute <action>."
    )]
    async fn compute(
        &self,
        Parameters(req): Parameters<ComputeRequest>,
    ) -> Result<CallToolResult, McpError> {
        let args = json!({
            "action": req.action,
            "path": req.path,
            "id": req.id,
            "logs_tail": req.logs_tail,
            "logs_since": req.logs_since,
            "logs_no_stdout": req.logs_no_stdout,
            "logs_no_stderr": req.logs_no_stderr,
        });
        let result = do_compute(&args).await;
        self.track_mcp("compute", &result);
        result
    }

    #[tool(
        description = "Export data from the running database instance to a file on the host. Required: format (sql or custom). Optional: path (repo root), output_dir (defaults to current directory), id (container override). Returns the path of the exported file. Equivalent to gfs export."
    )]
    async fn export_database(
        &self,
        Parameters(req): Parameters<ExportRequest>,
    ) -> Result<CallToolResult, McpError> {
        let args = json!({
            "path": req.path,
            "output_dir": req.output_dir,
            "format": req.format,
            "id": req.id,
        });
        let result = do_export(&args).await;
        self.track_mcp("export", &result);
        result
    }

    #[tool(
        description = "Import data into the running database instance from a file on the host. Supports multiple formats including SQL dumps, CSV, JSON, and custom database-specific formats. Required: file (path to data file). Optional: path (repo root), format (sql, csv, json, custom, etc.; inferred from extension when omitted), id (container override). Equivalent to gfs import."
    )]
    async fn import_database(
        &self,
        Parameters(req): Parameters<ImportRequest>,
    ) -> Result<CallToolResult, McpError> {
        let args = json!({
            "path": req.path,
            "file": req.file,
            "format": req.format,
            "id": req.id,
        });
        let result = do_import(&args).await;
        self.track_mcp("import", &result);
        result
    }

    #[tool(
        description = "Execute a SQL query against the running database instance. Returns query results as text output. Optional: path (repo root), database (name to query), query (SQL statement; if omitted, returns connection info). Note: interactive terminal mode is not supported via MCP. Equivalent to gfs query \"<sql>\"."
    )]
    async fn query(
        &self,
        Parameters(req): Parameters<QueryRequest>,
    ) -> Result<CallToolResult, McpError> {
        let args = json!({
            "path": req.path,
            "database": req.database,
            "query": req.query,
        });
        let result = do_query(&args).await;
        self.track_mcp("query", &result);
        result
    }

    #[tool(
        description = "Extract database schema metadata from the running database instance. Returns complete schema including schemas, tables, columns, constraints, and relationships as structured JSON. Use this to understand the database structure before writing queries or making changes. Optional: path (repo root). Equivalent to gfs schema extract."
    )]
    async fn extract_schema(
        &self,
        Parameters(req): Parameters<ExtractSchemaRequest>,
    ) -> Result<CallToolResult, McpError> {
        let args = json!({ "path": req.path });
        let result = do_extract_schema(&args).await;
        self.track_mcp("extract_schema", &result);
        result
    }

    #[tool(
        description = "Show schema from a specific commit. View the database schema as it existed at any point in history. Returns both structured metadata (JSON) and native DDL (SQL). Use metadata_only or ddl_only flags to filter output. Required: commit (hash or ref like HEAD, main). Optional: path, metadata_only, ddl_only. Equivalent to gfs schema show."
    )]
    async fn show_schema(
        &self,
        Parameters(req): Parameters<ShowSchemaRequest>,
    ) -> Result<CallToolResult, McpError> {
        let args = json!({
            "commit": req.commit,
            "path": req.path,
            "metadata_only": req.metadata_only,
            "ddl_only": req.ddl_only,
        });
        let result = do_show_schema(&args).await;
        self.track_mcp("show_schema", &result);
        result
    }

    #[tool(
        description = "Compare schemas between two commits. Track schema evolution by comparing table counts, column counts, and DDL changes. Returns schema hashes, difference summary, and change counts. Required: commit1, commit2 (hashes or refs). Optional: path. Use this before merging branches to review schema changes. Equivalent to gfs schema diff."
    )]
    async fn diff_schema(
        &self,
        Parameters(req): Parameters<DiffSchemaRequest>,
    ) -> Result<CallToolResult, McpError> {
        let args = json!({
            "commit1": req.commit1,
            "commit2": req.commit2,
            "path": req.path,
        });
        let result = do_diff_schema(&args).await;
        self.track_mcp("diff_schema", &result);
        result
    }
}

impl GfsMcpHandler {
    /// Track a tool invocation. Uses `"mcp"` as source (or `"cursor"`/`"claude_code"` if detected).
    fn track_mcp(&self, command: &'static str, result: &Result<CallToolResult, McpError>) {
        let source = mcp_source();
        let version = env!("CARGO_PKG_VERSION");
        let os = std::env::consts::OS;
        match result {
            Ok(_) => {
                self.telemetry.track(
                    "command_executed",
                    vec![
                        ("command", json!(command)),
                        ("source", json!(source)),
                        ("version", json!(version)),
                        ("os", json!(os)),
                    ],
                );
            }
            Err(_) => {
                self.telemetry.track(
                    "command_failed",
                    vec![
                        ("command", json!(command)),
                        ("source", json!(source)),
                        ("version", json!(version)),
                        ("os", json!(os)),
                        ("error_category", json!("McpError")),
                    ],
                );
            }
        }
    }
}

#[tool_handler]
impl ServerHandler for GfsMcpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "GFS MCP server. Tools: list_providers, status, commit, log, checkout, init, compute, export_database, import_database, query, extract_schema, show_schema, diff_schema. \
                 Schema versioning: commits automatically capture database schemas. Use show_schema to view schema at any commit, diff_schema to compare schema evolution. \
                 Use path to target a repo or set GFS_REPO_PATH."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "gfs-mcp".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

// --- Internal helpers (same logic as before) ---

async fn do_list_providers() -> Result<CallToolResult, McpError> {
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(registry.as_ref())
        .map_err(|e| to_error_data(format!("failed to register database providers: {e}")))?;

    let names = registry.list();
    let providers: Vec<serde_json::Value> = names
        .into_iter()
        .filter_map(|name| {
            let provider = registry.get(&name)?;
            let versions = provider.supported_versions();
            let features: Vec<String> = provider
                .supported_features()
                .iter()
                .map(|f| f.id.clone())
                .collect();
            Some(json!({
                "database_provider": name,
                "versions": versions,
                "features": features,
            }))
        })
        .collect();

    json_ok(json!({ "providers": providers }))
}

async fn do_status(args: &serde_json::Value) -> Result<CallToolResult, McpError> {
    let args = if args.is_object() { args } else { &json!({}) };
    let repo_path = repo_path_from_value(args);

    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute =
        Arc::new(DockerCompute::new().map_err(|e| to_error_data(format!("Docker: {e}")))?);
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(registry.as_ref())
        .map_err(|e| to_error_data(format!("register providers: {e}")))?;

    let use_case = StatusRepoUseCase::new(repository, compute, registry);
    let status = use_case
        .run(&repo_path)
        .await
        .map_err(|e| to_error_data(e.to_string()))?;

    json_ok(json!({
        "current_branch": status.current_branch,
        "compute": status.compute.map(|c| json!({
            "container_id": c.container_id,
            "container_status": c.container_status,
            "connection_string": c.connection_string,
        })),
    }))
}

async fn do_commit(args: &serde_json::Value) -> Result<CallToolResult, McpError> {
    let args = if !args.is_object() {
        return json_err("missing arguments: message required", Some("MISSING_ARGS"));
    } else {
        args
    };
    let message = args
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if message.is_empty() {
        return json_err("commit message must be non-empty", Some("INVALID_INPUT"));
    }
    let repo_path = repo_path_from_value(args);
    let author = args
        .get("author")
        .and_then(|v| v.as_str())
        .map(String::from);
    let author_email = args
        .get("author_email")
        .and_then(|v| v.as_str())
        .map(String::from);

    #[cfg(target_os = "macos")]
    {
        use gfs_domain::ports::storage::StoragePort;
        let storage: Arc<dyn StoragePort> = Arc::new(gfs_storage_apfs::ApfsStorage::new());
        let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
        let compute: Arc<dyn Compute> =
            Arc::new(DockerCompute::new().map_err(|e| to_error_data(format!("Docker: {e}")))?);
        let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
        containers::register_all(registry.as_ref())
            .map_err(|e| to_error_data(format!("register providers: {e}")))?;
        let use_case = CommitRepoUseCase::new(repository.clone(), compute, storage, registry);
        let branch = repository
            .get_current_branch(&repo_path)
            .await
            .unwrap_or_else(|_| "HEAD".to_string());
        let commit_hash = use_case
            .run(
                repo_path,
                message.to_string(),
                author,
                author_email,
                None,
                None,
            )
            .await
            .map_err(|e| to_error_data(e.to_string()))?;
        json_ok(json!({
            "branch": branch,
            "commit_id": commit_hash,
            "message": message,
        }))
    }

    #[cfg(not(target_os = "macos"))]
    {
        use gfs_domain::ports::storage::StoragePort;
        let storage: Arc<dyn StoragePort> = Arc::new(gfs_storage_file::FileStorage::new());
        let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
        let compute: Arc<dyn Compute> =
            Arc::new(DockerCompute::new().map_err(|e| to_error_data(format!("Docker: {e}")))?);
        let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
        containers::register_all(registry.as_ref())
            .map_err(|e| to_error_data(format!("register providers: {e}")))?;
        let use_case = CommitRepoUseCase::new(repository.clone(), compute, storage, registry);
        let branch = repository
            .get_current_branch(&repo_path)
            .await
            .unwrap_or_else(|_| "HEAD".to_string());
        let commit_hash = use_case
            .run(
                repo_path,
                message.to_string(),
                author,
                author_email,
                None,
                None,
            )
            .await
            .map_err(|e| to_error_data(e.to_string()))?;
        json_ok(json!({
            "branch": branch,
            "commit_id": commit_hash,
            "message": message,
        }))
    }
}

async fn do_log(args: &serde_json::Value) -> Result<CallToolResult, McpError> {
    let args = if args.is_object() { args } else { &json!({}) };
    let repo_path = repo_path_from_value(args);
    let max_count = args
        .get("max_count")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let from = args.get("from").and_then(|v| v.as_str()).map(String::from);
    let until = args.get("until").and_then(|v| v.as_str()).map(String::from);

    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let use_case = LogRepoUseCase::new(repository);
    let options = LogOptions {
        from,
        until,
        limit: max_count,
    };
    let commits = use_case
        .run(repo_path, options)
        .await
        .map_err(|e| to_error_data(e.to_string()))?;

    let list: Vec<serde_json::Value> = commits
        .iter()
        .map(|cwr| {
            let c = &cwr.commit;
            json!({
                "id": c.hash,
                "message": c.message,
                "author": c.author,
                "author_email": c.author_email,
                "author_date": c.author_date.to_rfc3339(),
                "refs": cwr.refs,
            })
        })
        .collect();
    json_ok(json!({ "commits": list }))
}

async fn do_checkout(args: &serde_json::Value) -> Result<CallToolResult, McpError> {
    let args = if args.is_object() { args } else { &json!({}) };
    let revision: Option<String> = args
        .get("revision")
        .and_then(|v| v.as_str())
        .map(String::from);
    let create_branch: Option<String> = args
        .get("create_branch")
        .and_then(|v| v.as_str())
        .map(String::from);

    let (revision, create_branch): (String, Option<String>) = match (&revision, &create_branch) {
        (Some(r), None) => (r.clone(), None),
        (None, Some(b)) => (String::new(), Some(b.clone())),
        (Some(r), Some(b)) => (r.clone(), Some(b.clone())),
        (None, None) => {
            return json_err(
                "revision required or use create_branch",
                Some("MISSING_ARGS"),
            );
        }
    };

    let repo_path = repo_path_from_value(args);
    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute: Arc<dyn Compute> =
        Arc::new(DockerCompute::new().map_err(|e| to_error_data(format!("Docker: {e}")))?);
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(registry.as_ref())
        .map_err(|e| to_error_data(format!("register providers: {e}")))?;
    let use_case = CheckoutRepoUseCase::new(repository, compute, registry);
    let commit_hash = use_case
        .run(repo_path, revision.clone(), create_branch.clone())
        .await
        .map_err(|e| to_error_data(e.to_string()))?;

    json_ok(json!({
        "revision": revision.trim(),
        "create_branch": create_branch,
        "commit_id": commit_hash,
    }))
}

async fn do_init(args: &serde_json::Value) -> Result<CallToolResult, McpError> {
    let args = if args.is_object() { args } else { &json!({}) };
    let repo_path = repo_path_from_value(args);
    let database_provider = args
        .get("database_provider")
        .and_then(|v| v.as_str())
        .map(String::from);
    let database_version = args
        .get("database_version")
        .and_then(|v| v.as_str())
        .map(String::from);

    let repository: Arc<dyn Repository> = Arc::new(GfsRepository::new());
    let compute =
        Arc::new(DockerCompute::new().map_err(|e| to_error_data(format!("Docker: {e}")))?);
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(registry.as_ref())
        .map_err(|e| to_error_data(format!("register providers: {e}")))?;

    let use_case = InitRepositoryUseCase::new(repository, compute, registry);
    use_case
        .run(
            repo_path.clone(),
            None,
            database_provider.clone(),
            database_version.clone(),
        )
        .await
        .map_err(|e| to_error_data(e.to_string()))?;

    json_ok(json!({
        "path": repo_path.display().to_string(),
        "database_provider": database_provider,
        "database_version": database_version,
    }))
}

async fn do_compute(args: &serde_json::Value) -> Result<CallToolResult, McpError> {
    let args = if args.is_object() { args } else { &json!({}) };
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| to_error_data("missing argument: action required"))?;
    let repo_path = repo_path_from_value(args);
    let id_override = args.get("id").and_then(|v| v.as_str()).map(String::from);

    let id = match id_override {
        Some(id) => id,
        None => {
            let config = GfsConfig::load(&repo_path)
                .map_err(|e| to_error_data(format!("not a GFS repository: {e}")))?;
            let name = config
                .runtime
                .as_ref()
                .map(|r| r.container_name.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    to_error_data(
                        "no container_name in repo config (set runtime.container_name or pass id)",
                    )
                })?;
            name.to_string()
        }
    };

    let compute = DockerCompute::new().map_err(|e| to_error_data(format!("Docker: {e}")))?;
    let instance_id = InstanceId(id);

    let result = match action {
        "status" => {
            let status = compute
                .status(&instance_id)
                .await
                .map_err(|e| to_error_data(e.to_string()))?;
            json!({
                "id": status.id.0,
                "state": format_instance_state(&status.state),
                "pid": status.pid,
                "started_at": status.started_at.map(|t| t.to_rfc3339()),
                "exit_code": status.exit_code,
            })
        }
        "start" => {
            let (_, status) = start_or_restart(&compute, &instance_id, &repo_path, false).await?;
            json!({
                "id": status.id.0,
                "state": format_instance_state(&status.state),
            })
        }
        "stop" => {
            let status = compute
                .stop(&instance_id)
                .await
                .map_err(|e| to_error_data(e.to_string()))?;
            json!({
                "id": status.id.0,
                "state": format_instance_state(&status.state),
            })
        }
        "restart" => {
            let (_, status) = start_or_restart(&compute, &instance_id, &repo_path, true).await?;
            json!({
                "id": status.id.0,
                "state": format_instance_state(&status.state),
            })
        }
        "pause" => {
            let status = compute
                .pause(&instance_id)
                .await
                .map_err(|e| to_error_data(e.to_string()))?;
            json!({
                "id": status.id.0,
                "state": format_instance_state(&status.state),
            })
        }
        "unpause" => {
            let status = compute
                .unpause(&instance_id)
                .await
                .map_err(|e| to_error_data(e.to_string()))?;
            json!({
                "id": status.id.0,
                "state": format_instance_state(&status.state),
            })
        }
        "logs" => {
            let tail = args
                .get("logs_tail")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize);
            let since_str = args.get("logs_since").and_then(|v| v.as_str());
            let since = since_str
                .map(|s| {
                    chrono::DateTime::parse_from_rfc3339(s)
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .map_err(|e| to_error_data(format!("invalid logs_since: {e}")))
                })
                .transpose()?;
            let stdout = args
                .get("logs_no_stdout")
                .and_then(|v| v.as_bool())
                .map(|b| !b)
                .unwrap_or(true);
            let stderr = args
                .get("logs_no_stderr")
                .and_then(|v| v.as_bool())
                .map(|b| !b)
                .unwrap_or(true);
            let options = LogsOptions {
                tail,
                since,
                stdout,
                stderr,
            };
            let entries = compute
                .logs(&instance_id, options)
                .await
                .map_err(|e| to_error_data(e.to_string()))?;
            let lines: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    json!({
                        "timestamp": e.timestamp.to_rfc3339(),
                        "stream": format!("{:?}", e.stream).to_lowercase(),
                        "message": e.message.trim_end(),
                    })
                })
                .collect();
            json!({ "entries": lines })
        }
        _ => {
            return json_err(
                &format!(
                    "unknown action: {} (use status, start, stop, restart, pause, unpause, logs)",
                    action
                ),
                Some("INVALID_INPUT"),
            );
        }
    };

    json_ok(result)
}

fn format_instance_state(s: &InstanceState) -> &'static str {
    match s {
        InstanceState::Starting => "starting",
        InstanceState::Running => "running",
        InstanceState::Paused => "paused",
        InstanceState::Stopping => "stopping",
        InstanceState::Stopped => "stopped",
        InstanceState::Restarting => "restarting",
        InstanceState::Failed => "failed",
        InstanceState::Unknown => "unknown",
    }
}

async fn start_or_restart(
    compute: &DockerCompute,
    instance_id: &InstanceId,
    repo_path: &std::path::Path,
    restart: bool,
) -> Result<(InstanceId, InstanceStatus), McpError> {
    let active = match repo_layout::get_active_workspace_data_dir(repo_path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => return just_start_or_restart(compute, instance_id, restart).await,
    };
    let config = match GfsConfig::load(repo_path) {
        Ok(c) => c,
        Err(_) => return just_start_or_restart(compute, instance_id, restart).await,
    };
    let provider_name = match &config.environment {
        Some(e) if !e.database_provider.is_empty() => e.database_provider.as_str(),
        _ => return just_start_or_restart(compute, instance_id, restart).await,
    };
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(registry.as_ref())
        .map_err(|e| to_error_data(format!("register providers: {e}")))?;
    let provider = registry
        .get(provider_name)
        .ok_or_else(|| to_error_data(format!("unknown database provider: {}", provider_name)))?;
    let compute_data_path = provider
        .definition()
        .data_dir
        .to_string_lossy()
        .into_owned();
    let current_bind = match compute
        .get_instance_data_mount_host_path(instance_id, &compute_data_path)
        .await
    {
        Ok(Some(p)) => p.to_string_lossy().into_owned(),
        _ => return just_start_or_restart(compute, instance_id, restart).await,
    };
    if paths_differ(&active, &current_bind) {
        compute
            .stop(instance_id)
            .await
            .map_err(|e| to_error_data(e.to_string()))?;
        compute
            .remove_instance(instance_id)
            .await
            .map_err(|e| to_error_data(e.to_string()))?;
        let mut definition = provider.definition();
        if let Some(ref env) = config.environment
            && !env.database_version.is_empty()
        {
            let base = definition
                .image
                .split(':')
                .next()
                .unwrap_or(&definition.image);
            definition.image = format!("{}:{}", base, env.database_version);
        }
        definition.host_data_dir = Some(std::path::PathBuf::from(&active));
        let new_id = compute
            .provision(&definition)
            .await
            .map_err(|e| to_error_data(e.to_string()))?;
        let status = compute
            .start(&new_id, Default::default())
            .await
            .map_err(|e| to_error_data(e.to_string()))?;
        repo_layout::update_runtime_config(
            repo_path,
            RuntimeConfig {
                runtime_provider: "docker".to_string(),
                runtime_version: "24".to_string(),
                container_name: new_id.0.clone(),
            },
        )
        .map_err(|e| to_error_data(e.to_string()))?;
        return Ok((new_id, status));
    }
    just_start_or_restart(compute, instance_id, restart).await
}

async fn just_start_or_restart(
    compute: &DockerCompute,
    instance_id: &InstanceId,
    restart: bool,
) -> Result<(InstanceId, InstanceStatus), McpError> {
    let status = if restart {
        compute
            .restart(instance_id)
            .await
            .map_err(|e| to_error_data(e.to_string()))?
    } else {
        compute
            .start(instance_id, Default::default())
            .await
            .map_err(|e| to_error_data(e.to_string()))?
    };
    Ok((instance_id.clone(), status))
}

fn paths_differ(a: &str, b: &str) -> bool {
    let a = std::path::Path::new(a);
    let b = std::path::Path::new(b);
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a != b,
        _ => a != b,
    }
}

async fn do_export(args: &serde_json::Value) -> Result<CallToolResult, McpError> {
    let args = if args.is_object() { args } else { &json!({}) };
    let repo_path = repo_path_from_value(args);

    let format = args
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if format.is_empty() {
        return json_err(
            "format is required (e.g. sql, custom)",
            Some("MISSING_ARGS"),
        );
    }

    let output_dir = args
        .get("output_dir")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(default_repo_path);

    let compute =
        Arc::new(DockerCompute::new().map_err(|e| to_error_data(format!("Docker: {e}")))?);
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(registry.as_ref())
        .map_err(|e| to_error_data(format!("register providers: {e}")))?;

    let use_case = ExportRepoUseCase::new(compute, registry);
    let output = use_case
        .run(&repo_path, output_dir, format)
        .await
        .map_err(|e| to_error_data(e.to_string()))?;

    json_ok(json!({
        "file_path": output.file_path.display().to_string(),
        "format": output.format,
        "stdout": output.stdout,
    }))
}

async fn do_import(args: &serde_json::Value) -> Result<CallToolResult, McpError> {
    let args = if args.is_object() { args } else { &json!({}) };
    let repo_path = repo_path_from_value(args);

    let file_str = args
        .get("file")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| to_error_data("file is required"))?;

    let input_file = PathBuf::from(file_str);
    let format = args
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let compute =
        Arc::new(DockerCompute::new().map_err(|e| to_error_data(format!("Docker: {e}")))?);
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(registry.as_ref())
        .map_err(|e| to_error_data(format!("register providers: {e}")))?;

    let use_case = ImportRepoUseCase::new(compute, registry);
    let output = use_case
        .run(&repo_path, input_file, &format)
        .await
        .map_err(|e| to_error_data(e.to_string()))?;

    json_ok(json!({
        "imported_from": output.imported_from.display().to_string(),
        "format": output.format,
        "stdout": output.stdout,
    }))
}

async fn do_query(args: &serde_json::Value) -> Result<CallToolResult, McpError> {
    let args = if args.is_object() { args } else { &json!({}) };
    let repo_path = repo_path_from_value(args);
    let query = args.get("query").and_then(|v| v.as_str()).map(String::from);
    let database = args
        .get("database")
        .and_then(|v| v.as_str())
        .map(String::from);

    // Load config to get provider name and container name
    let config = GfsConfig::load(&repo_path)
        .map_err(|e| to_error_data(format!("not a GFS repository: {e}")))?;

    let environment = config.environment.as_ref().ok_or_else(|| {
        to_error_data("no database configured (run init with --database-provider)")
    })?;

    let runtime = config
        .runtime
        .as_ref()
        .ok_or_else(|| to_error_data("no runtime configured"))?;

    let provider_name = &environment.database_provider;
    let container_name = &runtime.container_name;

    // Set up compute and registry
    let compute =
        Arc::new(DockerCompute::new().map_err(|e| to_error_data(format!("Docker: {e}")))?);

    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(registry.as_ref())
        .map_err(|e| to_error_data(format!("register providers: {e}")))?;

    // Get the provider
    let provider = registry
        .get(provider_name)
        .ok_or_else(|| to_error_data(format!("unknown database provider: {}", provider_name)))?;

    // Get connection info from the running container
    let instance_id = InstanceId(container_name.clone());
    let default_port = provider.default_port();

    let conn_info = compute
        .get_connection_info(&instance_id, default_port)
        .await
        .map_err(|e| {
            to_error_data(format!(
                "failed to get connection info (is the database running?): {e}"
            ))
        })?;

    // Override database name if provided
    let mut env = conn_info.env.clone();
    if let Some(db_name) = database.clone() {
        // Determine the database environment variable based on provider
        let db_env_var = match provider_name.as_str() {
            "postgres" => "POSTGRES_DB",
            "mysql" => "MYSQL_DATABASE",
            _ => "DATABASE", // fallback for future providers
        };

        // Remove existing database env var and add the override
        env.retain(|(k, _)| k != db_env_var);
        env.push((db_env_var.to_string(), db_name));
    }

    let params = ConnectionParams {
        host: conn_info.host.clone(),
        port: conn_info.port,
        env,
    };

    // If no query provided, return connection info for the client
    if query.is_none() {
        let connection_string = provider
            .connection_string(&params)
            .map_err(|e| to_error_data(format!("failed to build connection string: {e}")))?;
        return json_ok(json!({
            "connection_info": {
                "provider": provider_name,
                "host": conn_info.host,
                "port": conn_info.port,
                "connection_string": connection_string,
            },
            "note": "No query provided. Use the connection info above to connect, or provide a query parameter to execute SQL."
        }));
    }

    // Build the query command
    let mut cmd = provider
        .query_client_command(&params, query.as_deref())
        .map_err(|e| to_error_data(format!("failed to build query command: {e}")))?;

    // Execute the command and capture output
    let output = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            let client_name = cmd.get_program().to_string_lossy();
            to_error_data(format!(
                "database client '{}' not found on the MCP server host. \
                 Install it to use query via MCP.",
                client_name
            ))
        } else {
            to_error_data(format!("failed to execute query: {e}"))
        }
    })?;

    // Return results
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        json_ok(json!({
            "success": true,
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": output.status.code().unwrap_or(0),
        }))
    } else {
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        json_err(
            &format!(
                "Query failed with exit code {}: {}",
                output.status.code().unwrap_or(-1),
                if !stderr.is_empty() { &stderr } else { &stdout }
            ),
            Some("QUERY_FAILED"),
        )
    }
}

async fn do_extract_schema(args: &serde_json::Value) -> Result<CallToolResult, McpError> {
    let args = if args.is_object() { args } else { &json!({}) };
    let repo_path = repo_path_from_value(args);

    let compute =
        Arc::new(DockerCompute::new().map_err(|e| to_error_data(format!("Docker: {e}")))?);
    let registry = Arc::new(InMemoryDatabaseProviderRegistry::new());
    containers::register_all(registry.as_ref())
        .map_err(|e| to_error_data(format!("register providers: {e}")))?;

    let use_case = ExtractSchemaUseCase::new(compute, registry);
    let result = use_case
        .run(&repo_path)
        .await
        .map_err(|e| to_error_data(e.to_string()))?;

    // Return the schema metadata as JSON
    json_ok(serde_json::to_value(&result.metadata).unwrap_or_else(|e| {
        json!({
            "error": format!("failed to serialize schema metadata: {e}"),
        })
    }))
}

async fn do_show_schema(args: &serde_json::Value) -> Result<CallToolResult, McpError> {
    let args = if args.is_object() { args } else { &json!({}) };
    let repo_path = repo_path_from_value(args);

    let commit = args
        .get("commit")
        .and_then(|v| v.as_str())
        .ok_or_else(|| to_error_data("commit parameter is required"))?;

    let metadata_only = args
        .get("metadata_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let ddl_only = args
        .get("ddl_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Resolve commit hash
    let commit_hash = repo_layout::rev_parse(&repo_path, commit)
        .map_err(|e| to_error_data(format!("failed to resolve commit '{}': {}", commit, e)))?;

    // Load commit
    let commit_obj = repo_layout::get_commit_from_hash(&repo_path, &commit_hash)
        .map_err(|e| to_error_data(format!("failed to load commit {}: {}", commit_hash, e)))?;

    // Get schema hash
    let schema_hash = commit_obj.schema_hash.ok_or_else(|| {
        to_error_data(format!(
            "commit {} has no schema (schema versioning was not enabled)",
            commit_hash
        ))
    })?;

    // Load schema object
    let (metadata, ddl) =
        repo_layout::get_schema_by_hash(&repo_path, &schema_hash).map_err(|e| {
            to_error_data(format!(
                "failed to load schema object {}: {}",
                schema_hash, e
            ))
        })?;

    // Return based on flags
    if ddl_only {
        json_ok(json!({
            "schema_hash": schema_hash,
            "ddl": ddl,
        }))
    } else if metadata_only {
        json_ok(json!({
            "schema_hash": schema_hash,
            "metadata": metadata,
        }))
    } else {
        json_ok(json!({
            "schema_hash": schema_hash,
            "driver": metadata.driver,
            "version": metadata.version,
            "metadata": metadata,
            "ddl": ddl,
        }))
    }
}

async fn do_diff_schema(args: &serde_json::Value) -> Result<CallToolResult, McpError> {
    let args = if args.is_object() { args } else { &json!({}) };
    let repo_path = repo_path_from_value(args);

    let commit1 = args
        .get("commit1")
        .and_then(|v| v.as_str())
        .ok_or_else(|| to_error_data("commit1 parameter is required"))?;

    let commit2 = args
        .get("commit2")
        .and_then(|v| v.as_str())
        .ok_or_else(|| to_error_data("commit2 parameter is required"))?;

    // Resolve commit hashes
    let hash1 = repo_layout::rev_parse(&repo_path, commit1)
        .map_err(|e| to_error_data(format!("failed to resolve commit '{}': {}", commit1, e)))?;
    let hash2 = repo_layout::rev_parse(&repo_path, commit2)
        .map_err(|e| to_error_data(format!("failed to resolve commit '{}': {}", commit2, e)))?;

    // Load commits
    let commit1_obj = repo_layout::get_commit_from_hash(&repo_path, &hash1)
        .map_err(|e| to_error_data(format!("failed to load commit {}: {}", hash1, e)))?;
    let commit2_obj = repo_layout::get_commit_from_hash(&repo_path, &hash2)
        .map_err(|e| to_error_data(format!("failed to load commit {}: {}", hash2, e)))?;

    // Get schema hashes
    let schema_hash1 = commit1_obj
        .schema_hash
        .ok_or_else(|| to_error_data(format!("commit {} has no schema", hash1)))?;
    let schema_hash2 = commit2_obj
        .schema_hash
        .ok_or_else(|| to_error_data(format!("commit {} has no schema", hash2)))?;

    // Load schema objects
    let (metadata1, _ddl1) =
        repo_layout::get_schema_by_hash(&repo_path, &schema_hash1).map_err(|e| {
            to_error_data(format!(
                "failed to load schema object {}: {}",
                schema_hash1, e
            ))
        })?;
    let (metadata2, _ddl2) =
        repo_layout::get_schema_by_hash(&repo_path, &schema_hash2).map_err(|e| {
            to_error_data(format!(
                "failed to load schema object {}: {}",
                schema_hash2, e
            ))
        })?;

    // Compute rich schema diff using domain logic
    let diff = compute_schema_diff(&metadata1, &metadata2, &hash1, &hash2);

    // Format as JSON using JsonFormatter
    let json_string = JsonFormatter::format(&diff)
        .map_err(|e| to_error_data(format!("failed to serialize JSON output: {}", e)))?;

    // Parse back to serde_json::Value for MCP response
    let json_value: serde_json::Value = serde_json::from_str(&json_string)
        .map_err(|e| to_error_data(format!("failed to parse JSON output: {}", e)))?;

    json_ok(json_value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handler_get_info_returns_expected_server_name() {
        let handler = GfsMcpHandler::new();
        let info = handler.get_info();
        assert_eq!(info.server_info.name, "gfs-mcp");
        assert!(info.capabilities.tools.is_some());
    }

    #[test]
    fn handler_get_info_instructions_mention_list_providers() {
        let handler = GfsMcpHandler::new();
        let info = handler.get_info();
        let instructions = info.instructions.as_deref().unwrap_or("");
        assert!(
            instructions.contains("list_providers"),
            "instructions should mention list_providers"
        );
    }
}
