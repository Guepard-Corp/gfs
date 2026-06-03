use std::net::SocketAddr;

use metrics_exporter_prometheus::PrometheusBuilder;

/// Initialise tracing/log output. RUST_LOG takes precedence over `fallback`.
pub fn init_tracing(fallback: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(fallback));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Install the Prometheus exporter, serving `/metrics` on `addr`.
pub fn init_metrics(addr: SocketAddr) -> anyhow::Result<()> {
    PrometheusBuilder::new()
        .with_http_listener(addr)
        .install()?;
    Ok(())
}

/// Install the Prometheus recorder *without* its own HTTP listener and return the
/// render handle. Used in discovery mode, where the proxy serves `/metrics` and
/// `/clones` from a single combined HTTP server.
#[cfg(feature = "discovery")]
pub fn install_recorder() -> anyhow::Result<metrics_exporter_prometheus::PrometheusHandle> {
    Ok(PrometheusBuilder::new().install_recorder()?)
}

// Metric names (kept in one place so the proxy and any dashboards agree).
pub const CONNECTIONS_ACTIVE: &str = "proxy_connections_active";
pub const CONNECTIONS_TOTAL: &str = "proxy_connections_total";
pub const BYTES_TOTAL: &str = "proxy_bytes_total"; // label: direction
