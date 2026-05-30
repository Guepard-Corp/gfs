//! Shared side connections (warmer, refresher, cache-stats scraper). All connect
//! to the same backend as the clients, with the warming role. Plain TCP by
//! default; TLS when `--backend-tls` is set (matching the data path).

use std::time::Duration;

use tokio_postgres::{Client, Config as PgConfig, NoTls};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::config::Config;

/// Build a `tokio_postgres` config for a side connection to the backend.
pub fn config(cfg: &Config, application_name: &str) -> PgConfig {
    let backend = cfg.backend.as_deref().unwrap_or("127.0.0.1:5432");
    let (host, port) = backend.rsplit_once(':').unwrap_or((backend, "5432"));
    let mut pg = PgConfig::new();
    pg.host(host)
        .port(port.parse().unwrap_or(5432))
        .user(&cfg.warm_user)
        .password(&cfg.warm_password)
        .dbname(&cfg.warm_dbname)
        .application_name(application_name)
        .connect_timeout(Duration::from_secs(5));
    pg
}

/// Open a side connection (honouring `--backend-tls`) and spawn its driver task.
/// Returns just the `Client`.
pub async fn connect(cfg: &Config, application_name: &str) -> anyhow::Result<Client> {
    let pg = config(cfg, application_name);
    if cfg.backend_tls {
        let rustls_cfg = (*crate::tls::backend_client_config(cfg.backend_tls_insecure)?).clone();
        let (client, connection) = pg.connect(MakeRustlsConnect::new(rustls_cfg)).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::debug!(error = %e, "side connection closed");
            }
        });
        Ok(client)
    } else {
        let (client, connection) = pg.connect(NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::debug!(error = %e, "side connection closed");
            }
        });
        Ok(client)
    }
}
