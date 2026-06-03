//! Connection handling: accept clients, optionally terminate TLS, negotiate the
//! PG startup phase, connect to the configured backend, then pump bytes verbatim
//! in both directions. The proxy is a transparent passthrough — GFS clones are
//! copy-on-read in-DB (the gfs Table Access Method), so there is no query
//! sniffing or search_path rewriting to do.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::config::Config;
use crate::pg::{self, StartupKind};
use crate::stream;
use crate::telemetry;
use crate::tls;

/// Optional backend-TLS context shared by all connections: connector + the
/// server name used for SNI/verification.
type BackendTls = Option<(TlsConnector, String)>;

const IO_BUF: usize = 16 * 1024;

/// Entry point: front a single `--backend`, or (with `--discover`) auto-discover
/// clone containers and front each on its own listener.
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    #[cfg(feature = "discovery")]
    if cfg.is_discovery() {
        return crate::discovery::run(cfg).await;
    }
    let backend = cfg.backend.clone().ok_or_else(|| {
        anyhow::anyhow!("--backend is required (build with the `discovery` feature to auto-discover instead)")
    })?;
    serve_backend(cfg, backend).await
}

/// Bind one listener and pump every client to `backend` verbatim. Runs until the
/// listener errors or the task is aborted (discovery aborts it when the clone
/// disappears).
pub async fn serve_backend(cfg: Config, backend: String) -> anyhow::Result<()> {
    let acceptor = match (&cfg.tls_cert, &cfg.tls_key) {
        (Some(c), Some(k)) => {
            let a = tls::acceptor(c, k)?;
            tracing::info!("TLS termination enabled");
            Some(a)
        }
        (None, None) => None,
        _ => anyhow::bail!("--tls-cert and --tls-key must be provided together"),
    };

    let backend_tls: BackendTls = if cfg.backend_tls {
        let connector = tls::backend_connector(cfg.backend_tls_insecure)?;
        let domain = cfg.backend_tls_domain.clone().unwrap_or_else(|| {
            backend
                .rsplit_once(':')
                .map_or_else(|| backend.clone(), |(h, _)| h.to_string())
        });
        tracing::info!(insecure = cfg.backend_tls_insecure, %domain, "backend TLS enabled");
        Some((connector, domain))
    } else {
        None
    };

    let listener = TcpListener::bind(cfg.listen).await?;
    tracing::info!(listen = %cfg.listen, %backend, "accepting connections");

    loop {
        let (client, peer) = listener.accept().await?;
        let backend = backend.clone();
        let acceptor = acceptor.clone();
        let backend_tls = backend_tls.clone();
        tokio::spawn(async move {
            metrics::counter!(telemetry::CONNECTIONS_TOTAL).increment(1);
            metrics::gauge!(telemetry::CONNECTIONS_ACTIVE).increment(1.0);
            if let Err(e) = handle_conn(client, backend, acceptor, backend_tls).await {
                tracing::debug!(%peer, error = %e, "connection ended");
            }
            metrics::gauge!(telemetry::CONNECTIONS_ACTIVE).decrement(1.0);
        });
    }
}

