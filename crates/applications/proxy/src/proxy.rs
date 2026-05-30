//! Connection handling: accept clients, optionally terminate TLS, negotiate the
//! PG startup phase, connect to the configured backend, then pump bytes in both
//! directions. The client→server direction is *sniffed* (never altered) to
//! classify queries for telemetry and feed the warmer.

use std::collections::HashMap;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::config::Config;
use crate::pg::{self, Frontend, StartupKind};
use crate::stream;
use crate::telemetry;
use crate::tls;
use crate::warmer::Warmer;

/// Optional backend-TLS context shared by all connections: connector + the
/// server name used for SNI/verification.
type BackendTls = Option<(TlsConnector, String)>;

/// Largest single frontend message we keep buffered for sniffing. Bigger
/// messages (bulk COPY, huge statements) are classified by type then skipped —
/// forwarding is never affected.
const SNIFF_MSG_CAP: usize = 1 << 20; // 1 MiB
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

/// Bind one listener and pump every client to `backend`, with that backend's
/// warmer/refresher/cache-stats side tasks. Runs until the listener errors or the
/// task is aborted (discovery aborts it when the clone disappears).
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
    // Shared "new hydration happened" flag: the warmer sets it, the refresher
    // consumes it. Starts false so an idle clone is never refreshed until the
    // warmer actually hydrates something.
    let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let warmer = Warmer::start(&cfg, dirty.clone());
    crate::refresher::start(&cfg, dirty);
    crate::cache_stats::start(&cfg);
    tracing::info!(listen = %cfg.listen, %backend, "accepting connections");

    loop {
        let (client, peer) = listener.accept().await?;
        let backend = backend.clone();
        let warmer = warmer.clone();
        let acceptor = acceptor.clone();
        let backend_tls = backend_tls.clone();
        tokio::spawn(async move {
            metrics::counter!(telemetry::CONNECTIONS_TOTAL).increment(1);
            metrics::gauge!(telemetry::CONNECTIONS_ACTIVE).increment(1.0);
            if let Err(e) = handle_conn(client, backend, warmer, acceptor, backend_tls).await {
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
    warmer: Warmer,
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
            return serve(tls_stream, server, warmer, buf, strip_plus).await;
        }
        // No TLS configured → `serve` will reply 'N' and continue in plaintext.
    }

    serve(client, server, warmer, buf, strip_plus).await
}

/// Drive one client (plain or TLS) and backend (plain or TLS) through startup
/// negotiation and the pump. `strip_plus` rewrites the backend's SASL auth
/// message to drop channel binding (needed when proxy↔backend is TLS).
async fn serve<C, S>(
    mut client: C,
    mut server: S,
    warmer: Warmer,
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
            let c2s = tokio::spawn(sniff_copy(cr, sw, seed, warmer));
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

/// Client→server copy that frames frontend messages to (a) sniff reads for
/// telemetry/warming and (b) rewrite `SET search_path` in place so unqualified
/// reads resolve to the federated overlay (gfs_ovl__<schema>). Rewriting keeps
/// the message count identical (one `Q` in, one `Q` out), so the protocol stays
/// in sync. Non-rewritten messages are forwarded verbatim; oversized messages
/// (bulk COPY, huge statements) stream through untouched.
async fn sniff_copy<R, W>(mut r: R, mut w: W, seed: Vec<u8>, warmer: Warmer) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf: Vec<u8> = seed;
    let mut prepared: HashMap<String, String> = HashMap::new();
    let mut passthrough: usize = 0; // bytes of an oversized message still to stream
    let mut raw = false; // unparseable framing → forward everything verbatim

    forward_frontend(&mut buf, &mut w, &mut passthrough, &mut raw, &mut prepared, &warmer).await?;

    let mut tmp = [0u8; IO_BUF];
    loop {
        let n = r.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        forward_frontend(&mut buf, &mut w, &mut passthrough, &mut raw, &mut prepared, &warmer).await?;
    }
    let _ = w.shutdown().await;
    Ok(())
}

/// Cap on remembered prepared statements per connection (cleared if exceeded).
const MAX_PREPARED: usize = 1024;

#[inline]
async fn write_c2s<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(bytes).await?;
    metrics::counter!(telemetry::BYTES_TOTAL, "direction" => "c2s").increment(bytes.len() as u64);
    Ok(())
}

