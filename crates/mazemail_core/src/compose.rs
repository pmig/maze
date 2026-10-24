//! Writing mail (PLAN §4.10): identities, local drafts, reply/forward prefill and
//! sending.
//!
//! # Drafts
//!
//! [`CoreHandle::create_draft`], [`CoreHandle::update_draft`] and
//! [`CoreHandle::delete_draft`] keep the composer's autosaved [`Draft`]s in the
//! encrypted database. Uploading drafts to the server's Drafts folder comes later.
//!
//! # Replies and forwards
//!
//! [`CoreHandle::reply_draft`] and [`CoreHandle::forward_draft`] prefill a draft from a
//! stored message:
//!
//! - **Identity** (CONCEPT §8.6): the identity whose address the original was sent to,
//!   exactly (To, Cc, `Delivered-To`, `X-Original-To`, `Envelope-To`; the message's own
//!   account first), else with `+tag` sub-addressing ignored, else the account's default
//!   identity.
//! - **Recipients**: a reply goes to `Reply-To` (or `From`); reply all adds the
//!   original To and Cc. Your own addresses are left out, and replying to a message you
//!   sent goes to its recipients.
//! - **Subject** `Re: `/`Fwd: ` (not doubled, any language), **`In-Reply-To`** and
//!   **`References`** (the original's chain plus its Message-ID).
//! - **Quote**: an attribution line and the original as Markdown `>` blocks; HTML
//!   originals are converted to Markdown (`htmd`), plain text is quoted as is. A
//!   forward includes the original unquoted below a header block. (The original's
//!   attachments aren't forwarded yet.)
//!
//! # Sending
//!
//! [`CoreHandle::send_draft`] checks the draft, reads attached files, builds the MIME
//! message (`multipart/alternative`, Message-ID at the identity's domain), stores it as
//! an encrypted blob and queues it in the outbox with its SMTP envelope, then makes the
//! first attempt right away (see [`crate::outgoing`]). A permanent refusal (for example
//! [`smtp::SmtpError::SendAsDenied`]) comes back as [`Error::Send`] and the message is
//! taken out of the outbox again, so the composer can stay open; a temporary failure
//! (offline) leaves it queued for the outbox runner.

use std::collections::HashSet;
use std::sync::Arc;

use mazemail_mime::compose::{
    OutgoingAttachment, OutgoingMessage, build_message, content_type_for, forward_subject,
    html_to_markdown, new_message_id, quote, reply_references, reply_subject,
};
use mazemail_mime::{MessageHeaders, ParsedMessage};
use mazemail_types::compose::{Draft, DraftOrigin};
use mazemail_types::{AccountId, Address, DraftId, IdentityId, MessageId, PublicId as _};
use store::{Identity, MessageDetail, OutboxEnvelope, Queries as _, Reader};
use time::{OffsetDateTime, UtcOffset};

use crate::correspondents::normalize_email;
use crate::handle::CoreHandle;
use crate::outgoing::{CoreRoutes, SentCopy, sent_folder_of};
use crate::sync_bridge::{FetchPriority, RefreshScope};
use crate::{Error, Result};

/// The largest message (attachments included, before encoding) Maze sends: most
/// providers refuse more than 25 MB.
pub const MAX_MESSAGE_BYTES: u64 = 25 * 1024 * 1024;

/// Why a draft can't be sent or saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ComposeErrorKind {
    /// No recipients.
    NoRecipients,
    /// No identity to send from (no account yet, or it was removed).
    NoIdentity,
    /// The identity's account can't send (no server settings, no sync engine).
    CantSend,
    /// An attached file can't be read, or the message is too large.
    Attachment,
    /// The message couldn't be built.
    Build,
}

pub(crate) fn compose_error(kind: ComposeErrorKind, message: impl Into<String>) -> Error {
    Error::Compose {
        kind,
        message: message.into(),
    }
}

