//! Bearer-token authentication for the MCP HTTP transports.
//!
//! Both the standalone `gfs-mcp` binary and the CLI's embedded `gfs mcp web`
//! serve the same tools over HTTP, so they share one guard rather than each
//! growing their own.

use axum::response::IntoResponse;

/// Where a generated token is written so a client can read it without scraping
/// logs: `$GFS_MCP_TOKEN_FILE`, else `$XDG_STATE_HOME/gfs/mcp-token`, else
/// `~/.gfs/mcp-token`.
pub fn token_file_path() -> Option<std::path::PathBuf> {
    if let Ok(explicit) = std::env::var("GFS_MCP_TOKEN_FILE")
        && !explicit.trim().is_empty()
    {
        return Some(std::path::PathBuf::from(explicit));
    }
    if let Ok(state) = std::env::var("XDG_STATE_HOME")
        && !state.trim().is_empty()
    {
        return Some(std::path::Path::new(&state).join("gfs").join("mcp-token"));
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(|h| std::path::Path::new(&h).join(".gfs").join("mcp-token"))
}

/// Write `token` to `path`, readable only by this user.
///
/// The file is created with 0600 before any content reaches it, rather than
/// written and then chmod-ed, so the secret is never briefly world-readable.
///
/// # Errors
/// Returns the underlying I/O error when the directory or file cannot be created.
pub fn write_token_file(path: &std::path::Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(token.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

/// The bearer token the HTTP transport requires: `GFS_MCP_TOKEN` when set, else a
/// token already present in the token file, else one freshly generated for this run.
///
/// # Errors
/// Returns an error when the operating system CSPRNG is unavailable.
pub fn resolve_http_token() -> Result<(String, bool), Box<dyn std::error::Error + Send + Sync>> {
    if let Ok(configured) = std::env::var("GFS_MCP_TOKEN") {
        let configured = configured.trim().to_string();
        if !configured.is_empty() {
            return Ok((configured, false));
        }
    }
    // A token already written to the configured file is adopted rather than
    // replaced. This is how `gfs mcp start` hands one to the daemon it spawns:
    // the path travels in the environment, the secret does not, so the token
    // cannot be read out of the process table with `ps -E`.
    if let Some(path) = token_file_path()
        && let Ok(existing) = std::fs::read_to_string(&path)
    {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return Ok((existing, false));
        }
    }
    // 256 bits from the OS CSPRNG; the repository's security rules require >= 128.
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| format!("cannot read the OS random source: {e}"))?;
    let token = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    Ok((token, true))
}

/// Compare two tokens without leaking their common prefix through timing.
pub fn tokens_match(presented: &str, expected: &str) -> bool {
    let (a, b) = (presented.as_bytes(), expected.as_bytes());
    // Fold the length difference in rather than returning early on it.
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

/// Reject any request that does not carry `Authorization: Bearer <token>`.
///
/// The transport binds loopback, but loopback is shared with every other process
/// on the host -- and these tools check out branches, run queries and manage
/// database users, so "local" is not a trust boundary.
pub async fn require_bearer(
    axum::extract::State(expected): axum::extract::State<std::sync::Arc<String>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if tokens_match(presented, expected.as_str()) {
        next.run(request).await
    } else {
        // Say nothing about which part was wrong.
        (axum::http::StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }
}

/// `std::env` is process-global and `cargo test` runs tests in parallel, so every
/// test that sets GFS_MCP_TOKEN* takes this first. Without it they race and the
/// failure looks like a bug in the code under test rather than in the harness.
#[cfg(test)]
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod auth_tests {
    use super::*;

    #[test]
    fn an_identical_token_matches() {
        assert!(tokens_match("abc123", "abc123"));
    }

    #[test]
    fn a_different_token_does_not_match() {
        assert!(!tokens_match("abc123", "abc124"));
    }

    #[test]
    fn a_prefix_of_the_token_does_not_match() {
        // The length difference is folded in rather than short-circuited, so a
        // caller cannot learn the length from how quickly it is rejected.
        assert!(!tokens_match("abc", "abc123"));
        assert!(!tokens_match("abc123", "abc"));
    }

    #[test]
    fn an_empty_presented_token_never_matches_a_real_one() {
        assert!(!tokens_match("", "abc123"));
    }

    #[test]
    fn a_generated_token_is_at_least_128_bits_and_differs_per_call() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: serialised by ENV_LOCK; restored before returning.
        unsafe {
            std::env::remove_var("GFS_MCP_TOKEN");
            std::env::set_var("GFS_MCP_TOKEN_FILE", dir.path().join("absent"));
        }
        let (a, generated) = resolve_http_token().expect("csprng");
        assert!(generated, "no GFS_MCP_TOKEN is set in the test environment");
        // 32 bytes rendered as hex.
        assert_eq!(a.len(), 64, "expected 256 bits of hex, got {}", a.len());
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        let (b, _) = resolve_http_token().expect("csprng");
        unsafe { std::env::remove_var("GFS_MCP_TOKEN_FILE") };
        assert_ne!(a, b, "two generated tokens must not be equal");
    }
}

#[cfg(test)]
mod token_file_tests {
    use super::*;

    #[test]
    fn the_file_is_created_readable_only_by_this_user() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("mcp-token");
        write_token_file(&path, "deadbeef").expect("write");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "deadbeef\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "token file must be 0600, got {mode:o}");
        }
    }

    #[test]
    fn rewriting_truncates_rather_than_leaving_a_longer_old_token_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-token");
        write_token_file(&path, "aaaaaaaaaaaaaaaaaaaa").unwrap();
        write_token_file(&path, "bb").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "bb\n");
    }

    #[test]
    fn an_explicit_path_wins_over_the_defaults() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: single-threaded test, variables restored before returning.
        unsafe { std::env::set_var("GFS_MCP_TOKEN_FILE", "/tmp/explicit-gfs-token") };
        assert_eq!(
            token_file_path(),
            Some(std::path::PathBuf::from("/tmp/explicit-gfs-token"))
        );
        unsafe { std::env::remove_var("GFS_MCP_TOKEN_FILE") };
    }
}

