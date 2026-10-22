//! Conversions from imap-proto's borrowed response types into this crate's types.

use std::borrow::Cow;

use async_imap::imap_proto::{
    AttributeValue, BodyContentCommon, BodyContentSinglePart, BodyStructure, ContentEncoding,
    MailboxListData, NameAttribute,
};
use bytes::Bytes;
use mazemail_types::{Flags, FolderRole, Sensitive};
use time::OffsetDateTime;
use time::format_description::FormatItem;
use time::macros::format_description;

use crate::gmail::GmailLabel;
use crate::types::{BodyPart, BodySummary, FlagChange, GmailMeta, HeaderFetch, RemoteFolder};
use crate::utf7;

/// Split IMAP flag names into [`Flags`] and remaining keywords. `\Recent` and `\*`
/// are dropped (session-only / meta flags).
pub(crate) fn split_flags<'a, I>(names: I) -> (Flags, Vec<String>)
where
    I: IntoIterator<Item = &'a str>,
{
    let mut flags = Flags::empty();
    let mut keywords = Vec::new();
    for name in names {
        if let Some(flag) = Flags::from_imap(name) {
            flags |= flag;
        } else if !name.eq_ignore_ascii_case("\\Recent") && name != "\\*" {
            keywords.push(name.to_owned());
        }
    }
    (flags, keywords)
}

/// Attributes of one `FETCH` response, collected.
#[derive(Default)]
pub(crate) struct FetchAttrs<'a> {
    pub(crate) uid: Option<u32>,
    pub(crate) flags: Option<&'a [Cow<'a, str>]>,
    pub(crate) modseq: Option<u64>,
    pub(crate) size: Option<u32>,
    pub(crate) internal_date: Option<&'a str>,
    pub(crate) header: Option<&'a [u8]>,
    pub(crate) body_section: Option<Option<&'a [u8]>>,
    pub(crate) body_structure: Option<&'a BodyStructure<'a>>,
    pub(crate) gm_msgid: Option<u64>,
    pub(crate) gm_thrid: Option<u64>,
    pub(crate) gm_labels: Option<&'a [Cow<'a, str>]>,
}

impl<'a> FetchAttrs<'a> {
    pub(crate) fn collect(attrs: &'a [AttributeValue<'a>]) -> Self {
        use async_imap::imap_proto::{MessageSection, SectionPath};
        let mut out = Self::default();
        for attr in attrs {
            match attr {
                AttributeValue::Uid(uid) => out.uid = Some(*uid),
                AttributeValue::Flags(flags) => out.flags = Some(flags),
                AttributeValue::ModSeq(m) => out.modseq = Some(*m),
                AttributeValue::Rfc822Size(s) => out.size = Some(*s),
                AttributeValue::InternalDate(d) => out.internal_date = Some(d),
                AttributeValue::BodyStructure(bs) => out.body_structure = Some(bs),
                AttributeValue::GmailMsgId(id) => out.gm_msgid = Some(*id),
                AttributeValue::GmailThrId(id) => out.gm_thrid = Some(*id),
                AttributeValue::GmailLabels(labels) => out.gm_labels = Some(labels),
                AttributeValue::Rfc822Header(data) => out.header = data.as_deref(),
                AttributeValue::BodySection {
                    section: Some(SectionPath::Full(MessageSection::Header)),
                    data,
                    ..
                } => out.header = data.as_deref(),
                AttributeValue::BodySection { data, .. } => {
                    out.body_section = Some(data.as_deref());
                }
                AttributeValue::Rfc822(data) => out.body_section = Some(data.as_deref()),
                _ => {}
            }
        }
        out
    }

    pub(crate) fn labels(&self) -> Option<Vec<GmailLabel>> {
        self.gm_labels
            .map(|labels| labels.iter().map(|l| GmailLabel::parse(l)).collect())
    }

    pub(crate) fn split_flags(&self) -> (Flags, Vec<String>) {
        split_flags(self.flags.unwrap_or_default().iter().map(|f| f.as_ref()))
    }

    /// A [`FlagChange`] if the response carries a UID and flags.
    pub(crate) fn flag_change(&self) -> Option<FlagChange> {
        let uid = self.uid?;
        self.flags?;
        let (flags, keywords) = self.split_flags();
        Some(FlagChange {
            uid,
            flags,
            keywords,
            modseq: self.modseq,
            gmail_labels: self.labels(),
        })
    }

    /// A [`HeaderFetch`] if the response carries a UID and the header block.
    pub(crate) fn header_fetch(&self) -> Option<HeaderFetch> {
        let uid = self.uid?;
        let header = self.header?;
        let (flags, keywords) = self.split_flags();
        let gmail = match (self.gm_msgid, self.gm_thrid) {
            (Some(msg_id), Some(thread_id)) => Some(GmailMeta {
                msg_id,
                thread_id,
                labels: self.labels().unwrap_or_default(),
            }),
            _ => None,
        };
        Some(HeaderFetch {
            uid,
            flags,
            keywords,
            size: self.size.unwrap_or(0),
            internal_date: self.internal_date.and_then(parse_internal_date),
            modseq: self.modseq,
            header: Sensitive::new(Bytes::copy_from_slice(header)),
            body: self.body_structure.map(summarize_body),
            gmail,
        })
    }
}