/// An identity as the From picker shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct IdentityInfo {
    /// The identity.
    pub id: IdentityId,
    /// Its account.
    pub account: AccountId,
    /// The account's name.
    pub account_name: String,
    /// The display name, if set.
    pub display_name: Option<String>,
    /// The address.
    pub email: String,
    /// The account's default identity.
    pub is_default: bool,
}

impl std::fmt::Debug for IdentityInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdentityInfo")
            .field("id", &self.id)
            .field("account", &self.account)
            .field("is_default", &self.is_default)
            .finish_non_exhaustive()
    }
}

/// What [`CoreHandle::send_draft`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendResult {
    /// The server accepted the message.
    Sent,
    /// The message waits in the outbox (offline, server busy) and goes out later.
    Queued {
        /// Why it couldn't go out right away (the server's or network's words).
        reason: String,
    },
}

/// Which reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReplyKind {
    /// To the sender (`Reply-To` or `From`).
    Sender,
    /// To the sender and every other recipient.
    All,
}

/// The original message as replies and forwards need it.
struct Original {
    detail: MessageDetail,
    headers: Option<MessageHeaders>,
    body: Option<String>,
}

impl Original {
    fn from(&self) -> Option<Address> {
        self.headers
            .as_ref()
            .and_then(|h| h.from.first().cloned())
            .or_else(|| self.detail.from.clone())
    }

    fn reply_to(&self) -> Vec<Address> {
        self.headers
            .as_ref()
            .map(|h| h.reply_to.clone())
            .unwrap_or_default()
    }

    fn to(&self) -> Vec<Address> {
        self.headers
            .as_ref()
            .map_or_else(|| self.detail.to.clone(), |h| h.to.clone())
    }

    fn cc(&self) -> Vec<Address> {
        self.headers
            .as_ref()
            .map_or_else(|| self.detail.cc.clone(), |h| h.cc.clone())
    }

    fn subject(&self) -> String {
        self.headers
            .as_ref()
            .and_then(|h| h.subject.clone())
            .or_else(|| self.detail.subject.clone())
            .unwrap_or_default()
    }

    fn message_id(&self) -> Option<String> {
        self.headers
            .as_ref()
            .and_then(|h| h.message_id.clone())
            .or_else(|| self.detail.rfc_message_id.clone())
    }

    fn references(&self) -> Vec<String> {
        self.headers
            .as_ref()
            .map_or_else(|| self.detail.references.clone(), |h| h.references.clone())
    }

    fn in_reply_to(&self) -> Option<String> {
        self.headers
            .as_ref()
            .and_then(|h| h.in_reply_to.first().cloned())
            .or_else(|| self.detail.in_reply_to.clone())
    }

    /// Every address the message was delivered to, in the order Thunderbird checks
    /// them for the identity (delivery headers first, then To and Cc).
    fn delivered_to(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(h) = &self.headers {
            out.extend(h.delivered_to.iter().cloned());
            out.extend(h.x_original_to.iter().cloned());
            out.extend(h.envelope_to.iter().cloned());
        } else if let Some(d) = &self.detail.delivered_to {
            out.push(d.clone());
        }
        out.extend(self.to().into_iter().map(|a| a.email));
        out.extend(self.cc().into_iter().map(|a| a.email));
        out
    }
}

/// The text to quote: the HTML body as Markdown, else the plain text.
fn quotable_text(parsed: &ParsedMessage) -> Option<String> {
    if let Some(html) = parsed.html_body.as_deref() {
        let markdown = html_to_markdown(html);
        if !markdown.trim().is_empty() {
            return Some(markdown);
        }
    }
    parsed.text_body.as_ref().map(|t| t.text.clone())
}

