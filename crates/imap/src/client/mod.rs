//! [`ImapClient`]: the real [`ImapSession`] over Tokio, rustls and async-imap.
//!
//! async-imap provides the connection plumbing (greeting, `LOGIN`, `AUTHENTICATE`,
//! `COMPRESS`) and the response parser (imap-proto). Every command after login is
//! sent raw through [`Wire`] and its responses are interpreted here, because the
//! library's typed helpers drop response codes such as `APPENDUID`, `COPYUID`,
//! `HIGHESTMODSEQ` and the untagged `VANISHED`/`FETCH` data of a QRESYNC select.

mod convert;
mod transport;
mod wire;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use async_imap::imap_proto::{Capability, MailboxDatum, RequestId, Response, Status};
use async_imap::types::UnsolicitedResponse;
use async_imap::{Authenticator, Client};
use async_trait::async_trait;
use bytes::Bytes;
use mazemail_types::Flags;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

use self::convert::{FetchAttrs, remote_folder};
use self::transport::{BoxedStream, tls_handshake};
use self::wire::{Code, Wire, split_outcome, tagged_result};
use crate::config::{ImapAuth, ImapConfig, Security};
use crate::error::{ImapError, Result};
use crate::gmail::{LabelOp, quote};
use crate::session::ImapSession;
use crate::types::{
    AppendUid, Caps, ChangeSet, CopyUid, FlagChange, FlagOp, FolderStatus, HeaderFetch,
    IdleOutcome, QresyncArgs, RemoteFolder, SelectInfo, SessionEvent,
};
use crate::uid_set::UidSet;
use crate::utf7;

/// Unsolicited events kept before the oldest are dropped (see [`SessionEvent::Overflow`]).
const MAX_EVENTS: usize = 10_000;

/// An authenticated connection to a real IMAP server.
///
/// ```no_run
/// # async fn demo() -> imap::Result<()> {
/// use imap::{ImapAuth, ImapClient, ImapConfig, ImapSession};
///
/// let config = ImapConfig::new("imap.example.org", 993, ImapAuth::password("alice", "secret"));
/// let mut session = ImapClient::connect(&config).await?;
/// let folders = session.list().await?;
/// let inbox = session.select("INBOX", None).await?;
/// # let _ = (folders, inbox);
/// session.logout().await?;
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct ImapClient {
    wire: Wire,
    state: State,
}

/// Per-session protocol state, kept apart from [`Wire`] so response handlers can
/// update it while the wire is borrowed for reading.
#[derive(Debug, Default)]
struct State {
    caps: Caps,
    qresync_enabled: bool,
    /// Wire name of the selected folder.
    selected: Option<String>,
    events: Vec<SessionEvent>,
    overflowed: bool,
    /// Decoded folder name → name as the server sent it, from the last `LIST`.
    names: HashMap<String, String>,
}

impl State {
    fn push_event(&mut self, event: SessionEvent) {
        if self.overflowed {
            return;
        }
        if self.events.len() >= MAX_EVENTS {
            tracing::warn!("imap unsolicited event queue overflowed");
            self.events.clear();
            self.events.push(SessionEvent::Overflow);
            self.overflowed = true;
            return;
        }
        self.events.push(event);
    }

    fn has_change_events(&self) -> bool {
        self.events
            .iter()
            .any(|e| !matches!(e, SessionEvent::Alert(_) | SessionEvent::Recent(_)))
    }

    /// Whether an untagged response reports a change in the selected folder.
    fn is_change(resp: &Response<'_>) -> bool {
        matches!(
            resp,
            Response::MailboxData(MailboxDatum::Exists(_))
                | Response::Expunge(_)
                | Response::Vanished { .. }
                | Response::Fetch(..)
        )
    }

    /// Queue an untagged response that no command handler consumed.
    fn record(&mut self, resp: &Response<'_>) {
        match resp {
            Response::MailboxData(MailboxDatum::Exists(n)) => {
                self.push_event(SessionEvent::Exists(*n));
            }
            Response::MailboxData(MailboxDatum::Recent(n)) => {
                self.push_event(SessionEvent::Recent(*n));
            }
            Response::Expunge(seq) => self.push_event(SessionEvent::Expunge(*seq)),
            Response::Vanished { uids, .. } => {
                self.push_event(SessionEvent::Vanished(ranges_to_set(uids)));
            }
            Response::Fetch(seq, attrs) => {
                let attrs = FetchAttrs::collect(attrs);
                if attrs.flags.is_some() {
                    let (flags, keywords) = attrs.split_flags();
                    self.push_event(SessionEvent::Flags {
                        seq: *seq,
                        uid: attrs.uid,
                        flags,
                        keywords,
                        modseq: attrs.modseq,
                    });
                }
            }
            Response::Data { status, outcome } => {
                let (code, text) = split_outcome(outcome);
                if code == Some(Code::Alert) {
                    self.push_event(SessionEvent::Alert(text));
                } else if *status != Status::Bye {
                    tracing::trace!(?status, "imap untagged status ignored");
                }
            }
            Response::Capabilities(caps) if caps.contains(&Capability::Imap4rev1) => {
                self.caps = caps_from(caps);
            }
            _ => tracing::trace!("imap untagged response ignored"),
        }
    }