/// Drain complete frontend messages from `buf`, forwarding each (rewritten when
/// it is a `SET search_path`, verbatim otherwise) and feeding the warmer.
async fn forward_frontend<W>(
    buf: &mut Vec<u8>,
    w: &mut W,
    passthrough: &mut usize,
    raw: &mut bool,
    prepared: &mut HashMap<String, String>,
    warmer: &Warmer,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    loop {
        if *raw {
            if !buf.is_empty() {
                write_c2s(w, buf).await?;
                buf.truncate(0);
            }
            return Ok(());
        }
        // Stream the remainder of an oversized message first.
        if *passthrough > 0 {
            let d = (*passthrough).min(buf.len());
            if d > 0 {
                write_c2s(w, &buf[..d]).await?;
                buf.drain(0..d);
                *passthrough -= d;
            }
            if *passthrough > 0 {
                return Ok(());
            }
        }

        let Some((typ, total)) = pg::peek_header(buf) else {
            return Ok(()); // header incomplete
        };
        if total < 5 {
            // Unparseable framing: forward everything verbatim from here on.
            *raw = true;
            continue;
        }

        if total <= SNIFF_MSG_CAP {
            if buf.len() < total {
                return Ok(()); // wait for the whole message
            }
            // Sniff (telemetry + warming) and decide whether to rewrite.
            let rewritten = sniff_message(&buf[..total], prepared, warmer);
            match rewritten {
                Some(out) => write_c2s(w, &out).await?,
                None => write_c2s(w, &buf[..total]).await?,
            }
            buf.drain(0..total);
            continue;
        }

        // Oversized (bulk COPY / huge statement): never a SET search_path. Forward
        // what we have, then stream the rest untouched.
        if matches!(typ, b'Q' | b'P') {
            metrics::counter!(telemetry::QUERIES_TOTAL, "kind" => "other").increment(1);
        }
        metrics::counter!(telemetry::MESSAGES_SKIPPED_LARGE_TOTAL).increment(1);
        let avail = buf.len();
        if avail > 0 {
            write_c2s(w, buf).await?;
            buf.truncate(0);
        }
        *passthrough = total - avail;
        return Ok(());
    }
}

/// Inspect one complete frontend message for telemetry/warming, and return a
/// rewritten message to forward instead (only for `SET search_path`), or `None`
/// to forward the original verbatim.
fn sniff_message(
    msg: &[u8],
    prepared: &mut HashMap<String, String>,
    warmer: &Warmer,
) -> Option<Vec<u8>> {
    match pg::parse_frontend(msg) {
        // Simple query: warm reads directly (it carries literal predicates), and
        // rewrite SET search_path so unqualified reads hit the overlay.
        Frontend::Query(q) => {
            let kind = pg::classify(q);
            metrics::counter!(telemetry::QUERIES_TOTAL, "kind" => kind.as_str()).increment(1);
            if kind == pg::QueryKind::Read {
                warmer.observe(q);
            }
            if let Some(newsql) = pg::rewrite_search_path(q) {
                metrics::counter!(telemetry::SEARCH_PATH_REWRITES_TOTAL).increment(1);
                return Some(pg::build_query_msg(&newsql));
            }
            None
        }
        // Extended protocol Parse: remember read statements; warm on Bind.
        Frontend::Parse { name, query } => {
            let kind = pg::classify(query);
            metrics::counter!(telemetry::QUERIES_TOTAL, "kind" => kind.as_str()).increment(1);
            if kind == pg::QueryKind::Read {
                if prepared.len() >= MAX_PREPARED {
                    prepared.clear();
                }
                prepared.insert(name.to_owned(), query.to_owned());
            } else {
                prepared.remove(name);
            }
            None
        }
        // Bind: if the statement is a remembered read and all params are
        // text-format, substitute them and warm the concrete query.
        Frontend::Bind { stmt, params } => {
            if let (Some(p), Some(q)) = (params, prepared.get(stmt)) {
                warmer.observe(&pg::substitute_params(q, &p));
            }
            None
        }
        Frontend::Other(t) => {
            tracing::trace!(msg_type = (t as char).to_string(), "non-query frontend message");
            None
        }
    }
}
