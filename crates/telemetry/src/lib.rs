use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Initialise the global tracing subscriber.
///
/// Configuration is driven entirely by environment variables so that the same
/// binary can produce different output in development vs. production without
/// recompilation.
///
/// | Variable          | Values                          | Default   |
/// |-------------------|---------------------------------|-----------|
/// | `RUST_LOG`        | standard `tracing` directives   | `"info"`  |
/// | `RUST_LOG_FORMAT` | `"json"` or anything else       | compact   |
///
/// Call this **once**, as the very first statement of `main()`, before any
/// other crate initialisation.
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    match std::env::var("RUST_LOG_FORMAT").as_deref() {
        Ok("json") => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().json())
                .init();
        }
        _ => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().compact())
                .init();
        }
    }
}

// ---------------------------------------------------------------------------
// PostHog configuration (set GFS_POSTHOG_API_KEY at build time)
// ---------------------------------------------------------------------------

pub const POSTHOG_API_KEY: &str = match option_env!("GFS_POSTHOG_API_KEY") {
    Some(k) => k,
    None => "",
};

const POSTHOG_HOST: &str = "https://eu.i.posthog.com";

// ---------------------------------------------------------------------------
// Source detection
// ---------------------------------------------------------------------------

/// Detect the surface from which GFS is being invoked.
///
/// Heuristic order:
/// 1. Cursor IDE   – `CURSOR_WINDOW_ID` or `CURSOR_TRACE_ID`
/// 2. Claude Code  – `CLAUDE_CODE_ENTRYPOINT` or any `CLAUDECODE_*` var
/// 3. CI           – `CI=true` (or `CI=1`)
/// 4. Otherwise    – `"cli"`
///
/// MCP tools pass `"mcp"` explicitly at their call sites.
pub fn detect_source() -> &'static str {
    if std::env::var_os("CURSOR_WINDOW_ID").is_some()
        || std::env::var_os("CURSOR_TRACE_ID").is_some()
    {
        return "cursor";
    }
    if std::env::var_os("CLAUDE_CODE_ENTRYPOINT").is_some() {
        return "claude_code";
    }
    // Check for any CLAUDECODE_* env var
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("CLAUDECODE_") {
            return "claude_code";
        }
    }
    if matches!(std::env::var("CI").as_deref(), Ok("true") | Ok("1")) {
        return "ci";
    }
    "cli"
}

// ---------------------------------------------------------------------------
// Error categorisation
// ---------------------------------------------------------------------------

/// Return a coarse error category string derived from the error's type/display.
/// Never exposes the full error message.
pub fn error_category(err: &anyhow::Error) -> &'static str {
    let display = err.to_string();
    // Match against known domain error prefixes (from thiserror Display strings)
    if display.contains("No .gfs repository") || display.contains("not a gfs repository") {
        return "RepositoryError";
    }
    if display.contains("IO error") || display.contains("os error") {
        return "IoError";
    }
    if display.contains("storage") || display.contains("Storage") || display.contains("mount") {
        return "StorageError";
    }
    if display.contains("compute")
        || display.contains("Compute")
        || display.contains("container")
        || display.contains("docker")
    {
        return "ComputeError";
    }
    if display.contains("config")
        || display.contains("Config")
        || display.contains("Invalid config")
    {
        return "ConfigError";
    }
    if display.contains("schema") || display.contains("Schema") {
        return "SchemaError";
    }
    if display.contains("commit") || display.contains("Commit") {
        return "CommitError";
    }
    if display.contains("checkout")
        || display.contains("Checkout")
        || display.contains("revision not found")
    {
        return "CheckoutError";
    }
    if display.contains("database") || display.contains("Database") || display.contains("sql") {
        return "DatabaseError";
    }
    "UnknownError"
}

// ---------------------------------------------------------------------------
// TelemetryClient
// ---------------------------------------------------------------------------

use std::sync::{Arc, Mutex};

/// Inner state shared across clones of `TelemetryClient`.
/// Joining spawned threads on drop ensures the process does not exit before
/// in-flight HTTP requests to PostHog complete.
#[derive(Debug)]
struct TelemetryInner {
    enabled: bool,
    distinct_id: String,
    handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl Drop for TelemetryInner {
    fn drop(&mut self) {
        let handles = std::mem::take(&mut *self.handles.lock().unwrap_or_else(|e| e.into_inner()));
        tracing::debug!(target: "gfs_telemetry", "flushing {} in-flight telemetry thread(s)", handles.len());
        for h in handles {
            let _ = h.join();
        }
    }
}

/// Telemetry client — cheap to clone (backed by `Arc`).
///
/// All pending HTTP requests are flushed when the last clone is dropped,
/// so the process will not exit before events reach PostHog.
#[derive(Debug, Clone)]
pub struct TelemetryClient {
    inner: Arc<TelemetryInner>,
}

impl TelemetryClient {
    /// Create a new `TelemetryClient`.
    ///
    /// Telemetry is disabled if:
    /// - `GFS_NO_TELEMETRY` env var is `"1"` or `"true"`, or
    /// - `telemetry` is set to `false` in `~/.gfs/config.toml`.
    ///
    /// On first run (no `~/.gfs/telemetry_id` file), a UUID is generated,
    /// persisted, and a one-time notice is printed to stderr.
    pub fn new() -> Self {
        // Check env-var opt-out first
        if matches!(
            std::env::var("GFS_NO_TELEMETRY").as_deref(),
            Ok("1") | Ok("true")
        ) {
            tracing::debug!(target: "gfs_telemetry", "telemetry disabled via GFS_NO_TELEMETRY");
            return Self::disabled();
        }

        // Check global config opt-out
        let enabled = gfs_domain::model::config::GlobalSettings::load()
            .map(|s| {
                tracing::debug!(target: "gfs_telemetry", "loaded global config: telemetry={}", s.telemetry);
                s.telemetry
            })
            .unwrap_or_else(|| {
                tracing::debug!(target: "gfs_telemetry", "no global config found, telemetry defaults to enabled");
                true
            });

        if !enabled {
            tracing::debug!(target: "gfs_telemetry", "telemetry disabled via config");
            return Self::disabled();
        }

        let distinct_id = load_or_create_telemetry_id();
        tracing::debug!(target: "gfs_telemetry", "TelemetryClient ready: enabled=true, distinct_id={distinct_id}");

        Self {
            inner: Arc::new(TelemetryInner {
                enabled: true,
                distinct_id,
                handles: Mutex::new(Vec::new()),
            }),
        }
    }