const INTERNAL_DATE: &[FormatItem<'static>] = format_description!(
    "[day]-[month repr:short case_sensitive:false]-[year] [hour]:[minute]:[second] [offset_hour sign:mandatory][offset_minute]"
);

/// Parse an `INTERNALDATE` such as `17-Jul-1996 02:44:25 -0700` (the day may be
/// space-padded: ` 7-Jul-1996 …`).
pub(crate) fn parse_internal_date(s: &str) -> Option<OffsetDateTime> {
    let s = s.trim();
    let normalized;
    let s = match s.find('-') {
        Some(1) => {
            normalized = format!("0{s}");
            normalized.as_str()
        }
        _ => s,
    };
    OffsetDateTime::parse(s, INTERNAL_DATE).ok()
}

fn name_attribute(attr: &NameAttribute<'_>) -> String {
    match attr {
        NameAttribute::NoInferiors => "\\Noinferiors".into(),
        NameAttribute::NoSelect => "\\Noselect".into(),
        NameAttribute::Marked => "\\Marked".into(),
        NameAttribute::Unmarked => "\\Unmarked".into(),
        NameAttribute::All => "\\All".into(),
        NameAttribute::Archive => "\\Archive".into(),
        NameAttribute::Drafts => "\\Drafts".into(),
        NameAttribute::Flagged => "\\Flagged".into(),
        NameAttribute::Junk => "\\Junk".into(),
        NameAttribute::Sent => "\\Sent".into(),
        NameAttribute::Trash => "\\Trash".into(),
        NameAttribute::Extension(s) => s.to_string(),
        _ => "\\Unknown".into(),
    }
}

/// Convert a `LIST` entry. `subscribed` is filled in later from `LSUB`.
pub(crate) fn remote_folder(data: &MailboxListData<'_>) -> RemoteFolder {
    let attributes: Vec<String> = data.name_attributes.iter().map(name_attribute).collect();
    let raw_name = data.name.to_string();
    let is_inbox = raw_name.eq_ignore_ascii_case("INBOX");
    let role = attributes
        .iter()
        .find_map(|a| FolderRole::from_special_use(a))
        .or(is_inbox.then_some(FolderRole::Inbox));
    let selectable = !attributes
        .iter()
        .any(|a| a.eq_ignore_ascii_case("\\Noselect") || a.eq_ignore_ascii_case("\\NonExistent"));
    RemoteFolder {
        name: if is_inbox {
            "INBOX".to_owned()
        } else {
            utf7::decode_lossy(&raw_name)
        },
        raw_name,
        delimiter: data.delimiter.as_deref().and_then(|d| d.chars().next()),
        attributes,
        role,
        selectable,
        subscribed: false,
    }
}