/// Picks the identity for a reply or forward (see the [module docs](self)).
pub(crate) fn select_identity(
    identities: &[Identity],
    account: AccountId,
    delivered_to: &[String],
) -> Option<IdentityId> {
    let own_first =
        |a: &&Identity, b: &&Identity| (a.account != account).cmp(&(b.account != account));
    let mut ordered: Vec<&Identity> = identities.iter().collect();
    ordered.sort_by(own_first);
    for address in delivered_to {
        if let Some(identity) = ordered
            .iter()
            .find(|i| i.input.email.trim().eq_ignore_ascii_case(address.trim()))
        {
            return Some(identity.id);
        }
    }
    for address in delivered_to {
        let Some(normalized) = normalize_email(address) else {
            continue;
        };
        if let Some(identity) = ordered
            .iter()
            .find(|i| normalize_email(&i.input.email).as_deref() == Some(normalized.as_str()))
        {
            return Some(identity.id);
        }
    }
    default_identity(identities, Some(account))
}

/// The account's default identity (or its first), else any account's.
pub(crate) fn default_identity(
    identities: &[Identity],
    account: Option<AccountId>,
) -> Option<IdentityId> {
    let of_account = |a: Option<AccountId>| {
        identities
            .iter()
            .filter(move |i| a.is_none_or(|a| i.account == a))
    };
    of_account(account)
        .find(|i| i.input.is_default)
        .or_else(|| of_account(account).next())
        .or_else(|| identities.iter().find(|i| i.input.is_default))
        .or_else(|| identities.first())
        .map(|i| i.id)
}

/// Removes `own` addresses and duplicates (case-insensitive).
fn without(
    addresses: Vec<Address>,
    own: &HashSet<String>,
    seen: &mut HashSet<String>,
) -> Vec<Address> {
    addresses
        .into_iter()
        .filter(|a| {
            let key = a.email.trim().to_ascii_lowercase();
            !key.is_empty()
                && !own.contains(&key)
                && normalize_email(&key).is_none_or(|n| !own.contains(&n))
                && seen.insert(key)
        })
        .collect()
}

/// The recipients of a reply: `(to, cc)`.
fn reply_recipients(
    original: &Original,
    kind: ReplyKind,
    own: &HashSet<String>,
) -> (Vec<Address>, Vec<Address>) {
    let from = original.from();
    let from_me = from.as_ref().is_some_and(|f| {
        let key = f.email.trim().to_ascii_lowercase();
        own.contains(&key) || normalize_email(&key).is_some_and(|n| own.contains(&n))
    });
    let mut seen = HashSet::new();
    if from_me {
        // A reply to a message you sent goes to its recipients again.
        let to = without(original.to(), own, &mut seen);
        let cc = match kind {
            ReplyKind::Sender => Vec::new(),
            ReplyKind::All => without(original.cc(), own, &mut seen),
        };
        return (to, cc);
    }
    let reply_to = original.reply_to();
    let primary = if reply_to.is_empty() {
        from.into_iter().collect()
    } else {
        reply_to
    };
    // The sender stays even if it is one of your addresses' aliases you don't own.
    let mut to: Vec<Address> = Vec::new();
    for address in primary {
        let key = address.email.trim().to_ascii_lowercase();
        if seen.insert(key) {
            to.push(address);
        }
    }
    match kind {
        ReplyKind::Sender => (to, Vec::new()),
        ReplyKind::All => {
            to.extend(without(original.to(), own, &mut seen));
            let cc = without(original.cc(), own, &mut seen);
            (to, cc)
        }
    }
}

/// `Thu, 22 Oct 2026 at 10:12`.
fn format_date(unix: i64, offset: UtcOffset) -> String {
    let Ok(date) = OffsetDateTime::from_unix_timestamp(unix) else {
        return String::new();
    };
    let format = time::macros::format_description!(
        "[weekday repr:short], [day padding:none] [month repr:short] [year] at [hour]:[minute]"
    );
    date.to_offset(offset).format(&format).unwrap_or_default()
}

