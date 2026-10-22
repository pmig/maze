//! Byte transport under the IMAP session: TCP, TLS, DEFLATE, or any test stream,
//! erased into one type so that STARTTLS and COMPRESS can swap layers.

use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use rustls::ClientConfig;
use rustls::RootCertStore;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_rustls::TlsConnector;

use crate::config::TlsVerification;
use crate::error::{ImapError, Result};

/// Anything that can carry an IMAP connection.
pub(crate) trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Transport for T {}

/// A type-erased transport. `Debug` prints nothing about the peer or the data.
pub(crate) struct BoxedStream(Box<dyn Transport>);

impl BoxedStream {
    pub(crate) fn new(stream: impl Transport + 'static) -> Self {
        Self(Box::new(stream))
    }
}

impl fmt::Debug for BoxedStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BoxedStream")
    }
}

impl AsyncRead for BoxedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for BoxedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.0).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

/// Build a rustls client configuration with an explicit `ring` provider, so that it
/// does not depend on a process-wide default provider being installed.
pub(crate) fn tls_config(verification: &TlsVerification) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| ImapError::Tls(e.to_string()))?;
    let config = match verification {
        TlsVerification::Platform => {
            let verifier = rustls_platform_verifier::Verifier::new(provider)
                .map_err(|e| ImapError::Tls(e.to_string()))?;
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth()
        }
        TlsVerification::WebPkiRoots => {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            builder.with_root_certificates(roots).with_no_client_auth()
        }
        TlsVerification::CustomRoots(certs) => {
            let mut roots = RootCertStore::empty();
            for cert in certs {
                roots
                    .add(cert.clone())
                    .map_err(|e| ImapError::Tls(format!("invalid root certificate: {e}")))?;
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
    };
    Ok(Arc::new(config))
}

/// Run a TLS handshake over `stream`, verifying the certificate for `host`.
pub(crate) async fn tls_handshake(
    stream: BoxedStream,
    host: &str,
    verification: &TlsVerification,
) -> Result<BoxedStream> {
    let config = tls_config(verification)?;
    let server_name = ServerName::try_from(host.to_owned())
        .map_err(|_| ImapError::InvalidInput(format!("invalid TLS server name {host:?}")))?;
    let tls = TlsConnector::from(config)
        .connect(server_name, stream)
        .await
        .map_err(|e| {
            // rustls reports certificate problems as `InvalidData` I/O errors.
            if e.kind() == io::ErrorKind::InvalidData {
                ImapError::Tls(e.to_string())
            } else {
                ImapError::Io(e)
            }
        })?;
    Ok(BoxedStream::new(tls))
}
