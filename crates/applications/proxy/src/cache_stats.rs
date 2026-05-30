//! Cache-coverage telemetry (M2). Periodically scrapes the in-DB `gfs_sync`
//! catalog on a side connection and exposes gauges, so dashboards can see how
//! much of each clone is hydrated. Read-only; a no-op on non-clone backends.
//!
//! The proxy measures traffic/warming; the DB measures cache coverage. (Whether
//! a given read was served local vs remote isn't visible from the proxy without
//! per-query EXPLAIN; coverage is the cheap, meaningful proxy-exposable signal.)

use std::time::Duration;

use crate::config::Config;
use crate::{db, telemetry};

const CAPABILITY_SQL: &str = "SELECT to_regclass('gfs_sync.cached_range') IS NOT NULL AS ok";
const STATS_SQL: &str = "SELECT \
    (SELECT count(*) FROM gfs_sync.cached_range)::bigint AS ranges, \
    (SELECT count(DISTINCT (schema_name, table_name)) FROM gfs_sync.cached_range)::bigint AS tables, \
    (SELECT count(*) FROM gfs_sync.table_meta)::bigint AS overlays";

/// Start the scraper if `--cache-metrics` is set. Requires SELECT on the
/// `gfs_sync` catalog for the warming role (a superuser locally).
pub fn start(cfg: &Config) {
    if !cfg.cache_metrics {
        return;
    }
    let interval = Duration::from_secs(cfg.cache_metrics_interval.max(1));
    tokio::spawn(run(cfg.clone(), interval));
}

async fn run(cfg: Config, interval: Duration) {
    loop {
        match db::connect(&cfg, "guepard-proxy-v2-cache-stats").await {
            Ok(client) => {
                let capable = client
                    .query_one(CAPABILITY_SQL, &[])
                    .await
                    .map(|r| r.get::<_, bool>("ok"))
                    .unwrap_or(false);
                if !capable {
                    // Not (yet) a clone; re-probe later instead of disabling.
                    tracing::debug!("cache-stats: gfs_sync not available yet; re-probing");
                    tokio::time::sleep(interval).await;
                    continue;
                }

                loop {
                    match client.query_one(STATS_SQL, &[]).await {
                        Ok(row) => {
                            metrics::gauge!(telemetry::CACHE_RANGES)
                                .set(row.get::<_, i64>("ranges") as f64);
                            metrics::gauge!(telemetry::CACHE_TABLES)
                                .set(row.get::<_, i64>("tables") as f64);
                            metrics::gauge!(telemetry::OVERLAY_TABLES)
                                .set(row.get::<_, i64>("overlays") as f64);
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "cache-stats query failed; reconnecting");
                            break;
                        }
                    }
                    tokio::time::sleep(interval).await;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "cache-stats connect failed; backing off");
            }
        }
        tokio::time::sleep(interval).await;
    }
}
