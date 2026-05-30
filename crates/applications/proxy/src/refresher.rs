//! Periodic exclusion-CHECK rebuild. Warming (`warm_query_chunks`) only hydrates
//! ranges; the read-blocking `ALTER FOREIGN TABLE` that turns a hydrated range
//! into elision is decoupled here so it runs **coalesced on a timer** instead of
//! on every read. Calls `gfs_sync.refresh_exclusions()` (lock_timeout-bounded
//! in-DB). No-op on non-clone backends.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::db;

const CAPABILITY_SQL: &str = "SELECT to_regproc('gfs_sync.refresh_exclusions') IS NOT NULL AS ok";
const REFRESH_SQL: &str = "SELECT gfs_sync.refresh_exclusions()";

/// Start the refresher (only meaningful alongside `--warm`). `dirty` is set by
/// the warmer whenever it actually hydrates new rows; the refresher only touches
/// the DB when it is set, so an idle clone sees ZERO refresh traffic.
pub fn start(cfg: &Config, dirty: Arc<AtomicBool>) {
    if !cfg.warm {
        return;
    }
    let interval = Duration::from_secs(cfg.refresh_interval.max(1));
    tokio::spawn(run(cfg.clone(), interval, dirty));
}

async fn run(cfg: Config, interval: Duration, dirty: Arc<AtomicBool>) {
    loop {
        match db::connect(&cfg, "guepard-proxy-v2-refresher").await {
            Ok(client) => {
                let capable = client
                    .query_one(CAPABILITY_SQL, &[])
                    .await
                    .map(|r| r.get::<_, bool>("ok"))
                    .unwrap_or(false);
                if !capable {
                    // Not (yet) a clone (e.g. bootstrap still running, or clone
                    // created later). Re-probe on the next reconnect rather than
                    // disabling permanently.
                    tracing::debug!("refresher: gfs_sync.refresh_exclusions not available yet; re-probing");
                    tokio::time::sleep(interval).await;
                    continue;
                }

                loop {
                    tokio::time::sleep(interval).await;
                    // Activity-driven: skip the DB round-trip entirely unless the
                    // warmer flagged new hydration since the last refresh. An idle
                    // clone is never touched (no churn, no NOTICE/lock activity).
                    if !dirty.swap(false, Ordering::Relaxed) {
                        continue;
                    }
                    if let Err(e) = client.execute(REFRESH_SQL, &[]).await {
                        tracing::debug!(error = %e, "refresh_exclusions failed; reconnecting");
                        dirty.store(true, Ordering::Relaxed); // retry after reconnect
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "refresher connect failed; backing off");
            }
        }
        tokio::time::sleep(interval).await;
    }
}
