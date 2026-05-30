//! Backend connection that is either plain TCP or TLS, behind one type so the
//! pump can stay generic. Also the `connect_backend` helper that negotiates a
//! PostgreSQL `SSLRequest` and wraps the socket in TLS when enabled.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// PostgreSQL `SSLRequest`: int32 length = 8, int32 code = 80877103.
const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 0x04, 0xD2, 0x16, 0x2F];

pub enum BackendStream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for BackendStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            BackendStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            BackendStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for BackendStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, b: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            BackendStream::Plain(s) => Pin::new(s).poll_write(cx, b),
            BackendStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, b),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            BackendStream::Plain(s) => Pin::new(s).poll_flush(cx),
            BackendStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            BackendStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            BackendStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Connect to the backend, optionally upgrading to TLS via PostgreSQL's
/// `SSLRequest` handshake. `tls` is `Some((connector, server_name))` to encrypt.
pub async fn connect_backend(
    addr: &str,
    tls: Option<(&TlsConnector, &str)>,
) -> io::Result<BackendStream> {
    let mut tcp = TcpStream::connect(addr).await?;
    tcp.set_nodelay(true).ok();

    let Some((connector, domain)) = tls else {
        return Ok(BackendStream::Plain(tcp));
    };

    tcp.write_all(&SSL_REQUEST).await?;
    let mut resp = [0u8; 1];
    tcp.read_exact(&mut resp).await?;
    if resp[0] != b'S' {
        return Err(io::Error::other("backend refused TLS (SSLRequest not accepted)"));
    }
    let server_name = ServerName::try_from(domain.to_string())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid backend TLS domain"))?;
    let tls_stream = connector.connect(server_name, tcp).await?;
    Ok(BackendStream::Tls(Box::new(tls_stream)))
}
