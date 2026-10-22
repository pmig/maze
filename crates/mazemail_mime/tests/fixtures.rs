//! Snapshot tests over the synthetic fixture mails in `tests/fixtures/`.
//!
//! Per fixture: the parsed summary (headers, attachments, snippet, text body,
//! language), the sanitized and rewritten HTML, and the reader document.

#![allow(clippy::unwrap_used)] // Tests may unwrap (AGENTS.md).

use std::path::Path;

use mazemail_mime::html::{
    ResourceOrigin, SanitizeOptions, SanitizedHtml, content_security_policy, rewrite_resources,
};
use mazemail_mime::{
    AttachmentMeta, Lang, MessageHeaders, ParsedMessage, ReaderDoc, TextBody, parse,
    parse_headers_only,
};
use serde::Serialize;

#[derive(Serialize)]
struct Summary<'a> {
    headers: &'a MessageHeaders,
    attachments: &'a [AttachmentMeta],
    has_attachments: bool,
    snippet: &'a str,
    size: usize,
    text_body: Option<&'a TextBody>,
    html_body_bytes: Option<usize>,
    language: Option<Lang>,
}

#[derive(Serialize)]
struct HtmlOutput {
    trackers_removed: usize,
    has_remote_resources: bool,
    cid_references: Vec<String>,
    remote_resources: Vec<String>,
    csp: String,
}

fn load(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn origin() -> ResourceOrigin {
    ResourceOrigin::new("http://127.0.0.1:4711/fixture-token").unwrap()
}

fn snapshot_fixture(name: &str) -> ParsedMessage {
    let raw = load(name);
    let message = parse(&raw).unwrap();
    let stem = name.trim_end_matches(".eml");

    let summary = Summary {
        headers: &message.headers,
        attachments: &message.attachments,
        has_attachments: message.has_attachments,
        snippet: &message.snippet,
        size: message.size,
        text_body: message.text_body.as_ref(),
        html_body_bytes: message.html_body.as_ref().map(String::len),
        language: message.language(),
    };
    insta::assert_yaml_snapshot!(format!("{stem}__parsed"), summary);

    // The headers-only path agrees with the full parse.
    assert_eq!(parse_headers_only(&raw).unwrap(), message.headers);

    if let Some(prepared) = message.prepare_html(&SanitizeOptions::default()) {
        let SanitizedHtml {
            html,
            trackers_removed,
            has_remote_resources,
            cid_references,
        } = prepared.sanitized;
        insta::assert_snapshot!(format!("{stem}__sanitized"), html);
        let rewritten = rewrite_resources(&html, &origin());
        insta::assert_snapshot!(format!("{stem}__rewritten"), rewritten.html);
        let output = HtmlOutput {
            trackers_removed,
            has_remote_resources,
            cid_references,
            remote_resources: rewritten.remote_resources,
            csp: content_security_policy(&origin()),
        };
        insta::assert_yaml_snapshot!(format!("{stem}__html_meta"), output);
        assert_eq!(prepared.reader, message.reader_doc());
    }

    let reader: ReaderDoc = message.reader_doc();
    insta::assert_yaml_snapshot!(format!("{stem}__reader"), reader);
    message
}

#[test]
fn plain_flowed() {
    let m = snapshot_fixture("plain_flowed.eml");
    assert!(m.text_body.as_ref().is_some_and(|b| b.format.flowed));
    assert_eq!(m.headers.references.len(), 2);
}

#[test]
fn multipart_alternative() {
    let m = snapshot_fixture("multipart_alternative.eml");
    assert!(m.text_body.is_some() && m.html_body.is_some());
    assert!(!m.has_attachments);
}

#[test]
fn newsletter() {
    let m = snapshot_fixture("newsletter.eml");
    let prepared = m.prepare_html(&SanitizeOptions::default()).unwrap();
    assert_eq!(prepared.sanitized.trackers_removed, 2);
    let html = prepared.sanitized.html.to_ascii_lowercase();
    for banned in [
        "<script",
        "<iframe",
        "<form",
        "<meta",
        "<link",
        "onclick",
        "onload",
        "javascript:",
    ] {
        assert!(!html.contains(banned), "{banned} survived");
    }
    assert_eq!(
        m.headers.list_id.as_deref(),
        Some("weekly.news.example.org")
    );
}

#[test]
fn outlook_reply() {
    let m = snapshot_fixture("outlook_reply.eml");
    assert_eq!(m.snippet, "Works for me, see you Thursday. Bob");
}

#[test]
fn german_encoded_words() {
    let m = snapshot_fixture("german_encoded_words.eml");
    assert_eq!(
        m.headers.subject.as_deref(),
        Some("AW: Grüße aus München – Abstimmung über das Budget")
    );
    assert_eq!(m.language(), Some(Lang::De));
    assert_eq!(
        m.attachments[0].filename.as_deref(),
        Some("Übersicht März.xlsx")
    );
}

#[test]
fn broken_charset() {
    let m = snapshot_fixture("broken_charset.eml");
    assert_eq!(m.headers.date, None);
    assert!(m.headers.to.is_empty());
    assert!(m.snippet.contains("Still readable"));
}

#[test]
fn attachment_inline_cid() {
    let m = snapshot_fixture("attachment_inline_cid.eml");
    assert!(m.has_attachments);
    let prepared = m.prepare_html(&SanitizeOptions::default()).unwrap();
    assert_eq!(
        prepared.sanitized.cid_references,
        vec!["logo@example.com".to_owned()]
    );
    let inline: Vec<_> = m
        .attachments
        .iter()
        .filter(|a| !a.counts_as_attachment())
        .collect();
    assert_eq!(inline.len(), 1);
    assert_eq!(inline[0].content_id.as_deref(), Some("logo@example.com"));
}
