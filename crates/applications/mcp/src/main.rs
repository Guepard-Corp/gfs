//! gfs-mcp — MCP server for GFS / data-plane (RFC 008).
//!
//! **Transport:** stdio (default) or SSE over HTTP.
//!
//! - **Stdio (default):** When run with no arguments (e.g. by Cursor), the server uses stdin/stdout
//!   for JSON-RPC. Configure your client with the full path to this binary and optional `GFS_REPO_PATH`.
//! - **SSE HTTP:** Run with `--http [PORT]` to listen on `127.0.0.1:PORT` (default 3000). SSE at `/mcp/sse`, POST at `/mcp/message`.
//!   `gfs mcp start` spawns the server with `--http`.

use std::io;
use std::net::SocketAddr;

use axum::Router;
use gfs_mcp::GfsMcpHandler;
use gfs_mcp::blank_line_filter::SkipBlankLines;
use rmcp::ServiceExt;
use rmcp::transport::{
    StreamableHttpServerConfig, stdio,
    streamable_http_server::{session::local::LocalSessionManager, tower::StreamableHttpService},
};

fn default_repo_path() -> std::path::PathBuf {
    std::env::var("GFS_REPO_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().expect("current directory not available"))
}

/// Parse args: --http [port] → (true, port), else → (false, 0).
/// When no args are given and stdin is a TTY (interactive run), default to --http 3000
/// so that `cargo run --bin gfs-mcp` starts the HTTP server; otherwise stdio for Cursor etc.
fn parse_transport() -> (bool, u16) {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--http" {
            let port = args.next().and_then(|s| s.parse().ok()).unwrap_or(3000);
            return (true, port);
        }
        if arg == "--stdio" {
            return (false, 0);
        }
    }
    // No args: use HTTP when run interactively (TTY), stdio otherwise (e.g. Cursor)
    if atty::is(atty::Stream::Stdin) {
        (true, 3000)
    } else {
        (false, 0)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Log only to stderr so stdout is reserved for MCP JSON-RPC when using stdio.
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let default_repo = default_repo_path();
    let (use_http, port) = parse_transport();

    if use_http {
        run_http(port, default_repo).await
    } else {
        run_stdio(default_repo).await
    }
}

async fn run_stdio(
    default_repo: std::path::PathBuf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing::info!(
        default_repo = %default_repo.display(),
        "gfs-mcp starting (stdio transport)"
    );

    // Stdin goes through a filter that drops blank lines. Without it a single
    // stray newline terminated the server with exit code 0 — indistinguishable
    // from a clean client disconnect, so a supervisor saw a successful shutdown
    // and any in-flight request was lost with no error. See
    // `blank_line_filter` for why only blank lines are filtered.
    let (input, output) = stdio();
    let service = match GfsMcpHandler::new()
        .serve((SkipBlankLines::new(input), output))
        .await
    {
        Ok(s) => s,
        Err(e) => {
            let msg = e.to_string();
            tracing::error!("failed to start MCP server: {msg}");
            if msg.contains("connection closed") && msg.contains("initialized request") {
                tracing::error!(
                    "stdin was closed before the client sent the initialize request. \
                     This server must be started by your MCP client (e.g. Cursor), not as a daemon. \
                     Use 'gfs mcp start' to run the server on HTTP instead."
                );
            }
            return Err(e.into());
        }
    };

    tracing::info!("gfs-mcp ready, waiting for requests");
    service.waiting().await?;
    Ok(())
}

async fn run_http(
    port: u16,
    _default_repo: std::path::PathBuf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ct = tokio_util::sync::CancellationToken::new();
    let bind: SocketAddr = format!("127.0.0.1:{}", port).parse()?;

    let config = StreamableHttpServerConfig {
        cancellation_token: ct.child_token(),
        ..Default::default()
    };

    let mcp_service = StreamableHttpService::new(
        || Ok(GfsMcpHandler::new()),
        std::sync::Arc::new(LocalSessionManager::default()),
        config,
    );

    let app = Router::new().nest_service("/mcp", mcp_service);

    tracing::info!(
        addr = %bind,
        "gfs-mcp starting (streamable HTTP, no auth). Connect to: POST http://{}/mcp",
        bind
    );

    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            ct.cancel();
        })
        .await?;

    Ok(())
}