/// Flatten a BODYSTRUCTURE into its leaf parts with section numbers.
pub(crate) fn summarize_body(bs: &BodyStructure<'_>) -> BodySummary {
    let mut parts = Vec::new();
    walk(bs, "", &mut parts);
    BodySummary { parts }
}

fn walk(bs: &BodyStructure<'_>, section: &str, parts: &mut Vec<BodyPart>) {
    match bs {
        BodyStructure::Multipart { bodies, .. } => {
            for (i, child) in bodies.iter().enumerate() {
                let child_section = if section.is_empty() {
                    (i + 1).to_string()
                } else {
                    format!("{section}.{}", i + 1)
                };
                walk(child, &child_section, parts);
            }
        }
        BodyStructure::Basic { common, other, .. }
        | BodyStructure::Text { common, other, .. }
        | BodyStructure::Message { common, other, .. } => {
            // A non-multipart message body is section 1 (RFC 3501 §6.4.5).
            let section = if section.is_empty() { "1" } else { section };
            parts.push(leaf(section, common, other));
        }
    }
}

fn param<'a>(params: &'a Option<Vec<(Cow<'a, str>, Cow<'a, str>)>>, key: &str) -> Option<&'a str> {
    params
        .as_ref()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_ref())
}

fn leaf(
    section: &str,
    common: &BodyContentCommon<'_>,
    other: &BodyContentSinglePart<'_>,
) -> BodyPart {
    let mime_type = common.ty.ty.to_ascii_lowercase();
    let subtype = common.ty.subtype.to_ascii_lowercase();
    let disposition = common.disposition.as_ref();
    let disposition_type = disposition.map(|d| d.ty.to_ascii_lowercase());
    let filename = disposition
        .and_then(|d| param(&d.params, "filename"))
        .or_else(|| param(&common.ty.params, "name"))
        .map(str::to_owned);
    let content_id = other.id.as_deref().map(str::to_owned);
    let is_attachment = match disposition_type.as_deref() {
        Some("attachment") => true,
        Some("inline") => false,
        _ => {
            filename.is_some()
                || (mime_type == "message" && subtype == "rfc822")
                || (mime_type != "text" && content_id.is_none())
        }
    };
    BodyPart {
        section: section.to_owned(),
        charset: param(&common.ty.params, "charset").map(str::to_owned),
        encoding: encoding_name(&other.transfer_encoding),
        size: other.octets,
        disposition: disposition_type,
        filename: filename.map(Sensitive::new),
        content_id,
        is_attachment,
        mime_type,
        subtype,
    }
}

fn encoding_name(enc: &ContentEncoding<'_>) -> String {
    match enc {
        ContentEncoding::SevenBit => "7bit".into(),
        ContentEncoding::EightBit => "8bit".into(),
        ContentEncoding::Binary => "binary".into(),
        ContentEncoding::Base64 => "base64".into(),
        ContentEncoding::QuotedPrintable => "quoted-printable".into(),
        ContentEncoding::Other(s) => s.to_ascii_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_flags_and_keywords() {
        let (flags, keywords) =
            split_flags(["\\Seen", "\\Recent", "$Forwarded", "$label1", "Work", "\\*"]);
        assert_eq!(flags, Flags::SEEN | Flags::FORWARDED);
        assert_eq!(keywords, ["$label1", "Work"]);
    }

    #[test]
    fn parses_internal_dates() {
        let d = parse_internal_date("17-Jul-1996 02:44:25 -0700").unwrap();
        assert_eq!(d.unix_timestamp(), 837_596_665);
        let d = parse_internal_date(" 7-Jul-1996 02:44:25 +0000").unwrap();
        assert_eq!(d.day(), 7);
        let d = parse_internal_date("07-jul-1996 02:44:25 +0200").unwrap();
        assert_eq!(d.offset().whole_hours(), 2);
        assert!(parse_internal_date("yesterday").is_none());
    }
}
