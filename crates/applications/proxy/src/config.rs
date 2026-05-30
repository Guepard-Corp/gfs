use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

/// guepard-proxy-v2 — a thin PostgreSQL wire-protocol proxy that fronts a single
/// configured backend, observes queries for telemetry, and (optionally) triggers
/// in-DB cache warming for GFS lazy clones.
///
/// Inspired by guepard-proxy (v1) but stripped of SNI/Nomad routing: the backend
/// is configured, so the same binary works locally (Docker) and in the cloud.
#[derive(Parser, Clone, Debug)]
#[command(name = "guepard-proxy-v2", version, about)]
pub struct Config {
    /// Address to listen on for client PostgreSQL connections.
    #[arg(long, default_value = "0.0.0.0:6432", env = "PROXY_LISTEN")]
    pub listen: SocketAddr,

    /// Backend PostgreSQL address to forward to (`host:port`; the host may be a
    /// name, e.g. `host.docker.internal:5432`, resolved at connect time).
    /// Omit it to auto-discover clones from Docker (same as `--discover`).
    #[arg(long, env = "PROXY_BACKEND")]
    pub backend: Option<String>,

    /// Force Docker auto-discovery of GFS clone containers (labels
    /// `gfs.managed=true`, `gfs.role=clone`, `gfs.provider=postgres`), fronting
    /// each on its own listener. Implied when no `--backend` is given. Clones
    /// created/removed at runtime are picked up by a periodic reconcile.
    #[cfg(feature = "discovery")]
    #[arg(long, default_value_t = false, env = "PROXY_DISCOVER")]
    pub discover: bool,

    /// First listen port assigned to discovered clones; subsequent clones take the
    /// next free port upward. The clone→port mapping is published at `/clones`.
    #[cfg(feature = "discovery")]
    #[arg(long, default_value_t = 55500, env = "PROXY_LISTEN_BASE")]
    pub listen_base: u16,

    /// Seconds between Docker reconcile scans in `--discover` mode.
    #[cfg(feature = "discovery")]
    #[arg(long, default_value_t = 3, env = "PROXY_DISCOVER_INTERVAL")]
    pub discover_interval: u64,

    /// Address to expose Prometheus metrics on (`/metrics`).
    #[arg(long, default_value = "127.0.0.1:9090", env = "PROXY_METRICS")]
    pub metrics: SocketAddr,

    /// Log filter (overridden by RUST_LOG if set).
    #[arg(long, default_value = "info", env = "PROXY_LOG")]
    pub log: String,

    /// Enable cache warming: observe read queries and call the in-DB
    /// `gfs_sync.warm_for_query($1)` on a side connection. No-op (zero overhead)
    /// on backends that don't expose that function.
    #[arg(long, default_value_t = false, env = "PROXY_WARM")]
    pub warm: bool,

    /// Role used by the warmer's side connection (needs EXECUTE on
    /// `gfs_sync.warm_for_query`). Distinct from client credentials.
    #[arg(long, default_value = "postgres", env = "PROXY_WARM_USER")]
    pub warm_user: String,

    /// Password for the warming role.
    #[arg(long, default_value = "postgres", env = "PROXY_WARM_PASSWORD")]
    pub warm_password: String,

    /// Database the clone overlay (and `gfs_sync`) lives in.
    #[arg(long, default_value = "postgres", env = "PROXY_WARM_DBNAME")]
    pub warm_dbname: String,

    /// Seconds between `gfs_sync.refresh_exclusions()` calls, which apply newly
    /// hydrated ranges as elision (the AccessExclusive CHECK rebuild, decoupled
    /// from per-read warming). Only runs with `--warm`.
    #[arg(long, default_value_t = 10, env = "PROXY_REFRESH_INTERVAL")]
    pub refresh_interval: u64,

    /// Periodically scrape in-DB cache-coverage stats (`gfs_sync`) and expose
    /// them as gauges. No-op on backends that aren't GFS clones.
    #[arg(long, default_value_t = false, env = "PROXY_CACHE_METRICS")]
    pub cache_metrics: bool,

    /// Seconds between cache-coverage scrapes.
    #[arg(long, default_value_t = 15, env = "PROXY_CACHE_METRICS_INTERVAL")]
    pub cache_metrics_interval: u64,

    /// PEM certificate chain for TLS termination. Set together with `--tls-key`
    /// to accept `sslmode=require` clients; omit for plaintext only.
    #[arg(long, env = "PROXY_TLS_CERT")]
    pub tls_cert: Option<PathBuf>,

    /// PEM private key for TLS termination.
    #[arg(long, env = "PROXY_TLS_KEY")]
    pub tls_key: Option<PathBuf>,

    /// Encrypt the proxy→backend connection (data path + side connections): the
    /// proxy sends a PostgreSQL SSLRequest to the backend and wraps it in TLS.
    #[arg(long, default_value_t = false, env = "PROXY_BACKEND_TLS")]
    pub backend_tls: bool,

    /// Skip backend certificate verification (encrypt-only, like
    /// `sslmode=require`). Otherwise verified against the system trust store.
    #[arg(long, default_value_t = false, env = "PROXY_BACKEND_TLS_INSECURE")]
    pub backend_tls_insecure: bool,

    /// Server name used for backend TLS (SNI + verification). Defaults to the
    /// backend IP when unset.
    #[arg(long, env = "PROXY_BACKEND_TLS_DOMAIN")]
    pub backend_tls_domain: Option<String>,
}

impl Config {
    /// Whether to run in Docker auto-discovery mode. Implied when no `--backend`
    /// is given (and the `discovery` feature is built in), so the proxy "just
    /// finds the clones" with zero config; an explicit `--discover` forces it.
    #[cfg(feature = "discovery")]
    pub fn is_discovery(&self) -> bool {
        self.discover || self.backend.is_none()
    }

    #[cfg(not(feature = "discovery"))]
    pub fn is_discovery(&self) -> bool {
        false
    }
}