    fn disabled() -> Self {
        Self {
            inner: Arc::new(TelemetryInner {
                enabled: false,
                distinct_id: String::new(),
                handles: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Track an event. No-op if telemetry is disabled or the API key is empty.
    /// The HTTP request runs in a background thread that is joined when this
    /// client (and all its clones) are dropped — ensuring delivery before exit.
    pub fn track(&self, event_name: &'static str, props: Vec<(&'static str, serde_json::Value)>) {
        if !self.inner.enabled {
            tracing::debug!(target: "gfs_telemetry", "track({event_name}): skipped — telemetry disabled");
            return;
        }
        if POSTHOG_API_KEY.is_empty() {
            tracing::debug!(target: "gfs_telemetry", "track({event_name}): skipped — POSTHOG_API_KEY not set");
            return;
        }

        tracing::debug!(target: "gfs_telemetry", "track({event_name}): queuing event with {} props", props.len());

        let distinct_id = self.inner.distinct_id.clone();
        let props = props
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect::<Vec<_>>();

        let handle = std::thread::spawn(move || {
            tracing::debug!(target: "gfs_telemetry", "send_event({event_name}): sending to PostHog");
            match send_event(event_name, &distinct_id, props) {
                Ok(()) => tracing::debug!(target: "gfs_telemetry", "send_event({event_name}): ok"),
                Err(e) => {
                    tracing::debug!(target: "gfs_telemetry", "send_event({event_name}): failed — {e}")
                }
            }
        });

        if let Ok(mut handles) = self.inner.handles.lock() {
            handles.push(handle);
        }
    }
}

impl Default for TelemetryClient {
    fn default() -> Self {
        Self::new()
    }
}

fn send_event(
    event_name: &str,
    distinct_id: &str,
    props: Vec<(String, serde_json::Value)>,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = posthog_rs::client((POSTHOG_API_KEY, POSTHOG_HOST));
    let mut event = posthog_rs::Event::new(event_name, distinct_id);
    for (key, value) in props {
        let _ = event.insert_prop(&key, &value);
    }
    client.capture(event)?;
    Ok(())
}

/// Load `~/.gfs/telemetry_id`, or generate a new UUID and persist it.
/// Prints a first-run notice to stderr when generating.
fn load_or_create_telemetry_id() -> String {
    let id_path = match gfs_domain::model::config::GlobalSettings::path() {
        Some(config_path) => config_path
            .parent()
            .expect("config path has a parent")
            .join("telemetry_id"),
        None => return uuid::Uuid::new_v4().to_string(),
    };

    if let Ok(contents) = std::fs::read_to_string(&id_path) {
        let id = contents.trim().to_string();
        if !id.is_empty() {
            tracing::debug!(target: "gfs_telemetry", "loaded telemetry_id from {}", id_path.display());
            return id;
        }
    }

    // First run: generate and persist
    let id = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = id_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&id_path, &id).is_ok() {
        tracing::debug!(target: "gfs_telemetry", "generated new telemetry_id, saved to {}", id_path.display());
        eprintln!(
            "GFS collects anonymous usage data to improve the product.\n\
             Run `gfs config --global telemetry.enabled false` or set GFS_NO_TELEMETRY=1 to opt out."
        );
    } else {
        tracing::debug!(target: "gfs_telemetry", "generated new telemetry_id (could not persist to {})", id_path.display());
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "sets a global tracing subscriber; run in isolation with -- --ignored"]
    fn init_does_not_panic() {
        // init() sets a global subscriber — can only be called once per process.
        init();
    }

    #[test]
    fn detect_source_returns_valid_string() {
        // detect_source() must always return one of the known source values.
        // We don't mutate env vars here to avoid races with parallel tests.
        let source = detect_source();
        let valid = ["cli", "mcp", "cursor", "claude_code", "ci"];
        assert!(
            valid.contains(&source),
            "detect_source returned unexpected value: {source}"
        );
    }

    #[test]
    fn error_category_repo_error() {
        let err = anyhow::anyhow!("No .gfs repository found in /tmp");
        assert_eq!(error_category(&err), "RepositoryError");
    }

    #[test]
    fn error_category_unknown() {
        let err = anyhow::anyhow!("something completely different");
        assert_eq!(error_category(&err), "UnknownError");
    }

    #[test]
    fn telemetry_client_disabled_by_env() {
        unsafe { std::env::set_var("GFS_NO_TELEMETRY", "1") };
        let client = TelemetryClient::new();
        assert!(!client.inner.enabled);
        unsafe { std::env::remove_var("GFS_NO_TELEMETRY") };
    }

    #[test]
    fn posthog_key_constant_accessible() {
        // Just ensure the constant compiles and is accessible
        let _ = POSTHOG_API_KEY;
    }
}