fn signature_block(reader: &Reader, identity: Option<IdentityId>) -> Result<Option<String>> {
    let Some(identity) = identity
        .map(|id| reader.identity(id))
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    let Some(signature) = identity.input.signature else {
        return Ok(None);
    };
    Ok(reader
        .signature(signature)?
        .and_then(|s| s.body_text)
        .map(|text| text.trim_end().to_owned())
        .filter(|text| !text.is_empty())
        .map(|text| format!("-- \n{text}")))
}

impl CoreHandle {
    /// The identities to send from: by account, each account's default first.
    pub fn identities(&self) -> Result<Vec<IdentityInfo>> {
        let reader = self.store().reader()?;
        let accounts = reader.accounts()?;
        let mut identities = reader.identities(None)?;
        let ordinal = |account: AccountId| {
            accounts
                .iter()
                .position(|a| a.id == account)
                .unwrap_or(usize::MAX)
        };
        identities.sort_by_key(|i| {
            (
                ordinal(i.account),
                !i.input.is_default,
                i.input.ordinal,
                i.id,
            )
        });
        Ok(identities
            .into_iter()
            .map(|i| IdentityInfo {
                id: i.id,
                account: i.account,
                account_name: accounts
                    .iter()
                    .find(|a| a.id == i.account)
                    .map(|a| a.name.clone())
                    .unwrap_or_default(),
                display_name: i
                    .input
                    .display_name
                    .clone()
                    .filter(|n| !n.trim().is_empty()),
                email: i.input.email,
                is_default: i.input.is_default,
            })
            .collect())
    }

    /// A blank draft from the first account's default identity (with its signature).
    pub fn new_draft(&self) -> Result<Draft> {
        let reader = self.store().reader()?;
        let identities = self.identities()?;
        let identity = identities.first().map(|i| i.id);
        let body = signature_block(&reader, identity)?
            .map(|sig| format!("\n\n{sig}\n"))
            .unwrap_or_default();
        Ok(Draft {
            identity,
            body_markdown: body,
            ..Draft::default()
        })
    }

    fn original(&self, message: MessageId) -> Result<Original> {
        let reader = self.store().reader()?;
        let detail = reader
            .message(message)?
            .ok_or_else(|| Error::not_found("message", message.to_public()))?;
        let parsed = match detail.blob {
            Some(blob) => match self.store().blobs().read(&blob) {
                Ok(raw) => mazemail_mime::parse(&raw).ok(),
                Err(store::Error::BlobMissing(_)) => None,
                Err(err) => return Err(err.into()),
            },
            None => None,
        };
        if parsed.is_none() {
            // Quote what we have; the body follows for next time.
            self.sync_bridge()
                .fetch_body(message, FetchPriority::Interactive);
        }
        let (headers, body) = match parsed {
            Some(parsed) => {
                let body = quotable_text(&parsed);
                (Some(parsed.headers), body)
            }
            None => (None, None),
        };
        Ok(Original {
            detail,
            headers,
            body,
        })
    }

    /// A reply to `message`, prefilled (see the [module docs](self)).
    pub async fn reply_draft(&self, message: MessageId, kind: ReplyKind) -> Result<Draft> {
        let core = self.clone();
        self.run_blocking(move || core.reply_draft_blocking(message, kind))
            .await
    }