    fn wire_name(&self, folder: &str) -> String {
        if folder.eq_ignore_ascii_case("INBOX") {
            "INBOX".to_owned()
        } else if let Some(raw) = self.names.get(folder) {
            raw.clone()
        } else {
            utf7::encode(folder)
        }
    }
}

fn caps_from(caps: &[Capability<'_>]) -> Caps {
    Caps::from_names(caps.iter().map(|c| match c {
        Capability::Imap4rev1 => "IMAP4rev1".to_owned(),
        Capability::Auth(m) => format!("AUTH={m}"),
        Capability::Atom(a) => a.to_string(),
    }))
}

fn ranges_to_set(ranges: &[std::ops::RangeInclusive<u32>]) -> UidSet {
    let mut set = UidSet::new();
    for r in ranges {
        set.insert_range(*r.start(), *r.end());
    }
    set
}

/// Quote a string argument, rejecting characters that cannot appear in a quoted string.
fn quote_arg(s: &str) -> Result<String> {
    if s.contains(['\r', '\n', '\0']) {
        return Err(ImapError::InvalidInput(
            "line breaks and NUL are not allowed in IMAP strings".into(),
        ));
    }
    Ok(quote(s))
}

/// Whether `s` is a valid IMAP keyword (an atom that is not a system flag).
pub(crate) fn validate_keyword(s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && !s.starts_with('\\')
        && s.bytes().all(|b| {
            b.is_ascii_graphic() && !matches!(b, b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b']')
        });
    if ok {
        Ok(())
    } else {
        Err(ImapError::InvalidInput(format!("invalid keyword {s:?}")))
    }
}

/// Validate a `BODY[section]` spec: part numbers and `HEADER`/`TEXT`/`MIME`.
fn validate_section(s: &str) -> Result<()> {
    let ok = s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.');
    if ok {
        Ok(())
    } else {
        Err(ImapError::InvalidInput(format!(
            "invalid body section {s:?}"
        )))
    }
}

enum Step<T> {
    Done(Result<Option<Code>>),
    Continue,
    Other(T),
}

struct PlainAuth<'a> {
    user: &'a str,
    password: &'a str,
    sent: bool,
}

impl Authenticator for PlainAuth<'_> {
    type Response = Vec<u8>;

    fn process(&mut self, _challenge: &[u8]) -> Vec<u8> {
        if std::mem::replace(&mut self.sent, true) {
            return Vec::new();
        }
        format!("\0{}\0{}", self.user, self.password).into_bytes()
    }
}

struct XOAuth2<'a> {
    user: &'a str,
    token: &'a str,
    sent: bool,
}

impl Authenticator for XOAuth2<'_> {
    type Response = Vec<u8>;

    /// The first call sends the token. If the server rejects it, it sends a JSON
    /// error as a challenge and expects an empty response before the tagged `NO`.
    fn process(&mut self, _challenge: &[u8]) -> Vec<u8> {
        if std::mem::replace(&mut self.sent, true) {
            return Vec::new();
        }
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token).into_bytes()
    }
}

async fn within<T>(
    limit: Duration,
    operation: &'static str,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(limit, fut)
        .await
        .map_err(|_| ImapError::Timeout {
            operation,
            after: limit,
        })?
}

fn auth_error(err: async_imap::error::Error) -> ImapError {
    match err {
        async_imap::error::Error::No(text) | async_imap::error::Error::Bad(text) => {
            ImapError::Auth(text)
        }
        other => other.into(),
    }
}

/// Read the server greeting; returns capabilities if the greeting carried them.
async fn read_greeting(client: &mut Client<BoxedStream>) -> Result<Option<Caps>> {
    let response = client
        .read_response()
        .await
        .map_err(ImapError::from_stream_io)?
        .ok_or(ImapError::ConnectionLost(None))?;
    match response.parsed() {
        Response::Data {
            status: Status::Ok,
            outcome,
        } => Ok(match &outcome.code {
            Some(async_imap::imap_proto::ResponseCode::Capabilities(caps)) => Some(caps_from(caps)),
            _ => None,
        }),
        Response::Data {
            status: Status::PreAuth,
            ..
        } => Err(ImapError::Unsupported("PREAUTH greeting")),
        Response::Data {
            status: Status::Bye,
            outcome,
        } => Err(ImapError::ConnectionLost(Some(split_outcome(outcome).1))),
        _ => Err(ImapError::Protocol("unexpected server greeting".into())),
    }
}

/// `CAPABILITY` before authentication.
async fn query_caps(client: &mut Client<BoxedStream>) -> Result<Caps> {
    let (tx, rx) = async_channel::unbounded();
    client
        .run_command_and_check_ok("CAPABILITY", Some(tx))
        .await?;
    let mut caps = None;
    while let Ok(response) = rx.try_recv() {
        if let UnsolicitedResponse::Other(data) = response
            && let Response::Capabilities(list) = data.parsed()
        {
            caps = Some(caps_from(list));
        }
    }
    caps.ok_or_else(|| ImapError::Protocol("no CAPABILITY response".into()))
}

