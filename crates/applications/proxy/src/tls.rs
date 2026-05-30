//! TLS: termination for clients that require SSL (`sslmode=require`), and an
//! optional client connector to encrypt the proxy→backend connection.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error, SignatureScheme};
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// Build a TLS acceptor from a PEM certificate chain and private key.
pub fn acceptor(cert: &Path, key: &Path) -> anyhow::Result<TlsAcceptor> {
    let certs = rustls_pemfile::certs(&mut BufReader::new(
        File::open(cert).with_context(|| format!("opening cert {}", cert.display()))?,
    ))
    .collect::<Result<Vec<_>, _>>()
    .with_context(|| format!("parsing certs in {}", cert.display()))?;

    let key = rustls_pemfile::private_key(&mut BufReader::new(
        File::open(key).with_context(|| format!("opening key {}", key.display()))?,
    ))
    .with_context(|| format!("parsing key {}", key.display()))?
    .ok_or_else(|| anyhow::anyhow!("no private key found in {}", key.display()))?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building rustls server config")?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// rustls client config for the proxy→backend connection. With `insecure` the
/// certificate is not verified (encrypt-only, like `sslmode=require`); otherwise
/// it is verified against the system trust store (like `sslmode=verify-full`).
pub fn backend_client_config(insecure: bool) -> anyhow::Result<Arc<rustls::ClientConfig>> {
    let config = if insecure {
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        let loaded = rustls_native_certs::load_native_certs();
        for cert in loaded.certs {
            let _ = roots.add(cert);
        }
        if roots.is_empty() {
            anyhow::bail!("no system root certificates found for backend TLS verification");
        }
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };
    Ok(Arc::new(config))
}

/// Connector for the proxy→backend data path.
pub fn backend_connector(insecure: bool) -> anyhow::Result<TlsConnector> {
    Ok(TlsConnector::from(backend_client_config(insecure)?))
}

/// Certificate verifier that accepts any server certificate (encrypt-only).
#[derive(Debug)]
struct NoVerify;

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        use SignatureScheme::*;
        vec![
            RSA_PKCS1_SHA256, RSA_PKCS1_SHA384, RSA_PKCS1_SHA512,
            ECDSA_NISTP256_SHA256, ECDSA_NISTP384_SHA384, ECDSA_NISTP521_SHA512,
            RSA_PSS_SHA256, RSA_PSS_SHA384, RSA_PSS_SHA512, ED25519,
        ]
    }
}
