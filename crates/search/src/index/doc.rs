//! The indexer's input.

use std::fmt;

use mazemail_types::{AccountId, Address, MessageId, Sensitive, ThreadId};
use tantivy::TantivyDocument;

use super::schema::Fields;

/// A language with its own stemmed fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language {
    /// German: fills `subject_de` / `body_de`.
    De,
    /// English: fills `subject_en` / `body_en`.
    En,
}

/// An attachment as the index sees it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Attachment {
    /// The file name.
    pub name: Sensitive<String>,
    /// Text extracted from the attachment, if any (capped by the extractor).
    pub text: Option<Sensitive<String>>,
}

/// One message to index. The caller has already parsed the message, extracted the
/// text and detected the language (`whatlang`); the index only analyzes.
///
/// Only immutable data goes in (PLAN §4.4): folders, flags and tags are filtered in
/// SQLite, so changing them never touches the index.
#[derive(Clone, PartialEq, Eq)]
pub struct IndexDoc {
    /// The message, also the key for [`replace`](crate::Indexer::replace) and
    /// [`delete`](crate::Indexer::delete).
    pub message_id: MessageId,
    /// The thread it belongs to.
    pub thread_id: ThreadId,
    /// The account it belongs to.
    pub account_id: AccountId,
    /// The `Date:` header, Unix seconds.
    pub date: i64,
    /// The raw message size in bytes.
    pub size: u64,
    /// Senders.
    pub from: Vec<Address>,
    /// Recipients.
    pub to: Vec<Address>,
    /// Copy recipients.
    pub cc: Vec<Address>,
    /// The decoded subject.
    pub subject: Sensitive<String>,
    /// The body as plain text.
    pub body: Sensitive<String>,
    /// Attachments (names and extracted text). A non-empty list sets `has:attachment`.
    pub attachments: Vec<Attachment>,
    /// The `List-Id` (or list address) without angle brackets.
    pub list_id: Option<String>,
    /// The detected language; `None` fills only the neutral fields.
    pub language: Option<Language>,
}

impl fmt::Debug for IndexDoc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Content and addresses are sensitive; ids are not.
        f.debug_struct("IndexDoc")
            .field("message_id", &self.message_id)
            .field("thread_id", &self.thread_id)
            .field("account_id", &self.account_id)
            .field("date", &self.date)
            .field("size", &self.size)
            .field("language", &self.language)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Attachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attachment").finish_non_exhaustive()
    }
}

/// Ids are SQLite rowids: never negative, so the cast to `u64` is lossless both ways.
pub(crate) fn id_to_u64(raw: i64) -> u64 {
    raw as u64
}

pub(crate) fn id_from_u64(raw: u64) -> i64 {
    raw as i64
}

fn address_text(address: &Address) -> String {
    match &address.name {
        Some(name) if !name.is_empty() => format!("{name} {}", address.email),
        _ => address.email.clone(),
    }
}

impl IndexDoc {
    pub(crate) fn to_document(&self, fields: &Fields) -> TantivyDocument {
        let mut doc = TantivyDocument::new();
        doc.add_u64(fields.id, id_to_u64(self.message_id.0));
        doc.add_u64(fields.thread, id_to_u64(self.thread_id.0));
        doc.add_u64(fields.account, id_to_u64(self.account_id.0));
        doc.add_i64(fields.date, self.date);
        doc.add_u64(fields.size, self.size);
        doc.add_bool(fields.has_attachment, !self.attachments.is_empty());
        for (text_field, addr_field, addresses) in [
            (fields.from, fields.from_addr, &self.from),
            (fields.to, fields.to_addr, &self.to),
            (fields.cc, fields.cc_addr, &self.cc),
        ] {
            for address in addresses {
                doc.add_text(text_field, address_text(address));
                doc.add_text(addr_field, &address.email);
            }
        }
        let subject = self.subject.expose().as_str();
        let body = self.body.expose().as_str();
        doc.add_text(fields.subject, subject);
        doc.add_text(fields.body, body);
        match self.language {
            Some(Language::De) => {
                doc.add_text(fields.subject_de, subject);
                doc.add_text(fields.body_de, body);
            }
            Some(Language::En) => {
                doc.add_text(fields.subject_en, subject);
                doc.add_text(fields.body_en, body);
            }
            None => {}
        }
        for attachment in &self.attachments {
            doc.add_text(fields.attachment_names, attachment.name.expose());
            if let Some(text) = &attachment.text {
                doc.add_text(fields.attachment_text, text.expose());
            }
        }
        if let Some(list_id) = &self.list_id {
            doc.add_text(fields.list_id, list_id);
            doc.add_text(fields.list_id_raw, list_id);
        }
        doc
    }
}