    fn reply_draft_blocking(&self, message: MessageId, kind: ReplyKind) -> Result<Draft> {
        let original = self.original(message)?;
        let reader = self.store().reader()?;
        let identities = reader.identities(None)?;
        let identity = select_identity(
            &identities,
            original.detail.account,
            &original.delivered_to(),
        );
        let own: HashSet<String> = identities
            .iter()
            .map(|i| i.input.email.trim().to_ascii_lowercase())
            .collect();
        let (to, cc) = reply_recipients(&original, kind, &own);
        let mut body = String::from("\n\n");
        if let Some(sig) = signature_block(&reader, identity)? {
            body.push_str(&sig);
            body.push_str("\n\n");
        }
        if let Some(text) = &original.body {
            let who = original
                .from()
                .map_or_else(|| "someone".to_owned(), |a| a.to_string());
            let when = format_date(original.detail.date, self.utc_offset());
            body.push_str(&format!("On {when}, {who} wrote:\n"));
            body.push_str(&quote(text));
        }
        let message_id = original.message_id();
        Ok(Draft {
            identity,
            to,
            cc,
            bcc: Vec::new(),
            subject: reply_subject(&original.subject()),
            body_markdown: body,
            in_reply_to: message_id.clone(),
            references: reply_references(
                &original.references(),
                original.in_reply_to().as_deref(),
                message_id.as_deref(),
            ),
            attachments: Vec::new(),
            origin: Some(match kind {
                ReplyKind::Sender => DraftOrigin::Reply(message),
                ReplyKind::All => DraftOrigin::ReplyAll(message),
            }),
            name: String::new(),
        })
    }

    /// A forward of `message`, inline (see the [module docs](self)).
    pub async fn forward_draft(&self, message: MessageId) -> Result<Draft> {
        let core = self.clone();
        self.run_blocking(move || core.forward_draft_blocking(message))
            .await
    }

    fn forward_draft_blocking(&self, message: MessageId) -> Result<Draft> {
        let original = self.original(message)?;
        let reader = self.store().reader()?;
        let identities = reader.identities(None)?;
        let identity = select_identity(
            &identities,
            original.detail.account,
            &original.delivered_to(),
        );
        let mut body = String::from("\n\n");
        if let Some(sig) = signature_block(&reader, identity)? {
            body.push_str(&sig);
            body.push_str("\n\n");
        }
        body.push_str("-------- Forwarded Message --------\n");
        body.push_str(&format!("Subject: {}\n", original.subject()));
        body.push_str(&format!(
            "Date: {}\n",
            format_date(original.detail.date, self.utc_offset())
        ));
        if let Some(from) = original.from() {
            body.push_str(&format!("From: {from}\n"));
        }
        let to = original.to();
        if !to.is_empty() {
            body.push_str(&format!("To: {}\n", Address::format_list(&to)));
        }
        let cc = original.cc();
        if !cc.is_empty() {
            body.push_str(&format!("Cc: {}\n", Address::format_list(&cc)));
        }
        body.push('\n');
        if let Some(text) = &original.body {
            body.push_str(text.trim_end());
            body.push('\n');
        }
        Ok(Draft {
            identity,
            subject: forward_subject(&original.subject()),
            body_markdown: body,
            references: original.message_id().into_iter().collect(),
            origin: Some(DraftOrigin::Forward(message)),
            ..Draft::default()
        })
    }

    /// Saves a new local draft.
    pub async fn create_draft(&self, draft: Draft) -> Result<DraftId> {
        self.ensure_open()?;
        Ok(self.store().write(move |w| w.create_draft(&draft)).await?)
    }

    /// Replaces a local draft.
    pub async fn update_draft(&self, id: DraftId, draft: Draft) -> Result<()> {
        self.ensure_open()?;
        self.store()
            .write(move |w| w.update_draft(id, &draft))
            .await
            .map_err(crate::handle::map_store)
    }

    /// Deletes a local draft.
    pub async fn delete_draft(&self, id: DraftId) -> Result<()> {
        self.ensure_open()?;
        self.store()
            .write(move |w| w.delete_draft(id))
            .await
            .map_err(crate::handle::map_store)
    }

    /// One local draft.
    pub fn draft(&self, id: DraftId) -> Result<Draft> {
        self.ensure_open()?;
        self.store()
            .reader()?
            .draft(id)?
            .map(|d| d.draft)
            .ok_or_else(|| Error::not_found("draft", id.to_public()))
    }

