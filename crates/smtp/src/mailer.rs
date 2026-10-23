//! The transport: one SMTP conversation per message with lettre's
//! `AsyncSmtpTransport` (Tokio, rustls with the platform verifier), `send_raw` of the
//! stored bytes.

use std::error::Error as _;
use std::fmt;
use std::net::Ipv4Addr;

use async_trait::async_trait;
use lettre::transport::smtp::authentication::{Credentials as LettreCredentials, Mechanism};
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::transport::smtp::extension::ClientId;
use lettre::{AsyncSmtpTransport, AsyncTransport as _, Tokio1Executor};
use store::OutboxEnvelope;

use crate::config::{Credentials, Security, SmtpConfig};
use crate::error::SmtpError;

/// Submits raw messages. [`LettreMailer`] in production; tests use fakes.
#[async_trait]
pub trait Mailer: Send + Sync + fmt::Debug {
    /// Connects, logs in with `credentials` and submits `raw` to the envelope's
    /// recipients. `Ok` means the server accepted the message (`250` after `DATA`).
    async fn send(
        &self,
        config: &SmtpConfig,
        credentials: &Credentials,
        envelope: &OutboxEnvelope,
        raw: &[u8],
    ) -> Result<(), SmtpError>;

    /// Connects and logs in without sending anything ("Test Connection").
    async fn check(&self, config: &SmtpConfig, credentials: &Credentials) -> Result<(), SmtpError>;
}

/// The real network: lettre over Tokio and rustls.
///
/// Every call opens its own connection (no pool): mail is sent rarely, and a fresh
/// connection always logs in with the current credentials.
#[derive(Debug, Clone, Copy, Default)]
pub struct LettreMailer;

impl LettreMailer {
    /// The transport for `config` logging in with `credentials`.
    fn transport(
        config: &SmtpConfig,
        credentials: &Credentials,
    ) -> Result<AsyncSmtpTransport<Tokio1Executor>, SmtpError> {
        let tls = match config.security {
            Security::PlainInsecure => Tls::None,
            Security::Tls | Security::StartTls => {
                let parameters = TlsParameters::builder(config.host.clone())
                    .build_rustls()
                    .map_err(|err| SmtpError::Tls {
                        message: err.to_string(),
                    })?;
                if config.security == Security::Tls {
                    Tls::Wrapper(parameters)
                } else {
                    // Required, never Opportunistic: no silent fallback to plain text.
                    Tls::Required(parameters)
                }
            }
        };
        let hello = config
            .hello_name
            .clone()
            .map_or(ClientId::Ipv4(Ipv4Addr::LOCALHOST), ClientId::Domain);
        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host)
            .port(config.port)
            .tls(tls)
            .timeout(Some(config.timeout))
            .hello_name(hello);
        match credentials {
            Credentials::None => {}
            Credentials::Password { user, password } => {
                builder = builder
                    .credentials(LettreCredentials::new(
                        user.clone(),
                        password.expose().clone(),
                    ))
                    .authentication(vec![Mechanism::Plain, Mechanism::Login]);
            }
            Credentials::XOAuth2 { user, access_token } => {
                // The string lettre sends is `auth::sasl::xoauth2`'s; checking it here
                // rejects user names or tokens that would break the SASL framing.
                auth::sasl::xoauth2(user, access_token.expose()).map_err(|err| {
                    SmtpError::Credentials {
                        message: format!("the sign-in can't be used for sending: {err}"),
                        needs_user: true,
                    }
                })?;
                builder = builder
                    .credentials(LettreCredentials::new(
                        user.clone(),
                        access_token.expose().clone(),
                    ))
                    .authentication(vec![Mechanism::Xoauth2]);
            }
        }
        Ok(builder.build())
    }
}

/// The lettre envelope; address texts never end up in the error.
fn envelope(envelope: &OutboxEnvelope) -> Result<lettre::address::Envelope, SmtpError> {
    let invalid = |what: &str| SmtpError::InvalidAddress {
        message: what.to_owned(),
    };
    let from = envelope
        .from
        .trim()
        .parse::<lettre::Address>()
        .map_err(|_| invalid("the sender's address"))?;
    let to = envelope
        .to
        .iter()
        .map(|to| to.trim().parse::<lettre::Address>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid("a recipient's address"))?;
    lettre::address::Envelope::new(Some(from), to).map_err(|_| invalid("no recipients"))
}

/// Maps lettre's error: server replies through [`SmtpError::from_reply`] (the reply text
/// is the error's source), TLS, timeouts and everything else as a connection problem.
pub(crate) fn from_lettre(err: &lettre::transport::smtp::Error) -> SmtpError {
    let source = err.source().map(ToString::to_string).unwrap_or_default();
    if let Some(code) = err.status() {
        return SmtpError::from_reply(u16::from(code), &source);
    }
    if err.is_timeout() {
        return SmtpError::Timeout;
    }
    if err.is_tls() {
        return SmtpError::Tls {
            message: err.to_string(),
        };
    }
    if err.is_client() {
        if source.contains("authentication mechanism") {
            return SmtpError::AuthFailed {
                message: "the server offers no login method Maze supports".to_owned(),
            };
        }
        if source.contains("non-ascii") {
            return SmtpError::Rejected {
                code: 0,
                message: source,
                permanent: true,
            };
        }
    }
    SmtpError::Connection {
        message: err.to_string(),
    }
}

#[async_trait]
impl Mailer for LettreMailer {
    async fn send(
        &self,
        config: &SmtpConfig,
        credentials: &Credentials,
        envelope_in: &OutboxEnvelope,
        raw: &[u8],
    ) -> Result<(), SmtpError> {
        let envelope = envelope(envelope_in)?;
        let transport = Self::transport(config, credentials)?;
        transport
            .send_raw(&envelope, raw)
            .await
            .map(|_| ())
            .map_err(|err| from_lettre(&err))
    }

    async fn check(&self, config: &SmtpConfig, credentials: &Credentials) -> Result<(), SmtpError> {
        // Opening a connection logs in when credentials are set; then NOOP and QUIT.
        let transport = Self::transport(config, credentials)?;
        match transport.test_connection().await {
            Ok(true) => Ok(()),
            Ok(false) => Err(SmtpError::Connection {
                message: format!("{} closed the connection", config.address()),
            }),
            Err(err) => Err(from_lettre(&err)),
        }
    }
}
