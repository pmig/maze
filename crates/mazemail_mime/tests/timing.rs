//! Coarse performance guard for the open-message path (CONCEPT §3: open a cached
//! message in under 30 ms).
//!
//! This is not a benchmark. It runs parse → sanitize + reader model → resource
//! rewrite on a large synthetic newsletter (about 250 KB of nested layout tables,
//! images, links and inline styles) and fails only far above the budget, so
//! accidental quadratic behavior shows up in CI without flaky thresholds. Release
//! builds need about 25 ms for this uncached worst case; opening a *cached* message
//! only deserializes the stored reader document.

#![allow(clippy::unwrap_used)] // Tests may unwrap (AGENTS.md).

use std::time::{Duration, Instant};

use mazemail_mime::html::{ResourceOrigin, SanitizeOptions, rewrite_resources};
use mazemail_mime::parse;

/// Limits per run, several times what the machine of the day measures (debug about
/// 90 ms, release about 25 ms for the full uncached pipeline on an M-series Mac), so
/// only algorithmic regressions trip them.
const DEBUG_LIMIT: Duration = Duration::from_millis(2_000);
const RELEASE_LIMIT: Duration = Duration::from_millis(150);

fn newsletter(sections: usize) -> Vec<u8> {
    let mut html = String::from(
        "<html><head><style>.x{background:url(https://cdn.example.org/bg.png)}</style></head><body>\
         <table width=\"100%\" cellpadding=\"0\"><tr><td align=\"center\"><table width=\"600\" role=\"presentation\">",
    );
    for i in 0..sections {
        html.push_str(&format!(
            "<tr><td style=\"padding:10px;background:url(https://cdn.example.org/s{i}.png)\">\
             <table width=\"100%\"><tr><td width=\"50%\"><a href=\"https://example.org/a/{i}\">\
             <img src=\"https://cdn.example.org/img/{i}.jpg\" width=\"280\" height=\"160\" alt=\"Item {i}\"></a></td>\
             <td width=\"50%\"><h2 style=\"font-family:Arial\">Headline number {i}</h2>\
             <p>Some <b>bold</b> and <i>italic</i> text about item {i}, with a \
             <a href=\"https://example.org/read/{i}\">link</a> and more words to make the paragraph \
             longer, as marketing copy tends to be.</p><ul><li>Point one</li><li>Point two</li></ul>\
             </td></tr></table></td></tr>"
        ));
    }
    html.push_str("</table></td></tr></table><img src=\"https://t.example.org/track/open?u=1\" width=\"1\" height=\"1\"></body></html>");
    let mut raw =
        b"From: News <news@example.org>\r\nTo: anna@example.com\r\nSubject: Big issue\r\n\
        Date: Thu, 6 Mar 2025 06:00:00 +0000\r\nMessage-ID: <big@example.org>\r\n\
        Content-Type: text/html; charset=utf-8\r\n\r\n"
            .to_vec();
    raw.extend_from_slice(html.as_bytes());
    raw
}

#[test]
fn opening_a_large_newsletter_stays_linear() {
    let raw = newsletter(400);
    assert!(raw.len() > 200_000, "fixture is {} bytes", raw.len());
    let origin = ResourceOrigin::new("http://127.0.0.1:4711/token").unwrap();

    let mut times = Vec::new();
    for _ in 0..3 {
        let start = Instant::now();
        let message = parse(&raw).unwrap();
        let prepared = message.prepare_html(&SanitizeOptions::default()).unwrap();
        let rewritten = rewrite_resources(&prepared.sanitized.html, &origin);
        times.push(start.elapsed());
        assert_eq!(prepared.sanitized.trackers_removed, 1);
        assert!(rewritten.remote_resources.len() > 800);
        assert!(!prepared.reader.is_empty());
    }
    times.sort();
    let median = times[1];
    let limit = if cfg!(debug_assertions) {
        DEBUG_LIMIT
    } else {
        RELEASE_LIMIT
    };
    eprintln!("open pipeline on {} bytes: median {median:?}", raw.len());
    assert!(median < limit, "median {median:?} exceeds {limit:?}");
}
