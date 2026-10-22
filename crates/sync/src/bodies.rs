//! Body downloads (PLAN §4.5 step 5): reuse first, then the network; large messages
//! opened by the user get their text parts only.

use bytes::{BufMut, Bytes, BytesMut};
use imap::{BodyPart, UidSet};
use mazemail_types::MessageId;
use store::{BodyState, Queries};

use crate::backoff::random_u64;
use crate::connect::Conn;
use crate::error::{Result, SyncError};
use crate::events::SyncEvent;
use crate::ingest::ReuseQuery;
use crate::shared::Shared;

/// Where a body can be fetched from.
struct Source {
    folder: mazemail_types::FolderId,
    remote: String,
    uid: u32,
    uidvalidity: Option<u32>,
}

/// Fetches and stores the body of `message`.
///
/// `interactive` (the user opened it): an existing text-parts body is good enough, and a
/// message above [`large_message_bytes`](crate::AccountSyncConfig::large_message_bytes)
/// gets only its text parts. Otherwise the full raw message is fetched.
pub(crate) async fn fetch_body(
    shared: &Shared,
    conn: &mut Conn,
    message: MessageId,
    interactive: bool,
) -> Result<BodyState> {
    let (detail, source) = shared
        .read(move |r| {
            let Some(detail) = r.message(message)? else {
                return Ok((None, None));
            };
            let mut source = None;
            for loc in &detail.locations {
                let Some(uid) = loc.uid else { continue };
                if let Some(folder) = r.folder(loc.folder)?
                    && !folder.is_label
                    && let Some(remote) = folder.remote_name.clone()
                {
                    source = Some(Source {
                        folder: folder.id,
                        remote,
                        uid,
                        uidvalidity: folder.sync.uidvalidity,
                    });
                    break;
                }
            }
            Ok((Some(detail), source))
        })
        .await?;
    let detail = detail.ok_or(SyncError::MessageNotFound(message))?;
    match detail.body_state {
        BodyState::Full => return Ok(BodyState::Full),
        BodyState::TextParts if interactive => return Ok(BodyState::TextParts),
        _ => {}
    }

    let query = ReuseQuery {
        message,
        rfc_message_id: detail.rfc_message_id.clone(),
        size: detail.size,
        header: None,
    };
    if let Some(raw) = shared.ingest.lookup_reused_body(&query).await? {
        shared
            .ingest
            .attach_body(message, raw, BodyState::Full)
            .await?;
        shared.events.emit(SyncEvent::BodyFetched(message));
        return Ok(BodyState::Full);
    }

    let source = source.ok_or(SyncError::NoServerCopy(message))?;
    let validity = conn.ensure_selected(&source.remote).await?;
    if source.uidvalidity.is_some_and(|v| v != validity) {
        // The folder was rebuilt on the server; its UIDs are stale until a pass ran.
        return Err(SyncError::FolderNotSynced(source.folder));
    }
    let large = detail
        .size
        .is_some_and(|s| s > shared.config.large_message_bytes);
    if interactive
        && large
        && let Some(raw) = fetch_text_parts(conn, source.uid).await?
    {
        shared
            .ingest
            .attach_body(message, raw, BodyState::TextParts)
            .await?;
        shared.events.emit(SyncEvent::BodyFetched(message));
        return Ok(BodyState::TextParts);
    }
    let fetched = conn.session.fetch_raw(source.uid).await;
    let raw = conn.track(fetched).map_err(|e| missing(e, message))?;
    shared
        .ingest
        .attach_body(message, raw, BodyState::Full)
        .await?;
    shared.events.emit(SyncEvent::BodyFetched(message));
    Ok(BodyState::Full)
}

fn missing(err: SyncError, message: MessageId) -> SyncError {
    match err {
        SyncError::Imap(imap::ImapError::MessageNotFound { .. }) => {
            SyncError::NoServerCopy(message)
        }
        other => other,
    }
}

/// The header and the displayable text parts (chosen from BODYSTRUCTURE) as one
/// synthetic MIME message, or `None` if the message has no text parts.
async fn fetch_text_parts(conn: &mut Conn, uid: u32) -> Result<Option<Bytes>> {
    let fetched = conn.session.fetch_headers(&UidSet::single(uid)).await;
    let Some(header) = conn.track(fetched)?.into_iter().find(|h| h.uid == uid) else {
        return Err(SyncError::Imap(imap::ImapError::MessageNotFound { uid }));
    };
    let Some(body) = header.body else {
        return Ok(None);
    };
    let wanted: Vec<BodyPart> = body.text_parts().cloned().collect();
    if wanted.is_empty() {
        return Ok(None);
    }
    let mut parts = Vec::with_capacity(wanted.len());
    for part in wanted {
        let fetched = conn.session.fetch_part(uid, &part.section).await;
        let content = conn.track(fetched)?;
        parts.push((part, content));
    }
    Ok(Some(text_parts_message(header.header.expose(), &parts)))
}