async fn authenticate(
    client: Client<BoxedStream>,
    auth: &ImapAuth,
    caps: &Caps,
) -> Result<async_imap::Session<BoxedStream>> {
    match auth {
        ImapAuth::Password { user, password } => {
            if caps.has_auth("PLAIN") {
                let authenticator = PlainAuth {
                    user,
                    password: password.expose(),
                    sent: false,
                };
                client
                    .authenticate("PLAIN", authenticator)
                    .await
                    .map_err(|(e, _)| auth_error(e))
            } else if caps.login_disabled() {
                Err(ImapError::Auth(
                    "server disables LOGIN and offers no AUTH=PLAIN".into(),
                ))
            } else {
                client
                    .login(user, password.expose())
                    .await
                    .map_err(|(e, _)| auth_error(e))
            }
        }
        ImapAuth::XOAuth2 { user, access_token } => {
            if !caps.has_auth("XOAUTH2") {
                return Err(ImapError::Unsupported("AUTH=XOAUTH2"));
            }
            let authenticator = XOAuth2 {
                user,
                token: access_token.expose(),
                sent: false,
            };
            client
                .authenticate("XOAUTH2", authenticator)
                .await
                .map_err(|(e, _)| auth_error(e))
        }
    }
}

impl ImapClient {
    /// Connect over TCP to `config.host:config.port`, secure the connection as
    /// configured, authenticate, and negotiate extensions (`ENABLE QRESYNC`, `ID`,
    /// optionally `COMPRESS=DEFLATE`).
    pub async fn connect(config: &ImapConfig) -> Result<Self> {
        let tcp = within(config.connect_timeout, "connect", async {
            TcpStream::connect((config.host.as_str(), config.port))
                .await
                .map_err(ImapError::Io)
        })
        .await?;
        // Small command lines should not wait for Nagle.
        if let Err(e) = tcp.set_nodelay(true) {
            tracing::debug!(error = %e, "could not set TCP_NODELAY");
        }
        Self::connect_stream(tcp, config).await
    }

    /// Like [`connect`](Self::connect), over an already established byte stream
    /// (a tunnel, a proxy connection, or a test transport). `config.security` is
    /// applied on top: [`Security::Tls`] runs a TLS handshake over `stream`,
    /// [`Security::StartTls`] upgrades after the greeting. `host` and `port` are
    /// only used for certificate verification.
    pub async fn connect_stream<S>(stream: S, config: &ImapConfig) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let limit = config.connect_timeout;
        let mut stream = BoxedStream::new(stream);
        if config.security == Security::Tls {
            stream = within(
                limit,
                "TLS handshake",
                tls_handshake(stream, &config.host, &config.tls_verification),
            )
            .await?;
        }
        let mut client = Client::new(stream);
        let mut caps = within(limit, "greeting", read_greeting(&mut client)).await?;

        if config.security == Security::StartTls {
            let pre = match caps {
                Some(caps) => caps,
                None => within(limit, "CAPABILITY", query_caps(&mut client)).await?,
            };
            if !pre.starttls() {
                return Err(ImapError::Unsupported("STARTTLS"));
            }
            within(limit, "STARTTLS", async {
                client
                    .run_command_and_check_ok("STARTTLS", None)
                    .await
                    .map_err(ImapError::from)
            })
            .await?;
            // Anything the server sent after the OK is discarded with the old
            // buffer, which prevents STARTTLS response injection.
            let tls = within(
                limit,
                "TLS handshake",
                tls_handshake(client.into_inner(), &config.host, &config.tls_verification),
            )
            .await?;
            client = Client::new(tls);
            caps = None;
        }

        let pre_auth_caps = match caps {
            Some(caps) => caps,
            None => within(limit, "CAPABILITY", query_caps(&mut client)).await?,
        };
        let session = within(
            limit,
            "authentication",
            authenticate(client, &config.auth, &pre_auth_caps),
        )
        .await?;
        tracing::debug!("imap authenticated");

