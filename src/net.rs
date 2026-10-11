//! Connections to mail servers: TCP, then TLS (rustls with ring, checked
//! against the system's trusted certificates), from the start or after
//! STARTTLS. Nothing private is sent before TLS is up, except to a server
//! on this computer when an account says `security = "none"`.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::account::{self, Security};
use crate::config;

/// How long a connection, or the TLS handshake, may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// A connection, encrypted or (only to this computer) not.
#[derive(Debug)]
pub enum Conn {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_flush(cx),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

impl Conn {
    /// The plain TCP connection under it, for STARTTLS.
    pub fn into_plain(self) -> Result<TcpStream> {
        match self {
            Conn::Plain(s) => Ok(s),
            Conn::Tls(_) => bail!("the connection is already encrypted"),
        }
    }
}

/// Where to connect and how to check the server.
#[derive(Clone)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub tls: Arc<ClientConfig>,
}

impl Endpoint {
    /// `host:port` as shown in messages.
    pub fn shown(&self) -> String {
        format!("{}:{}", crate::text::one_line(&self.host, 80), self.port)
    }
}

/// Opens the TCP connection, and for `Security::Tls` the TLS session on it.
/// STARTTLS endpoints come back plain: the protocol asks for TLS first
/// ([`start_tls`]).
pub async fn connect(endpoint: &Endpoint) -> Result<Conn> {
    if endpoint.security == Security::None && !account::is_local(&endpoint.host) {
        bail!("refusing to connect to {} without TLS", endpoint.shown());
    }
    let host = endpoint.host.trim_matches(|c| c == '[' || c == ']');
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, endpoint.port)))
        .await
        .map_err(|_| anyhow!("{} didn't answer", endpoint.shown()))?
        .with_context(|| format!("cannot connect to {}", endpoint.shown()))?;
    let _ = tcp.set_nodelay(true);
    match endpoint.security {
        Security::Tls => start_tls(tcp, endpoint).await,
        Security::Starttls | Security::None => Ok(Conn::Plain(tcp)),
    }
}

/// TLS over `tcp`, the server checked as `endpoint.host`.
pub async fn start_tls(tcp: TcpStream, endpoint: &Endpoint) -> Result<Conn> {
    let host = endpoint.host.trim_matches(|c| c == '[' || c == ']');
    let name = ServerName::try_from(host.to_string())
        .map_err(|_| anyhow!("{} isn't a server name", endpoint.shown()))?;
    let connector = TlsConnector::from(endpoint.tls.clone());
    let tls = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(name, tcp))
        .await
        .map_err(|_| anyhow!("{} didn't finish the TLS handshake", endpoint.shown()))?
        .map_err(|e| anyhow!("TLS with {} failed: {e}", endpoint.shown()))?;
    Ok(Conn::Tls(Box::new(tls)))
}

/// The TLS settings for an account: the system's trusted certificates, and
/// if `certificate` names a PEM file, the certificates in it too. A
/// certificate in that file is also accepted as the server's own when it's
/// the very one presented (pinned), for servers like Proton Mail Bridge
/// that make their own.
pub fn tls_config(certificate: Option<&Path>) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let extra = match certificate {
        Some(path) => read_certificates(path)?,
        None => Vec::new(),
    };
    let platform =
        rustls_platform_verifier::Verifier::new_with_extra_roots(extra.clone(), provider.clone())
            .map_err(|e| anyhow!("cannot use the system's certificates: {e}"))?;
    let verifier: Arc<dyn ServerCertVerifier> = if extra.is_empty() {
        Arc::new(platform)
    } else {
        Arc::new(Pinned {
            pinned: extra,
            inner: Arc::new(platform),
            provider: provider.clone(),
        })
    };
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow!("TLS: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// The certificates in a PEM file: at most 1 MB, a plain file.
fn read_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let shown = config::shown(path);
    let meta = std::fs::metadata(path).with_context(|| format!("cannot read {shown}"))?;
    if !meta.is_file() || meta.len() > 1 << 20 {
        bail!("{shown} isn't a certificate file");
    }
    let pem = std::fs::read(path).with_context(|| format!("cannot read {shown}"))?;
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&pem)
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow!("{shown} isn't a PEM certificate: {e}"))?;
    if certs.is_empty() {
        bail!("{shown} has no certificate in it");
    }
    Ok(certs)
}

/// Accepts the server's certificate if it's byte for byte one of
/// `pinned`; otherwise asks the system's verifier. The handshake's
/// signatures are still checked against the certificate either way.
#[derive(Debug)]
struct Pinned {
    pinned: Vec<CertificateDer<'static>>,
    inner: Arc<dyn ServerCertVerifier>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if self
            .pinned
            .iter()
            .any(|pin| pin.as_ref() == end_entity.as_ref())
        {
            return Ok(ServerCertVerified::assertion());
        }
        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Reads one line ending in `\n` (CRLF or LF) from `reader`, at most `max`
/// bytes; without the line break. A longer line, or the connection closing
/// first, is an error: a server can't make tuimail hold more.
pub async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> io::Result<Vec<u8>> {
    use tokio::io::AsyncBufReadExt;
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the server closed the connection",
            ));
        }
        let (used, done) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        if line.len() + used > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the server sent a line too long",
            ));
        }
        line.extend_from_slice(&available[..used]);
        reader.consume(used);
        if done {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lines_are_read_whole_and_capped() {
        let data: &[u8] = b"+OK hello\r\nsecond\nthird";
        let mut reader = tokio::io::BufReader::with_capacity(4, data);
        assert_eq!(read_line(&mut reader, 100).await.unwrap(), b"+OK hello");
        assert_eq!(read_line(&mut reader, 100).await.unwrap(), b"second");
        assert!(read_line(&mut reader, 100).await.is_err(), "no line break");
        let long: &[u8] = b"0123456789\r\n";
        let mut reader = tokio::io::BufReader::new(long);
        assert!(read_line(&mut reader, 5).await.is_err());
    }

    #[tokio::test]
    async fn plain_connections_go_only_to_this_computer() {
        let endpoint = Endpoint {
            host: "imap.example.org".into(),
            port: 143,
            security: Security::None,
            tls: tls_config(None).unwrap(),
        };
        let error = connect(&endpoint).await.unwrap_err().to_string();
        assert!(error.contains("without TLS"), "{error}");
    }

    #[test]
    fn a_certificate_file_must_hold_a_certificate() {
        let dir = std::env::temp_dir().join(format!("tuimail-cert-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("cert.pem");
        std::fs::write(&file, "not a certificate").unwrap();
        assert!(tls_config(Some(&file)).is_err());
        assert!(tls_config(Some(&dir)).is_err(), "a folder");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
