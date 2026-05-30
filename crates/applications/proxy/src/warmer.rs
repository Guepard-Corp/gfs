//! Cache-warming trigger.
//!
//! On observed read queries the warmer calls the in-DB primitive
//! `gfs_sync.warm_for_query($1)` over a dedicated **side connection** (a
//! low-privilege role, distinct from client credentials). It:
//!   - probes the backend once for `gfs_sync.warm_for_query`; if absent the
//!     backend isn't a GFS clone → warming becomes a no-op (zero overhead);
//!   - dedupes identical query texts within a TTL (warm_for_query EXPLAINs +
//!     hydrates, so it isn't free);
//!   - runs fully async, never on the client's data path;
//!   - reconnects with backoff if the side connection drops.
//!
//! The proxy only ever talks to the database — the contract is `gfs_sync.*`.
//! Note: only simple-protocol `Query` reads (literal predicates) are warmable;
//! parameterized `Parse` statements are skipped for now (no constants to push).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::config::Config;
use crate::{db, telemetry};

// Query-driven warming: chunk-warms integer-key tables (enabling elision) and
// falls back to row-copy for others. The proxy passes the raw read SQL; all the
// chunking smarts live in-DB.
const WARM_SQL: &str = "SELECT gfs_sync.warm_query_chunks($1)";
const CAPABILITY_SQL: &str = "SELECT to_regproc('gfs_sync.warm_query_chunks') IS NOT NULL AS ok";
const DEDUP_TTL: Duration = Duration::from_secs(30);
const DEDUP_MAX: usize = 10_000;
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);
/// How often to re-probe a backend that isn't (yet) a GFS clone.
const INCAPABLE_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Warmer {
    tx: Option<mpsc::Sender<String>>,
}

impl Warmer {
    /// Start the warmer. Disabled (`observe` is a no-op) unless `cfg.warm`.
    /// Sets `dirty` whenever it actually hydrates rows, so the refresher only
    /// runs when there is something new to elide.
    pub fn start(cfg: &Config, dirty: Arc<AtomicBool>) -> Self {
        if !cfg.warm {
            return Self { tx: None };
        }
        let (tx, rx) = mpsc::channel::<String>(1024);
        tokio::spawn(run(rx, cfg.clone(), dirty));
        Self { tx: Some(tx) }
    }

    /// Best-effort, non-blocking: queue a read query for warming. Dropped if the
    /// queue is full (warming is purely opportunistic).
    pub fn observe(&self, sql: &str) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(sql.to_owned());
        }
    }
}

/// Side-connection lifecycle: (re)connect, probe capability, drain the queue.
async fn run(mut rx: mpsc::Receiver<String>, cfg: Config, dirty: Arc<AtomicBool>) {
    let mut recent: HashMap<u64, Instant> = HashMap::new();
    loop {
        let client = match db::connect(&cfg, "guepard-proxy-v2-warmer").await {
            Ok(client) => client,
            Err(e) => {
                tracing::warn!(error = %e, "warmer could not connect; backing off");
                tokio::time::sleep(RECONNECT_BACKOFF).await;
                continue;
            }
        };

        let capable = match client.query_one(CAPABILITY_SQL, &[]).await {
            Ok(row) => row.get::<_, bool>("ok"),
            Err(e) => {
                tracing::warn!(error = %e, "warmer capability probe failed; backing off");
                tokio::time::sleep(RECONNECT_BACKOFF).await;
                continue;
            }
        };
        if !capable {
            // Not (yet) a GFS clone. Re-probe later instead of disabling for good:
            // the clone's port accepts connections while its bootstrap is still
            // creating gfs_sync.*, and the clone may be (re)created after we start.
            // Senders never block (observe uses try_send), so just back off.
            tracing::debug!("backend has no gfs_sync.warm_query_chunks yet; will re-probe");
            tokio::time::sleep(INCAPABLE_BACKOFF).await;
            continue;
        }

        tracing::info!("warmer connected; gfs_sync.warm_query_chunks available");
        // Drain until the connection breaks (then reconnect) or the channel closes.
        while let Some(sql) = rx.recv().await {
            metrics::counter!(telemetry::WARM_OBSERVED_TOTAL).increment(1);
            if recently_warmed(&mut recent, &sql) {
                metrics::counter!(telemetry::WARM_CALLS_TOTAL, "outcome" => "skipped_dup")
                    .increment(1);
                continue;
            }
            match client.query_one(WARM_SQL, &[&sql]).await {
                Ok(row) => {
                    metrics::counter!(telemetry::WARM_CALLS_TOTAL, "outcome" => "ok").increment(1);
                    // Only wake the refresher when hydration actually happened, so
                    // an idle (or fully-cached) clone stays quiet.
                    if row.get::<_, i32>(0) > 0 {
                        dirty.store(true, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    metrics::counter!(telemetry::WARM_CALLS_TOTAL, "outcome" => "failed")
                        .increment(1);
                    tracing::debug!(error = %e, "warm call failed; reconnecting");
                    break; // likely a dead connection → reconnect
                }
            }
        }

        // If recv() returned None the channel is closed: nothing more to do.
        if rx.is_closed() && rx.is_empty() {
            return;
        }
        tokio::time::sleep(RECONNECT_BACKOFF).await;
    }
}

/// True if `sql` was warmed within the TTL (so we skip it). Records it otherwise.
fn recently_warmed(recent: &mut HashMap<u64, Instant>, sql: &str) -> bool {
    let now = Instant::now();
    if recent.len() > DEDUP_MAX {
        recent.retain(|_, t| now.duration_since(*t) < DEDUP_TTL);
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    sql.hash(&mut h);
    let key = h.finish();
    match recent.get(&key) {
        Some(t) if now.duration_since(*t) < DEDUP_TTL => true,
        _ => {
            recent.insert(key, now);
            false
        }
    }
}