/// Exercise the middleware as a real axum service, without binding a socket.
#[cfg(test)]
mod http_auth_tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// A router shaped like the live one: a trivial inner route behind the guard.
    fn guarded_router(token: &str) -> Router {
        Router::new()
            .route(
                "/mcp",
                axum::routing::post(|| async { "reached the tool surface" }),
            )
            .layer(axum::middleware::from_fn_with_state(
                std::sync::Arc::new(token.to_string()),
                require_bearer,
            ))
    }

    async fn status_for(auth: Option<&str>) -> StatusCode {
        let mut req = Request::builder().method("POST").uri("/mcp");
        if let Some(v) = auth {
            req = req.header(axum::http::header::AUTHORIZATION, v);
        }
        guarded_router("the-expected-token")
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn a_request_with_no_authorization_header_is_refused() {
        assert_eq!(status_for(None).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_wrong_token_is_refused() {
        assert_eq!(
            status_for(Some("Bearer not-the-token")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_token_without_the_bearer_prefix_is_refused() {
        assert_eq!(
            status_for(Some("the-expected-token")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn the_correct_token_reaches_the_tool_surface() {
        // The calibration: without this the three refusals above would also pass
        // against a middleware that rejects everything.
        assert_eq!(
            status_for(Some("Bearer the-expected-token")).await,
            StatusCode::OK
        );
    }
}

#[cfg(test)]
mod token_adoption_tests {
    use super::*;

    /// The daemon's parent writes the file and passes only its path; the child
    /// must adopt that token rather than minting a second one.
    #[test]
    fn an_existing_token_file_is_adopted_not_replaced() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-token");
        write_token_file(&path, "parents-token").unwrap();
        // SAFETY: single-threaded test; both vars restored before returning.
        unsafe {
            std::env::remove_var("GFS_MCP_TOKEN");
            std::env::set_var("GFS_MCP_TOKEN_FILE", &path);
        }
        let (token, generated) = resolve_http_token().expect("resolve");
        unsafe { std::env::remove_var("GFS_MCP_TOKEN_FILE") };
        assert_eq!(token, "parents-token");
        assert!(!generated, "an adopted token must not report as generated");
    }

    #[test]
    fn an_empty_token_file_does_not_count_as_a_token() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-token");
        std::fs::write(&path, "   \n").unwrap();
        unsafe {
            std::env::remove_var("GFS_MCP_TOKEN");
            std::env::set_var("GFS_MCP_TOKEN_FILE", &path);
        }
        let (token, generated) = resolve_http_token().expect("resolve");
        unsafe { std::env::remove_var("GFS_MCP_TOKEN_FILE") };
        assert!(generated, "an empty file must fall through to generation");
        assert_eq!(token.len(), 64);
    }
}