/// Header fields replaced by the synthetic multipart structure.
const MIME_FIELDS: &[&str] = &["content-type", "content-transfer-encoding", "mime-version"];

/// Builds a MIME message from the original header block (without its MIME fields)
/// and the given parts, still in their transfer encoding. A `text/plain` +
/// `text/html` pair becomes `multipart/alternative`, anything else
/// `multipart/mixed`. Marked with `X-Mazemail-Partial: text-parts`.
pub(crate) fn text_parts_message(header: &[u8], parts: &[(BodyPart, Bytes)]) -> Bytes {
    let boundary = loop {
        let candidate = format!("=_mazemail_text_{:016x}", random_u64());
        let clash = parts.iter().any(|(_, c)| {
            c.windows(candidate.len())
                .any(|w| w == candidate.as_bytes())
        });
        if !clash {
            break candidate;
        }
    };
    let alternative = parts.len() == 2
        && parts.iter().any(|(p, _)| p.subtype == "plain")
        && parts.iter().any(|(p, _)| p.subtype == "html");
    let subtype = if alternative { "alternative" } else { "mixed" };

    let mut out = BytesMut::with_capacity(
        header.len() + parts.iter().map(|(_, c)| c.len() + 200).sum::<usize>() + 200,
    );
    let mut skipping = false;
    for line in header.split_inclusive(|b| *b == b'\n') {
        let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
        let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
        if trimmed.is_empty() {
            break; // end of the header block
        }
        let continuation = matches!(trimmed.first(), Some(b' ' | b'\t'));
        if !continuation {
            let name = trimmed
                .split(|b| *b == b':')
                .next()
                .map(|n| String::from_utf8_lossy(n).trim().to_ascii_lowercase())
                .unwrap_or_default();
            skipping = MIME_FIELDS.contains(&name.as_str());
        }
        if !skipping {
            out.put_slice(trimmed);
            out.put_slice(b"\r\n");
        }
    }
    out.put_slice(
        format!(
            "MIME-Version: 1.0\r\nX-Mazemail-Partial: text-parts\r\n\
             Content-Type: multipart/{subtype}; boundary=\"{boundary}\"\r\n\r\n"
        )
        .as_bytes(),
    );
    for (part, content) in parts {
        let charset = part
            .charset
            .as_deref()
            .map(|c| format!("; charset=\"{}\"", c.replace(['"', '\\', '\r', '\n'], "")))
            .unwrap_or_default();
        out.put_slice(
            format!(
                "--{boundary}\r\nContent-Type: {}/{}{charset}\r\nContent-Transfer-Encoding: {}\r\n\r\n",
                part.mime_type, part.subtype, part.encoding
            )
            .as_bytes(),
        );
        out.put_slice(content);
        if !content.ends_with(b"\n") {
            out.put_slice(b"\r\n");
        }
    }
    out.put_slice(format!("--{boundary}--\r\n").as_bytes());
    out.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(section: &str, subtype: &str, encoding: &str) -> BodyPart {
        BodyPart {
            section: section.into(),
            mime_type: "text".into(),
            subtype: subtype.into(),
            charset: Some("utf-8".into()),
            encoding: encoding.into(),
            size: 10,
            disposition: None,
            filename: None,
            content_id: None,
            is_attachment: false,
        }
    }

    #[test]
    fn synthetic_message_parses_back() {
        let header = b"From: Anna <anna@example.com>\r\nSubject: Big\r\nContent-Type: multipart/mixed;\r\n boundary=\"orig\"\r\nMIME-Version: 1.0\r\nMessage-ID: <big@x>\r\n\r\n";
        let raw = text_parts_message(
            header,
            &[
                (
                    part("1.1", "plain", "7bit"),
                    Bytes::from_static(b"Hello there"),
                ),
                (
                    part("1.2", "html", "base64"),
                    Bytes::from_static(b"PHA+SGVsbG88L3A+"),
                ),
            ],
        );
        let text = String::from_utf8_lossy(&raw);
        assert!(!text.contains("orig"), "{text}");
        assert!(text.contains("multipart/alternative"));
        assert!(text.contains("X-Mazemail-Partial: text-parts"));
        let parsed = mazemail_mime::parse(&raw).unwrap();
        assert_eq!(parsed.headers.subject.as_deref(), Some("Big"));
        assert_eq!(parsed.headers.message_id.as_deref(), Some("big@x"));
        assert_eq!(parsed.text_body.unwrap().text.trim(), "Hello there");
        assert!(parsed.html_body.unwrap().contains("<p>Hello</p>"));
    }

    #[test]
    fn single_part_is_mixed() {
        let raw = text_parts_message(
            b"Subject: x\n\n",
            &[(part("1", "plain", "7bit"), Bytes::from_static(b"only\n"))],
        );
        let parsed = mazemail_mime::parse(&raw).unwrap();
        assert!(String::from_utf8_lossy(&raw).contains("multipart/mixed"));
        assert_eq!(parsed.text_body.unwrap().text.trim(), "only");
    }
}
