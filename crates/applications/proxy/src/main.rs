mod cache_stats;
mod config;
mod db;
#[cfg(feature = "discovery")]
mod discovery;
mod pg;
mod proxy;
mod refresher;
mod stream;
mod telemetry;
mod tls;
mod warmer;

use clap::Parser;

use crate::config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::parse();
    telemetry::init_tracing(&cfg.log);

    let discover = cfg.is_discovery();

    // In discovery mode the /metrics + /clones server is started by the
    // discovery runner (it needs the live clone registry); otherwise install the
    // standalone Prometheus listener here.
    if !discover {
        telemetry::init_metrics(cfg.metrics)?;
    }

    tracing::info!(
        listen = %cfg.listen,
        backend = cfg.backend.as_deref().unwrap_or("<discover>"),
        metrics = %cfg.metrics,
        warm = cfg.warm,
        discover,
        "guepard-proxy-v2 starting"
    );

    proxy::run(cfg).await
}