        let mut this = Self {
            wire: Wire::new(session, config.command_timeout),
            state: State::default(),
        };
        this.refresh_capabilities().await?;
        if config.compress && this.state.caps.compress_deflate() {
            this = this.start_compression().await?;
        }
        if let Some(fields) = &config.client_id
            && this.state.caps.id()
        {
            this.send_id(fields).await?;
        }
        if this.state.caps.qresync() && this.state.caps.enable() {
            this.enable_qresync().await?;
        }
        Ok(this)
    }

    /// The capabilities as of the last refresh.
    pub fn caps(&self) -> &Caps {
        &self.state.caps
    }

    /// Whether `ENABLE QRESYNC` succeeded.
    pub fn qresync_enabled(&self) -> bool {
        self.state.qresync_enabled
    }

    /// Whether the server is Gmail.
    pub fn is_gmail(&self) -> bool {
        self.state.caps.gmail()
    }

    /// Whether an earlier error left the session unusable.
    pub fn is_broken(&self) -> bool {
        self.wire.is_broken()
    }

    /// Send `command` and read responses until its tagged completion. Untagged
    /// responses go to `on_data` first; those it doesn't consume become events.
    async fn run<F>(
        &mut self,
        op: &'static str,
        command: &str,
        mut on_data: F,
    ) -> Result<Option<Code>>
    where
        F: FnMut(&Response<'_>) -> bool + Send,
    {
        let id = self.wire.send(command, op).await?;
        self.finish(op, &id, &mut on_data).await
    }

    async fn finish<F>(
        &mut self,
        op: &'static str,
        id: &RequestId,
        on_data: &mut F,
    ) -> Result<Option<Code>>
    where
        F: FnMut(&Response<'_>) -> bool + Send,
    {
        let limit = Some(self.wire.command_timeout());
        loop {
            let state = &mut self.state;
            let step = self
                .wire
                .read_with(op, limit, |resp| match resp {
                    Response::Done {
                        tag,
                        status,
                        outcome,
                    } if tag == id => Step::Done(tagged_result(op, status, outcome)),
                    Response::Done { .. } => Step::Other(()),
                    Response::Continue(_) => Step::Continue,
                    other => {
                        if !on_data(other) {
                            state.record(other);
                        }
                        Step::Other(())
                    }
                })
                .await?;
            match step {
                Step::Done(result) => return result,
                Step::Other(()) => {}
                Step::Continue => {
                    self.wire.mark_broken();
                    return Err(ImapError::Protocol(format!(
                        "unexpected continuation request during {op}"
                    )));
                }
            }
        }
    }

    /// Wait for the `+` continuation of `id`. Returns `Err` if the server completed
    /// the command instead (e.g. `NO [TRYCREATE]` for APPEND).
    async fn await_continuation(&mut self, op: &'static str, id: &RequestId) -> Result<()> {
        let limit = Some(self.wire.command_timeout());
        loop {
            let state = &mut self.state;
            let step = self
                .wire
                .read_with(op, limit, |resp| match resp {
                    Response::Continue(_) => Step::Continue,
                    Response::Done {
                        tag,
                        status,
                        outcome,
                    } if tag == id => Step::Done(tagged_result(op, status, outcome)),
                    other => {
                        state.record(other);
                        Step::Other(())
                    }
                })
                .await?;
            match step {
                Step::Continue => return Ok(()),
                Step::Done(result) => {
                    result?;
                    return Err(ImapError::Protocol(format!(
                        "{op} completed without a continuation request"
                    )));
                }
                Step::Other(()) => {}
            }
        }
    }

    async fn refresh_capabilities(&mut self) -> Result<()> {
        let mut caps = None;
        self.run("CAPABILITY", "CAPABILITY", |resp| match resp {
            Response::Capabilities(list) => {
                caps = Some(caps_from(list));
                true
            }
            _ => false,
        })
        .await?;
        self.state.caps =
            caps.ok_or_else(|| ImapError::Protocol("no CAPABILITY response".into()))?;
        Ok(())
    }

    async fn start_compression(self) -> Result<Self> {
        let Self { wire, state } = self;
        let limit = wire.command_timeout();
        let session = wire.into_session();
        let session = within(limit, "COMPRESS", async {
            session
                .compress(BoxedStream::new)
                .await
                .map_err(ImapError::from)
        })
        .await?;
        tracing::debug!("imap COMPRESS=DEFLATE active");
        Ok(Self {
            wire: Wire::new(session, limit),
            state,
        })
    }

    async fn send_id(&mut self, fields: &[(String, String)]) -> Result<()> {
        let mut parts = Vec::with_capacity(fields.len() * 2);
        for (key, value) in fields {
            parts.push(quote_arg(key)?);
            parts.push(quote_arg(value)?);
        }
        let command = format!("ID ({})", parts.join(" "));
        match self
            .run("ID", &command, |resp| matches!(resp, Response::Id(_)))
            .await
        {
            Ok(_) => Ok(()),
            // ID is advisory; a server that refuses it still works.
            Err(e) if e.is_recoverable() => {
                tracing::debug!(error = %e, "imap ID refused");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    async fn enable_qresync(&mut self) -> Result<()> {
        let mut enabled = false;
        let result = self
            .run("ENABLE", "ENABLE QRESYNC", |resp| match resp {
                Response::Capabilities(list) => {
                    enabled |= list.iter().any(
                        |c| matches!(c, Capability::Atom(a) if a.eq_ignore_ascii_case("QRESYNC")),
                    );
                    true
                }
                _ => false,
            })
            .await;
        match result {
            Ok(_) => {
                self.state.qresync_enabled = enabled;
                Ok(())
            }
            Err(e) if e.is_recoverable() => {
                tracing::debug!(error = %e, "imap ENABLE QRESYNC refused");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn require_selected(&self) -> Result<()> {
        self.wire.ensure_usable()?;
        if self.state.selected.is_some() {
            Ok(())
        } else {
            Err(ImapError::NoFolderSelected)
        }
    }

    fn quoted_folder(&self, folder: &str) -> Result<(String, String)> {
        if folder.is_empty() || folder.chars().any(char::is_control) {
            return Err(ImapError::InvalidInput(
                "folder names must be non-empty and free of control characters".into(),
            ));
        }
        let wire_name = self.state.wire_name(folder);
        let quoted = quote_arg(&wire_name)?;
        Ok((wire_name, quoted))
    }

    /// Fetch one body section of one message.
    async fn fetch_body(&mut self, uid: u32, section: &str) -> Result<Bytes> {
        self.require_selected()?;
        validate_section(section)?;
        let command = format!("UID FETCH {uid} (UID BODY.PEEK[{section}])");
        let mut found: Option<Option<Bytes>> = None;
        self.run("UID FETCH", &command, |resp| {
            let Response::Fetch(_, attrs) = resp else {
                return false;
            };
            if !attrs
                .iter()
                .any(|a| matches!(a, async_imap::imap_proto::AttributeValue::Uid(u) if *u == uid))
            {
                return false;
            }
            for attr in attrs {
                use async_imap::imap_proto::AttributeValue as A;
                if let A::BodySection { data, .. } | A::Rfc822(data) = attr {
                    found = Some(data.as_deref().map(Bytes::copy_from_slice));
                    return true;
                }
            }
            false
        })
        .await?;
        match found {
            Some(Some(bytes)) => Ok(bytes),
            Some(None) => Err(ImapError::PartNotFound {
                uid,
                section: section.to_owned(),
            }),
            None => Err(ImapError::MessageNotFound { uid }),
        }
    }

    fn fetch_items(&self, base: &str) -> String {
        let mut items = base.to_owned();
        if self.state.caps.condstore() {
            items.push_str(" MODSEQ");
        }
        if self.state.caps.gmail() {
            items.push_str(" X-GM-LABELS");
        }
        items
    }
}

#[async_trait]
impl ImapSession for ImapClient {
    async fn capabilities(&mut self) -> Result<Caps> {
        self.wire.ensure_usable()?;
        Ok(self.state.caps.clone())
    }

    async fn list(&mut self) -> Result<Vec<RemoteFolder>> {
        let command = if self.state.caps.list_extended() && self.state.caps.special_use() {
            r#"LIST "" "*" RETURN (SPECIAL-USE)"#
        } else {
            r#"LIST "" "*""#
        };
        let mut folders = Vec::new();
        self.run("LIST", command, |resp| match resp {
            Response::MailboxData(MailboxDatum::List(data)) => {
                folders.push(remote_folder(data));
                true
            }
            _ => false,
        })
        .await?;
        let mut subscribed = HashSet::new();
        self.run("LSUB", r#"LSUB "" "*""#, |resp| match resp {
            Response::MailboxData(MailboxDatum::List(data)) => {
                subscribed.insert(data.name.to_string());
                true
            }
            _ => false,
        })
        .await?;
        self.state.names.clear();
        for folder in &mut folders {
            folder.subscribed = subscribed.contains(&folder.raw_name)
                || (folder.role == Some(mazemail_types::FolderRole::Inbox)
                    && subscribed.iter().any(|s| s.eq_ignore_ascii_case("INBOX")));
            self.state
                .names
                .insert(folder.name.clone(), folder.raw_name.clone());
        }
        tracing::debug!(count = folders.len(), "imap LIST");
        Ok(folders)
    }

    async fn select(&mut self, folder: &str, qresync: Option<QresyncArgs>) -> Result<SelectInfo> {
        let (wire_name, quoted) = self.quoted_folder(folder)?;
        let qresync = qresync
            .filter(|q| self.state.qresync_enabled && q.uidvalidity > 0 && q.highest_modseq > 0);
        let params = match &qresync {
            Some(q) => match q.known_uids.as_ref().filter(|k| !k.is_empty()) {
                Some(known) => format!(
                    " (QRESYNC ({} {} {known}))",
                    q.uidvalidity, q.highest_modseq
                ),
                None => format!(" (QRESYNC ({} {}))", q.uidvalidity, q.highest_modseq),
            },
            None if self.state.caps.condstore() => " (CONDSTORE)".to_owned(),
            None => String::new(),
        };
        let command = format!("SELECT {quoted}{params}");
        // A failed SELECT leaves no folder selected (RFC 3501 §6.3.1).
        self.state.selected = None;
        let mut info = SelectInfo::default();
        let mut saw_uidvalidity = false;
        let code = self
            .run("SELECT", &command, |resp| {
                select_collect(&mut info, &mut saw_uidvalidity, resp)
            })
            .await?;
        if code == Some(Code::ReadOnly) {
            info.read_only = true;
        }
        if !saw_uidvalidity {
            return Err(ImapError::Protocol(
                "SELECT response without UIDVALIDITY".into(),
            ));
        }
        info.qresync_applied = qresync.is_some_and(|q| q.uidvalidity == info.uidvalidity);
        if !info.qresync_applied {
            info.vanished = UidSet::new();
            info.changed.clear();
        }
        self.state.selected = Some(wire_name);
        tracing::debug!(
            exists = info.exists,
            qresync = info.qresync_applied,
            "imap SELECT"
        );
        Ok(info)
    }

    async fn fetch_changes(&mut self, since_modseq: u64) -> Result<ChangeSet> {
        self.require_selected()?;
        if !self.state.caps.condstore() {
            return Err(ImapError::Unsupported("CONDSTORE"));
        }
        let items = if self.state.caps.gmail() {
            "(UID FLAGS MODSEQ X-GM-LABELS)"
        } else {
            "(UID FLAGS MODSEQ)"
        };
        let modifier = if self.state.qresync_enabled {
            format!("(CHANGEDSINCE {since_modseq} VANISHED)")
        } else {
            format!("(CHANGEDSINCE {since_modseq})")
        };
        let command = format!("UID FETCH 1:* {items} {modifier}");
        let mut changes = ChangeSet::default();
        self.run("UID FETCH", &command, |resp| match resp {
            Response::Fetch(_, attrs) => match FetchAttrs::collect(attrs).flag_change() {
                Some(change) => {
                    changes.changed.push(change);
                    true
                }
                None => false,
            },
            Response::Vanished {
                earlier: true,
                uids,
            } => {
                for r in uids {
                    changes.vanished.insert_range(*r.start(), *r.end());
                }
                true
            }
            _ => false,
        })
        .await?;
        changes.changed.sort_by_key(|c| c.uid);
        changes.changed.dedup_by_key(|c| c.uid);
        Ok(changes)
    }

    async fn fetch_flags(&mut self, uids: &UidSet) -> Result<Vec<FlagChange>> {
        self.require_selected()?;
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let command = format!("UID FETCH {uids} ({})", self.fetch_items("UID FLAGS"));
        let mut out = Vec::new();
        self.run("UID FETCH", &command, |resp| match resp {
            Response::Fetch(_, attrs) => match FetchAttrs::collect(attrs).flag_change() {
                Some(change) if uids.contains(change.uid) => {
                    out.push(change);
                    true
                }
                _ => false,
            },
            _ => false,
        })
        .await?;
        out.sort_by_key(|c| c.uid);
        out.dedup_by_key(|c| c.uid);
        Ok(out)
    }

    async fn fetch_headers(&mut self, uids: &UidSet) -> Result<Vec<HeaderFetch>> {
        self.require_selected()?;
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let mut items =
            String::from("UID FLAGS RFC822.SIZE INTERNALDATE BODYSTRUCTURE BODY.PEEK[HEADER]");
        if self.state.caps.condstore() {
            items.push_str(" MODSEQ");
        }
        if self.state.caps.gmail() {
            items.push_str(" X-GM-MSGID X-GM-THRID X-GM-LABELS");
        }
        let command = format!("UID FETCH {uids} ({items})");
        let mut out = Vec::new();
        self.run("UID FETCH", &command, |resp| match resp {
            Response::Fetch(_, attrs) => match FetchAttrs::collect(attrs).header_fetch() {
                Some(fetch) => {
                    // A UID outside the set is the `n:*` artifact: a response to
                    // this command, not news.
                    if uids.contains(fetch.uid) {
                        out.push(fetch);
                    }
                    true
                }
                None => false,
            },
            _ => false,
        })
        .await?;
        out.sort_by_key(|h| h.uid);
        out.dedup_by_key(|h| h.uid);
        tracing::debug!(count = out.len(), "imap headers fetched");
        Ok(out)
    }

    async fn fetch_part(&mut self, uid: u32, section: &str) -> Result<Bytes> {
        if section.is_empty() {
            return Err(ImapError::InvalidInput(
                "empty section; use fetch_raw for the whole message".into(),
            ));
        }
        self.fetch_body(uid, section).await
    }

    async fn fetch_raw(&mut self, uid: u32) -> Result<Bytes> {
        self.fetch_body(uid, "").await
    }

    async fn uid_search_all(&mut self) -> Result<UidSet> {
        self.require_selected()?;
        let mut uids = Vec::new();
        self.run("UID SEARCH", "UID SEARCH ALL", |resp| match resp {
            Response::MailboxData(MailboxDatum::Search(found)) => {
                uids.extend_from_slice(found);
                true
            }
            _ => false,
        })
        .await?;
        Ok(uids.into_iter().collect())
    }

    async fn store_flags(&mut self, uids: &UidSet, op: &FlagOp) -> Result<()> {
        self.require_selected()?;
        for keyword in &op.keywords {
            validate_keyword(keyword)?;
        }
        let names = op.imap_names();
        if uids.is_empty() || (names.is_empty() && op.mode != crate::types::StoreMode::Set) {
            return Ok(());
        }
        let command = format!(
            "UID STORE {uids} {}FLAGS.SILENT ({})",
            op.mode.prefix(),
            names.join(" ")
        );
        self.run("UID STORE", &command, |_| false).await?;
        Ok(())
    }

    async fn store_gmail_labels(&mut self, uids: &UidSet, op: &LabelOp) -> Result<()> {
        self.require_selected()?;
        if !self.state.caps.gmail() {
            return Err(ImapError::Unsupported("X-GM-EXT-1"));
        }
        if uids.is_empty() || (op.labels.is_empty() && op.mode != crate::types::StoreMode::Set) {
            return Ok(());
        }
        let command = format!(
            "UID STORE {uids} {}X-GM-LABELS {}",
            op.mode.prefix(),
            op.wire_list()
        );
        // Gmail echoes the new label lists; they are the result of this command,
        // not news, so they are consumed here.
        self.run("UID STORE", &command, |resp| match resp {
            Response::Fetch(_, attrs) => FetchAttrs::collect(attrs).gm_labels.is_some(),
            _ => false,
        })
        .await?;
        Ok(())
    }

    async fn r#move(&mut self, uids: &UidSet, to: &str) -> Result<Option<CopyUid>> {
        self.require_selected()?;
        if uids.is_empty() {
            return Ok(None);
        }
        let (_, quoted) = self.quoted_folder(to)?;
        if !self.state.caps.has_move() {
            let copy = self.copy(uids, to).await?;
            self.store_flags(uids, &FlagOp::add(Flags::DELETED)).await?;
            if self.state.caps.uidplus() {
                self.expunge_uids(uids).await?;
            }
            return Ok(copy);
        }
        let command = format!("UID MOVE {uids} {quoted}");
        let mut copy_uid = None;
        // RFC 6851: COPYUID arrives in an untagged OK before the expunges.
        let tagged = self
            .run("UID MOVE", &command, |resp| match resp {
                Response::Data {
                    status: Status::Ok,
                    outcome,
                } => match split_outcome(outcome).0 {
                    Some(Code::CopyUid(c)) => {
                        copy_uid = Some(c);
                        true
                    }
                    _ => false,
                },
                _ => false,
            })
            .await?;
        if let Some(Code::CopyUid(c)) = tagged {
            copy_uid = Some(c);
        }
        Ok(copy_uid)
    }

    async fn copy(&mut self, uids: &UidSet, to: &str) -> Result<Option<CopyUid>> {
        self.require_selected()?;
        if uids.is_empty() {
            return Ok(None);
        }
        let (_, quoted) = self.quoted_folder(to)?;
        let command = format!("UID COPY {uids} {quoted}");
        let code = self.run("UID COPY", &command, |_| false).await?;
        Ok(match code {
            Some(Code::CopyUid(c)) => Some(c),
            _ => None,
        })
    }

    async fn expunge_uids(&mut self, uids: &UidSet) -> Result<()> {
        self.require_selected()?;
        if !self.state.caps.uidplus() {
            return Err(ImapError::Unsupported("UIDPLUS"));
        }
        if uids.is_empty() {
            return Ok(());
        }
        let command = format!("UID EXPUNGE {uids}");
        self.run("UID EXPUNGE", &command, |_| false).await?;
        Ok(())
    }

    async fn append(
        &mut self,
        folder: &str,
        raw: &[u8],
        flags: Flags,
    ) -> Result<Option<AppendUid>> {
        let (_, quoted) = self.quoted_folder(folder)?;
        let names = flags.to_imap();
        let flag_list = if names.is_empty() {
            String::new()
        } else {
            format!(" ({})", names.join(" "))
        };
        let literal_plus = self.state.caps.literal_plus();
        let command = format!(
            "APPEND {quoted}{flag_list} {{{}{}}}",
            raw.len(),
            if literal_plus { "+" } else { "" }
        );
        let id = self.wire.send(&command, "APPEND").await?;
        if !literal_plus {
            self.await_continuation("APPEND", &id).await?;
        }
        self.wire.write_literal(raw, "APPEND").await?;
        let code = self.finish("APPEND", &id, &mut |_| false).await?;
        Ok(match code {
            Some(Code::AppendUid(a)) => Some(a),
            _ => None,
        })
    }

    async fn create(&mut self, folder: &str) -> Result<()> {
        let (_, quoted) = self.quoted_folder(folder)?;
        self.run("CREATE", &format!("CREATE {quoted}"), |_| false)
            .await?;
        Ok(())
    }

    async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let (from_wire, from_quoted) = self.quoted_folder(from)?;
        let (_, to_quoted) = self.quoted_folder(to)?;
        self.run(
            "RENAME",
            &format!("RENAME {from_quoted} {to_quoted}"),
            |_| false,
        )
        .await?;
        self.state.names.remove(from);
        self.state.names.remove(to);
        if self.state.selected.as_deref() == Some(from_wire.as_str()) {
            self.state.selected = None;
        }
        Ok(())
    }

    async fn delete(&mut self, folder: &str) -> Result<()> {
        let (wire_name, quoted) = self.quoted_folder(folder)?;
        self.run("DELETE", &format!("DELETE {quoted}"), |_| false)
            .await?;
        self.state.names.remove(folder);
        if self.state.selected.as_deref() == Some(wire_name.as_str()) {
            self.state.selected = None;
        }
        Ok(())
    }

    async fn subscribe(&mut self, folder: &str, on: bool) -> Result<()> {
        let (_, quoted) = self.quoted_folder(folder)?;
        let (op, verb) = if on {
            ("SUBSCRIBE", "SUBSCRIBE")
        } else {
            ("UNSUBSCRIBE", "UNSUBSCRIBE")
        };
        self.run(op, &format!("{verb} {quoted}"), |_| false).await?;
        Ok(())
    }

    async fn status(&mut self, folder: &str) -> Result<FolderStatus> {
        let (wire_name, quoted) = self.quoted_folder(folder)?;
        let items = if self.state.caps.condstore() {
            "(MESSAGES UNSEEN UIDNEXT UIDVALIDITY HIGHESTMODSEQ)"
        } else {
            "(MESSAGES UNSEEN UIDNEXT UIDVALIDITY)"
        };
        let command = format!("STATUS {quoted} {items}");
        let mut status = FolderStatus::default();
        let mut seen = false;
        self.run("STATUS", &command, |resp| match resp {
            Response::MailboxData(MailboxDatum::Status {
                mailbox,
                status: attrs,
            }) if mailbox.as_ref() == wire_name
                || (wire_name == "INBOX" && mailbox.eq_ignore_ascii_case("INBOX")) =>
            {
                use async_imap::imap_proto::StatusAttribute as S;
                for attr in attrs {
                    match attr {
                        S::Messages(n) => status.messages = Some(*n),
                        S::Unseen(n) => status.unseen = Some(*n),
                        S::UidNext(n) => status.uidnext = Some(*n),
                        S::UidValidity(n) => status.uidvalidity = Some(*n),
                        S::HighestModSeq(m) => status.highest_modseq = Some(*m),
                        _ => {}
                    }
                }
                seen = true;
                true
            }
            _ => false,
        })
        .await?;
        if !seen {
            return Err(ImapError::Protocol(
                "STATUS without a STATUS response".into(),
            ));
        }
        Ok(status)
    }

    async fn idle(&mut self, cancel: CancellationToken, timeout: Duration) -> Result<IdleOutcome> {
        self.require_selected()?;
        if !self.state.caps.idle() {
            return Err(ImapError::Unsupported("IDLE"));
        }
        if cancel.is_cancelled() {
            return Ok(IdleOutcome::Cancelled);
        }
        if self.state.has_change_events() {
            return Ok(IdleOutcome::NewData);
        }
        let id = self.wire.send("IDLE", "IDLE").await?;
        self.await_continuation("IDLE", &id).await?;
        let deadline = tokio::time::Instant::now() + timeout;
        let outcome = loop {
            let state = &mut self.state;
            let read = self.wire.read_with("IDLE", None, |resp| match resp {
                Response::Done {
                    tag,
                    status,
                    outcome,
                } if tag == &id => Step::Done(tagged_result("IDLE", status, outcome)),
                Response::Continue(_) => Step::Other(false),
                other => {
                    let changed = State::is_change(other);
                    state.record(other);
                    Step::Other(changed)
                }
            });
            tokio::select! {
                biased;
                () = cancel.cancelled() => break IdleOutcome::Cancelled,
                () = tokio::time::sleep_until(deadline) => break IdleOutcome::Timeout,
                step = read => match step? {
                    Step::Other(true) => break IdleOutcome::NewData,
                    Step::Other(false) | Step::Continue => {}
                    // The server ended IDLE on its own; report it as new data so the
                    // caller re-checks the folder.
                    Step::Done(result) => {
                        result?;
                        return Ok(IdleOutcome::NewData);
                    }
                },
            }
        };
        self.wire.send_untagged("DONE", "IDLE").await?;
        self.finish("IDLE", &id, &mut |_| false).await?;
        tracing::trace!(?outcome, "imap IDLE ended");
        Ok(outcome)
    }

    async fn noop(&mut self) -> Result<()> {
        self.run("NOOP", "NOOP", |_| false).await?;
        Ok(())
    }

    fn take_events(&mut self) -> Vec<SessionEvent> {
        self.state.overflowed = false;
        std::mem::take(&mut self.state.events)
    }

    async fn logout(&mut self) -> Result<()> {
        if self.wire.is_broken() {
            return Ok(());
        }
        let result = self.run("LOGOUT", "LOGOUT", |_| false).await;
        self.wire.mark_broken();
        self.state.selected = None;
        match result {
            Ok(_) | Err(ImapError::ConnectionLost(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Handle one untagged response of a `SELECT`.
fn select_collect(info: &mut SelectInfo, saw_uidvalidity: &mut bool, resp: &Response<'_>) -> bool {
    match resp {
        Response::MailboxData(MailboxDatum::Exists(n)) => info.exists = *n,
        Response::MailboxData(MailboxDatum::Recent(n)) => info.recent = *n,
        Response::MailboxData(MailboxDatum::Flags(flags)) => {
            info.flags = flags.iter().map(|f| f.to_string()).collect();
        }
        Response::Data {
            status: Status::Ok,
            outcome,
        } => match split_outcome(outcome).0 {
            Some(Code::UidValidity(v)) => {
                info.uidvalidity = v;
                *saw_uidvalidity = true;
            }
            Some(Code::UidNext(n)) => info.uidnext = Some(n),
            Some(Code::HighestModSeq(m)) => info.highest_modseq = Some(m),
            Some(Code::NoModSeq) => info.highest_modseq = None,
            Some(Code::Unseen(n)) => info.first_unseen = Some(n),
            Some(Code::PermanentFlags(flags)) => {
                info.allows_new_keywords = flags.iter().any(|f| f == "\\*");
                info.permanent_flags = flags.into_iter().filter(|f| f != "\\*").collect();
            }
            // RFC 7162 §3.2.11: everything before [CLOSED] was about the previously
            // selected folder.
            Some(Code::Closed) => {
                *info = SelectInfo::default();
                *saw_uidvalidity = false;
            }
            Some(Code::Alert) => return false,
            _ => {}
        },
        Response::Vanished {
            earlier: true,
            uids,
        } => {
            for r in uids {
                info.vanished.insert_range(*r.start(), *r.end());
            }
        }
        Response::Fetch(_, attrs) => match FetchAttrs::collect(attrs).flag_change() {
            Some(change) => info.changed.push(change),
            None => return false,
        },
        _ => return false,
    }
    true
}