    /// Every local draft, most recently edited first.
    pub fn drafts(&self) -> Result<Vec<(DraftId, Draft)>> {
        self.ensure_open()?;
        Ok(self
            .store()
            .reader()?
            .drafts()?
            .into_iter()
            .map(|d| (d.id, d.draft))
            .collect())
    }

    /// Address completion from the address book and collected addresses.
    pub fn complete_addresses(&self, prefix: &str, limit: usize) -> Result<Vec<Address>> {
        self.ensure_open()?;
        let prefix = prefix.trim();
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self
            .store()
            .reader()?
            .search_contacts(prefix, limit)?
            .into_iter()
            .map(|c| Address::new(c.display_name.filter(|n| !n.trim().is_empty()), c.email))
            .collect())
    }

    /// Builds the message of `draft` and queues it: `(outbox item, identity)`.
    fn queue_draft(&self, draft: &Draft) -> Result<(store::OutboxId, IdentityId)> {
        if draft.recipients().next().is_none() {
            return Err(compose_error(
                ComposeErrorKind::NoRecipients,
                "add at least one recipient",
            ));
        }
        let reader = self.store().reader()?;
        let identities = reader.identities(None)?;
        let identity_id = draft
            .identity
            .filter(|id| identities.iter().any(|i| i.id == *id))
            .or_else(|| default_identity(&identities, None))
            .ok_or_else(|| {
                compose_error(
                    ComposeErrorKind::NoIdentity,
                    "add an account before sending mail",
                )
            })?;
        let sender = crate::outgoing::sender(self.store(), identity_id)
            .map_err(|err| compose_error(ComposeErrorKind::CantSend, err.to_string()))?;
        let identity = &sender.identity.input;

        let mut attachments = Vec::with_capacity(draft.attachments.len());
        let mut total: u64 = draft.body_markdown.len() as u64;
        for attachment in &draft.attachments {
            let data = std::fs::read(&attachment.path).map_err(|err| {
                compose_error(
                    ComposeErrorKind::Attachment,
                    format!("an attached file can't be read ({})", err.kind()),
                )
            })?;
            total = total.saturating_add(data.len() as u64);
            attachments.push(OutgoingAttachment {
                content_type: content_type_for(&attachment.name).to_owned(),
                name: attachment.name.clone(),
                data,
            });
        }
        if total > MAX_MESSAGE_BYTES {
            return Err(compose_error(
                ComposeErrorKind::Attachment,
                format!(
                    "the message is larger than {} MB; most servers refuse that",
                    MAX_MESSAGE_BYTES / (1024 * 1024)
                ),
            ));
        }

        let from = Address::new(
            identity
                .display_name
                .clone()
                .filter(|n| !n.trim().is_empty()),
            identity.email.clone(),
        );
        let message_id = new_message_id(&store::Uuid::new_v4().into_bytes(), &from.domain());
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let message = OutgoingMessage {
            reply_to: identity
                .reply_to
                .as_deref()
                .and_then(|r| Address::parse(r).ok()),
            from: Some(from.clone()),
            to: draft.to.clone(),
            cc: draft.cc.clone(),
            subject: draft.subject.trim().to_owned(),
            body_markdown: draft.body_markdown.clone(),
            in_reply_to: draft.in_reply_to.clone(),
            references: draft.references.clone(),
            message_id: message_id.clone(),
            date: now,
            attachments,
            user_agent: Some(format!("Maze/{}", self.app_version())),
        };
        let raw = build_message(&message)
            .map_err(|err| compose_error(ComposeErrorKind::Build, err.to_string()))?;
        let envelope = OutboxEnvelope {
            from: from.email,
            to: draft
                .recipients()
                .map(|a| a.email.trim().to_owned())
                .collect(),
        };
        let blob = self.store().blobs().stage(&raw)?;
        let id = self.store().write_blocking(move |w| {
            w.outbox_enqueue(Some(identity_id), &blob, &message_id, now, Some(&envelope))
        })?;
        Ok((id, identity_id))
    }

    /// Sends `draft` (see the [module docs](self)). A saved draft (`id`) is deleted
    /// once the message is sent or queued; it stays when sending is refused.
    ///
    /// # Errors
    ///
    /// [`Error::Compose`] when the draft can't be sent as it is (no recipients, no
    /// identity, unreadable attachment); [`Error::Send`] when the server refused it
    /// for good (login refused, `SendAsDenied`, recipient rejected);
    /// [`Error::NoSyncEngine`] without a sync engine.
    pub async fn send_draft(&self, id: Option<DraftId>, draft: Draft) -> Result<SendResult> {
        let core = self.clone();
        self.run(async move { core.send_draft_inner(id, draft).await })
            .await
    }

    async fn send_draft_inner(&self, id: Option<DraftId>, draft: Draft) -> Result<SendResult> {
        let engine = self.engine().ok_or(Error::NoSyncEngine)?;
        let outbox = self.outbox().cloned().ok_or(Error::NoSyncEngine)?;
        let core = self.clone();
        let (item, identity) = self.run_blocking(move || core.queue_draft(&draft)).await?;
        let routes = CoreRoutes {
            store: self.store().clone(),
            engine,
        };
        let delivery = outbox
            .send(item, &routes, Arc::new(SentCopy))
            .await
            .map_err(|err| match err {
                smtp::OutboxError::Store(err) => Error::Store(err),
                _ => Error::Task("sending"),
            })?;
        let result = match delivery {
            smtp::Delivery::Sent | smtp::Delivery::AlreadySent => {
                tracing::info!(outbox = %item, "message sent");
                self.after_sent(&[identity]);
                SendResult::Sent
            }
            smtp::Delivery::Retrying { error, .. } => {
                tracing::info!(outbox = %item, "message queued; sending failed for now");
                SendResult::Queued {
                    reason: error.to_string(),
                }
            }
            smtp::Delivery::Skipped => SendResult::Queued {
                reason: "it is being sent in the background".to_owned(),
            },
            smtp::Delivery::Failed(error) => {
                tracing::warn!(outbox = %item, permanent = true, "the server refused the message");
                if let Err(err) = self.store().write(move |w| w.outbox_remove(item)).await {
                    tracing::warn!(%err, "removing a refused message from the outbox");
                }
                return Err(Error::Send(error));
            }
        };
        if let Some(id) = id {
            match self.store().write(move |w| w.delete_draft(id)).await {
                Ok(()) | Err(store::Error::NotFound { .. }) => {}
                Err(err) => tracing::warn!(%err, "deleting the sent draft"),
            }
        }
        Ok(result)
    }

    /// After messages went out: upload pending Sent copies now and refresh the Sent
    /// folders (servers that file the copy themselves).
    pub(crate) fn after_sent(&self, identities: &[IdentityId]) {
        let mut accounts = Vec::new();
        for identity in identities {
            if let Some((account, sent)) = sent_folder_of(self.store(), *identity) {
                accounts.push(account);
                if let Some(folder) = sent {
                    self.sync_bridge().refresh(RefreshScope::Folder(folder));
                }
            }
        }
        accounts.sort();
        accounts.dedup();
        self.sync_bridge().ops_enqueued(&accounts);
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use store::IdentityInput;

    use super::*;

    fn identity(id: i64, account: i64, email: &str, is_default: bool) -> Identity {
        Identity {
            id: IdentityId(id),
            account: AccountId(account),
            input: IdentityInput {
                email: email.into(),
                is_default,
                ..IdentityInput::default()
            },
        }
    }

    #[test]
    fn identity_follows_the_address_the_mail_was_sent_to() {
        let identities = vec![
            identity(1, 1, "you@glasskube.example", true),
            identity(2, 1, "you@distr.example", false),
            identity(3, 2, "me@private.example", true),
        ];
        let pick = |account: i64, to: &[&str]| {
            select_identity(
                &identities,
                AccountId(account),
                &to.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            )
        };
        assert_eq!(pick(1, &["You@Distr.example"]), Some(IdentityId(2)));
        // Exact before sub-addressing; sub-addressing still finds it.
        assert_eq!(pick(1, &["you+news@distr.example"]), Some(IdentityId(2)));
        // Another account's identity matches too.
        assert_eq!(pick(1, &["me@private.example"]), Some(IdentityId(3)));
        // Nothing matches: the account's default.
        assert_eq!(pick(1, &["list@lists.example"]), Some(IdentityId(1)));
        assert_eq!(pick(2, &[]), Some(IdentityId(3)));
        assert_eq!(select_identity(&[], AccountId(1), &[]), None);
    }

    fn original(from: &str, reply_to: &[&str], to: &[&str], cc: &[&str]) -> Original {
        let parse = |list: &[&str]| -> Vec<Address> {
            list.iter().map(|a| Address::parse(a).unwrap()).collect()
        };
        Original {
            detail: MessageDetail {
                id: MessageId(1),
                account: AccountId(1),
                thread: mazemail_types::ThreadId(1),
                blob: None,
                body_state: store::BodyState::None,
                rfc_message_id: None,
                in_reply_to: None,
                references: Vec::new(),
                date: 0,
                received: None,
                from: None,
                to: Vec::new(),
                cc: Vec::new(),
                delivered_to: None,
                subject: None,
                snippet: None,
                size: None,
                has_attachments: false,
                list_id: None,
                bodystructure_json: None,
                gm_msgid: None,
                gm_thrid: None,
                auth_json: None,
                identity_hint: None,
                locations: Vec::new(),
                tags: Vec::new(),
                detached_at: None,
            },
            headers: Some(MessageHeaders {
                from: parse(&[from]),
                reply_to: parse(reply_to),
                to: parse(to),
                cc: parse(cc),
                ..MessageHeaders::default()
            }),
            body: None,
        }
    }

    fn emails(list: &[Address]) -> Vec<&str> {
        list.iter().map(|a| a.email.as_str()).collect()
    }

    #[test]
    fn reply_recipients_leave_out_your_own_addresses() {
        let own: HashSet<String> = ["you@distr.example".to_owned()].into();
        let mail = original(
            "Anna <anna@example.com>",
            &[],
            &["you@distr.example", "bob@example.com"],
            &["carol@example.com", "ANNA@example.com"],
        );
        let (to, cc) = reply_recipients(&mail, ReplyKind::Sender, &own);
        assert_eq!(
            (emails(&to), emails(&cc)),
            (vec!["anna@example.com"], vec![])
        );
        let (to, cc) = reply_recipients(&mail, ReplyKind::All, &own);
        assert_eq!(emails(&to), vec!["anna@example.com", "bob@example.com"]);
        assert_eq!(emails(&cc), vec!["carol@example.com"]);

        // Reply-To wins over From.
        let list = original(
            "Anna <anna@example.com>",
            &["team@lists.example"],
            &["you@distr.example"],
            &[],
        );
        let (to, _) = reply_recipients(&list, ReplyKind::Sender, &own);
        assert_eq!(emails(&to), vec!["team@lists.example"]);

        // Your own message: back to its recipients.
        let sent = original(
            "you@distr.example",
            &[],
            &["anna@example.com"],
            &["bob@example.com"],
        );
        let (to, cc) = reply_recipients(&sent, ReplyKind::All, &own);
        assert_eq!(
            (emails(&to), emails(&cc)),
            (vec!["anna@example.com"], vec!["bob@example.com"])
        );
    }

    #[test]
    fn dates_read_like_mail_clients() {
        assert_eq!(
            format_date(1_792_663_200, UtcOffset::UTC),
            "Thu, 22 Oct 2026 at 10:00"
        );
    }
}