/// Read the first startup packet to decide on a TLS upgrade, then hand off to
/// the generic `serve` over the resulting client stream (plain or TLS).
async fn handle_conn(
    mut client: TcpStream,
    backend: String,
    acceptor: Option<TlsAcceptor>,
    backend_tls: BackendTls,
) -> anyhow::Result<()> {
    client.set_nodelay(true).ok();

    // Connect to the backend (plain, or TLS via SSLRequest).
    let tls_ref = backend_tls.as_ref().map(|(c, d)| (c, d.as_str()));
    let server = stream::connect_backend(&backend, tls_ref).await?;
    // With a TLS backend, strip SCRAM channel binding from the auth handshake.
    let strip_plus = backend_tls.is_some();

    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let (kind, len) = loop {
        if let Some(h) = pg::parse_startup(&buf) {
            break h;
        }
        let mut tmp = [0u8; 1024];
        let n = client.read(&mut tmp).await?;
        if n == 0 {
            return Ok(()); // client closed before sending a startup packet
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    if kind == StartupKind::SslRequest {
        if let Some(acceptor) = acceptor {
            buf.drain(0..len); // consume the SSLRequest
            client.write_all(b"S").await?;
            let tls_stream = acceptor.accept(client).await?;
            // After the handshake the StartupMessage arrives encrypted; `buf` is
            // empty (the client waits for 'S' before the TLS handshake).
            return serve(tls_stream, server, buf, strip_plus).await;
        }
        // No TLS configured → `serve` will reply 'N' and continue in plaintext.
    }

    serve(client, server, buf, strip_plus).await
}

/// Drive one client (plain or TLS) and backend (plain or TLS) through startup
/// negotiation and the pump. `strip_plus` rewrites the backend's SASL auth
/// message to drop channel binding (needed when proxy↔backend is TLS).
async fn serve<C, S>(
    mut client: C,
    mut server: S,
    mut buf: Vec<u8>,
    strip_plus: bool,
) -> anyhow::Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    match negotiate_startup(&mut client, &mut buf).await? {
        Some(Negotiated::Startup(startup)) => {
            server.write_all(&startup).await?;
            let seed = std::mem::take(&mut buf);
            let (cr, cw) = tokio::io::split(client);
            let (sr, sw) = tokio::io::split(server);
            let c2s = tokio::spawn(raw_copy_seeded(cr, sw, seed));
            let s2c = tokio::spawn(async move {
                if strip_plus {
                    s2c_auth_copy(sr, cw).await
                } else {
                    raw_copy(sr, cw, "s2c").await
                }
            });
            let _ = tokio::join!(c2s, s2c);
        }
        Some(Negotiated::Cancel(bytes)) => {
            server.write_all(&bytes).await?;
            let (cr, cw) = tokio::io::split(client);
            let (sr, sw) = tokio::io::split(server);
            let a = tokio::spawn(raw_copy(cr, sw, "c2s"));
            let b = tokio::spawn(raw_copy(sr, cw, "s2c"));
            let _ = tokio::join!(a, b);
        }
        None => {}
    }
    Ok(())
}

enum Negotiated {
    Startup(Vec<u8>),
    Cancel(Vec<u8>),
}

/// Read startup packets until a real StartupMessage (or CancelRequest) is seen.
/// SSL/GSS requests are refused with `N` (TLS, if any, was handled earlier in
/// `handle_conn`), after which the client retries unencrypted.
async fn negotiate_startup<C>(client: &mut C, buf: &mut Vec<u8>) -> anyhow::Result<Option<Negotiated>>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        if let Some((kind, len)) = pg::parse_startup(buf) {
            match kind {
                StartupKind::SslRequest | StartupKind::GssEncRequest => {
                    client.write_all(b"N").await?;
                    buf.drain(0..len);
                    continue;
                }
                StartupKind::CancelRequest => {
                    let bytes = buf[..len].to_vec();
                    buf.drain(0..len);
                    return Ok(Some(Negotiated::Cancel(bytes)));
                }
                StartupKind::Startup => {
                    let bytes = buf[..len].to_vec();
                    buf.drain(0..len);
                    return Ok(Some(Negotiated::Startup(bytes)));
                }
            }
        }
        let mut tmp = [0u8; 4096];
        let n = client.read(&mut tmp).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

/// Verbatim copy in one direction, counting bytes.
async fn raw_copy<R, W>(mut r: R, mut w: W, dir: &'static str) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = [0u8; IO_BUF];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n]).await?;
        metrics::counter!(telemetry::BYTES_TOTAL, "direction" => dir).increment(n as u64);
    }
    let _ = w.shutdown().await;
    Ok(())
}

/// Client→server copy that first forwards any bytes already read past the
/// startup message (`seed`), then copies verbatim. Replaces the old sniffing
/// pump now that the proxy is a transparent passthrough.
async fn raw_copy_seeded<R, W>(r: R, mut w: W, seed: Vec<u8>) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if !seed.is_empty() {
        w.write_all(&seed).await?;
        metrics::counter!(telemetry::BYTES_TOTAL, "direction" => "c2s").increment(seed.len() as u64);
    }
    raw_copy(r, w, "c2s").await
}

/// Server→client copy that, during the auth phase, rewrites the backend's
/// `AuthenticationSASL` message to drop `SCRAM-SHA-256-PLUS` (channel binding
/// can't cross a TLS-terminating proxy). After auth completes it forwards
/// verbatim. Only used when the proxy↔backend link is TLS.
async fn s2c_auth_copy<R, W>(mut r: R, mut w: W) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut done = false;
    let mut tmp = [0u8; IO_BUF];
    loop {
        let n = r.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        if done {
            w.write_all(&tmp[..n]).await?;
            metrics::counter!(telemetry::BYTES_TOTAL, "direction" => "s2c").increment(n as u64);
            continue;
        }
        buf.extend_from_slice(&tmp[..n]);
        loop {
            let Some((typ, total)) = pg::peek_header(&buf) else { break };
            if total < 5 || buf.len() < total {
                if total < 5 {
                    done = true; // malformed; stop inspecting
                }
                break;
            }
            let out: Vec<u8> = match (typ, pg::auth_type(&buf[..total])) {
                (b'R', Some(10)) => pg::strip_scram_plus(&buf[..total])
                    .unwrap_or_else(|| buf[..total].to_vec()),
                _ => buf[..total].to_vec(),
            };
            w.write_all(&out).await?;
            metrics::counter!(telemetry::BYTES_TOTAL, "direction" => "s2c").increment(out.len() as u64);
            // AuthenticationOk / ReadyForQuery / ErrorResponse → auth phase over.
            if matches!((typ, pg::auth_type(&buf[..total])), (b'R', Some(0)))
                || typ == b'Z'
                || typ == b'E'
            {
                done = true;
            }
            buf.drain(0..total);
            if done {
                break;
            }
        }
        if done && !buf.is_empty() {
            w.write_all(&buf).await?;
            metrics::counter!(telemetry::BYTES_TOTAL, "direction" => "s2c").increment(buf.len() as u64);
            buf.clear();
        }
    }
    let _ = w.shutdown().await;
    Ok(())
}
