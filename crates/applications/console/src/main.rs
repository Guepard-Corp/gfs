//! GFS web console — a read-only dashboard over the databases GFS launches,
//! the clones/remotes they overlay, and proxy telemetry.
//!
//! Phase 1: an Axum HTTP API backed by Docker label introspection. The React
//! frontend (served from this same binary) and the telemetry/actions endpoints
//! land in later phases.

mod actions;
mod docker;
mod schema;
mod telemetry;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bollard::Docker;
use clap::Parser;
use serde_json::json;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::actions::{ActionResult, CloneRequest, ComputeRequest};
use crate::docker::{list_databases, GfsDatabase};
use crate::telemetry::{snapshot, TelemetrySnapshot};

#[derive(Parser, Debug)]
#[command(name = "gfs-console", about = "GFS web console backend")]
struct Args {
    /// Address to listen on.
    #[arg(long, env = "GFS_CONSOLE_LISTEN", default_value = "127.0.0.1:7070")]
    listen: SocketAddr,
    /// Warming proxy metrics/clones base URL.
    #[arg(long, env = "GFS_CONSOLE_PROXY", default_value = "http://127.0.0.1:9090")]
    proxy_url: String,
}

#[derive(Clone)]
struct AppState {
    docker: Arc<Docker>,
    http: reqwest::Client,
    proxy_url: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| anyhow::anyhow!("cannot connect to Docker: {e}"))?;
    docker
        .ping()
        .await
        .map_err(|e| anyhow::anyhow!("Docker ping failed (is the daemon running?): {e}"))?;
    let state = AppState {
        docker: Arc::new(docker),
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()?,
        proxy_url: args.proxy_url,
    };

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/databases", get(databases))
        .route("/api/clones", get(clones))
        .route("/api/telemetry", get(telemetry_handler))
        .route("/api/databases/:name/schema", get(database_schema))
        .route("/api/actions/clone", post(action_clone))
        .route("/api/actions/compute", post(action_compute))
        .fallback(static_handler)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(addr = %args.listen, "gfs-console listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}

/// Embedded React frontend (built into the binary from `web/dist`).
#[derive(rust_embed::RustEmbed)]
#[folder = "web/dist"]
struct Assets;

/// Serve a built asset by path; fall back to `index.html` for SPA routes.
async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    let file = Assets::get(path).or_else(|| Assets::get("index.html"));
    match file {
        Some(content) => {
            let mime = content.metadata.mimetype();
            ([(header::CONTENT_TYPE, mime)], content.data).into_response()
        }
        None => (StatusCode::NOT_FOUND, "frontend not built (run npm run build in web/)")
            .into_response(),
    }
}

/// `GET /api/databases` — every `gfs.managed=true` container (sources + clones).
async fn databases(
    State(state): State<AppState>,
) -> Result<Json<Vec<GfsDatabase>>, ApiError> {
    Ok(Json(list_databases(&state.docker).await?))
}

/// `GET /api/clones` — the subset whose `gfs.role=clone`, carrying their
/// `remote` (the upstream they overlay). This is the clone↔remote lineage.
async fn clones(State(state): State<AppState>) -> Result<Json<Vec<GfsDatabase>>, ApiError> {
    let all = list_databases(&state.docker).await?;
    Ok(Json(all.into_iter().filter(|d| d.role == "clone").collect()))
}

/// `GET /api/telemetry` — proxy Prometheus metrics + live `/clones` map.
async fn telemetry_handler(State(state): State<AppState>) -> Json<TelemetrySnapshot> {
    Json(snapshot(&state.http, &state.proxy_url).await)
}

/// `GET /api/databases/:name/schema` — DB → schema → table metadata (size +
/// rows) via the existing `gfs schema extract` (reuses `DatasourceMetadata`).
async fn database_schema(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<schema::SchemaSnapshot>, ApiError> {
    let db = list_databases(&state.docker)
        .await?
        .into_iter()
        .find(|d| d.name == name);
    let Some(db) = db else {
        return Err(ApiError(anyhow::anyhow!("database not found: {name}")));
    };
    let Some(repo) = db.repo else {
        return Ok(Json(schema::SchemaSnapshot {
            reachable: false,
            error: Some("container has no gfs.repo label".to_string()),
            metadata: None,
        }));
    };
    Ok(Json(schema::extract(&repo).await))
}

/// `POST /api/actions/clone` — launch a GFS clone from a remote URL.
async fn action_clone(Json(req): Json<CloneRequest>) -> Json<ActionResult> {
    Json(actions::clone(req).await)
}

/// `POST /api/actions/compute` — start/stop/restart a repo's container.
async fn action_compute(Json(req): Json<ComputeRequest>) -> Json<ActionResult> {
    Json(actions::compute(req).await)
}

/// Maps internal errors to a 500 with a JSON body.
struct ApiError(anyhow::Error);

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(e)
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        tracing::error!(error = %self.0, "request failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": self.0.to_string() })),
        )
            .into_response()
    }
}
