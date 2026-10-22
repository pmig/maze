//! Plain text → [`ReaderDoc`]: quote levels, links, signatures, format=flowed.

use std::sync::LazyLock;

use linkify::{LinkFinder, LinkKind};
use serde::{Deserialize, Serialize};

use crate::html::urls::is_allowed_link;

use super::model::{Block, DocSource, Inline, ReaderDoc};

/// Quote levels beyond this are clamped, which also bounds recursion.
const MAX_QUOTE_DEPTH: usize = 32;

/// How a `text/plain` body is encoded (RFC 3676).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TextFormat {
    /// `format=flowed`: lines ending in a space continue on the next line.
    pub flowed: bool,
    /// `delsp=yes`: the space before a soft line break is not part of the text.
    pub delsp: bool,
}

/// One line after decoding: quote depth and content without quote markers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    depth: usize,
    text: String,
}

/// Convert a plain-text body into a reader document.
///
/// Lines starting with `>` become nested [`Block::Quote`]s, blank lines separate
/// paragraphs, hard line breaks stay as `\n`, URLs and email addresses become links,
/// everything after a `-- ` line becomes a [`Block::Signature`], and an Outlook
/// "Original Message" separator starts quoted history. Flowed text is unwrapped first.
pub fn from_plain_text(text: &str, format: TextFormat) -> ReaderDoc {
    let text = text.replace("\r\n", "\n");
    let lines = if format.flowed {
        decode_flowed(&text, format.delsp)
    } else {
        decode_fixed(&text)
    };
    ReaderDoc {
        source: DocSource::PlainText,
        blocks: build_blocks(&lines, 0, true),
    }
}

/// Count leading quote markers. Fixed text also accepts `> > x`.
fn split_quote(line: &str, allow_spaces: bool) -> (usize, &str) {
    let mut depth = 0;
    let mut rest = line;
    loop {
        if let Some(r) = rest.strip_prefix('>') {
            depth += 1;
            rest = r;
        } else if allow_spaces
            && depth > 0
            && rest.starts_with(' ')
            && rest.trim_start_matches(' ').starts_with('>')
        {
            rest = rest.trim_start_matches(' ');
        } else {
            break;
        }
    }
    (depth.min(MAX_QUOTE_DEPTH), rest)
}

fn decode_fixed(text: &str) -> Vec<Line> {
    text.split('\n')
        .map(|raw| {
            let (depth, rest) = split_quote(raw, true);
            let rest = if depth > 0 {
                rest.strip_prefix(' ').unwrap_or(rest)
            } else {
                rest
            };
            // Keep the signature separator's trailing space; trim everything else.
            let text = if rest == "-- " { rest } else { rest.trim_end() };
            Line {
                depth,
                text: text.to_owned(),
            }
        })
        .collect()
}

/// RFC 3676 decoding: join soft-broken lines of the same quote depth, undo space
/// stuffing, never join the signature separator.
fn decode_flowed(text: &str, delsp: bool) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let mut open = false; // the last line in `out` ended with a soft break
    for raw in text.split('\n') {
        let (depth, rest) = split_quote(raw, false);
        let content = rest.strip_prefix(' ').unwrap_or(rest);
        let is_signature = content == "-- ";
        let soft = !is_signature && content.ends_with(' ');
        let piece = if soft && delsp {
            &content[..content.len() - 1]
        } else {
            content
        };
        match out.last_mut() {
            Some(last) if open && last.depth == depth && !is_signature => last.text.push_str(piece),
            _ => out.push(Line {
                depth,
                text: piece.to_owned(),
            }),
        }
        open = soft;
    }
    for line in &mut out {
        if line.text != "-- " {
            line.text.truncate(line.text.trim_end().len());
        }
    }
    out
}

fn is_signature_separator(line: &Line) -> bool {
    line.depth == 0 && line.text == "-- "
}

/// Outlook and friends quote without `>`, after a separator line.
fn is_history_separator(lines: &[Line], i: usize) -> bool {
    let line = lines[i].text.trim();
    let lower = line.to_lowercase();
    let dashed = line.starts_with("-----") && line.ends_with("-----");
    if dashed
        && [
            "original message",
            "ursprüngliche nachricht",
            "originalnachricht",
        ]
        .iter()
        .any(|m| lower.contains(m))
    {
        return true;
    }
    // "________________________________" followed by "From:"/"Von:".
    if line.len() >= 10 && line.chars().all(|c| c == '_') {
        return lines[i + 1..]
            .iter()
            .find(|l| !l.text.trim().is_empty())
            .is_some_and(|next| {
                let next = next.text.trim_start();
                next.starts_with("From:") || next.starts_with("Von:")
            });
    }
    false
}

/// Build blocks for lines at quote depth `base` and deeper. Signature detection is off
/// inside a signature, which keeps the recursion bounded by the quote depth.
fn build_blocks(lines: &[Line], base: usize, detect_signature: bool) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        if line.depth > base {
            flush_paragraph(&mut para, &mut blocks);
            let end = lines[i..]
                .iter()
                .position(|l| l.depth <= base)
                .map_or(lines.len(), |p| i + p);
            push_quote(&lines[i..end], base, &mut blocks);
            i = end;
            continue;
        }
        if base == 0 && line.depth == 0 && is_history_separator(lines, i) {
            flush_paragraph(&mut para, &mut blocks);
            // Everything that follows is history, one level deeper.
            let shifted: Vec<Line> = lines[i..]
                .iter()
                .map(|l| Line {
                    depth: (l.depth + 1).min(MAX_QUOTE_DEPTH),
                    text: l.text.clone(),
                })
                .collect();
            push_quote(&shifted, base, &mut blocks);
            break;
        }
        if base == 0 && detect_signature && is_signature_separator(line) {
            flush_paragraph(&mut para, &mut blocks);
            let end = lines[i + 1..]
                .iter()
                .position(|l| l.depth > 0)
                .map_or(lines.len(), |p| i + 1 + p);
            let signature = build_blocks(&lines[i + 1..end], 0, false);
            if !signature.is_empty() {
                blocks.push(Block::Signature { blocks: signature });
            }
            i = end;
            continue;
        }
        if line.text.trim().is_empty() {
            flush_paragraph(&mut para, &mut blocks);
        } else {
            para.push(&line.text);
        }
        i += 1;
    }
    flush_paragraph(&mut para, &mut blocks);
    blocks
}

fn push_quote(lines: &[Line], base: usize, blocks: &mut Vec<Block>) {
    let inner = build_blocks(lines, base + 1, false);
    if !inner.is_empty() {
        blocks.push(Block::Quote {
            depth: u8::try_from(base + 1).unwrap_or(u8::MAX),
            blocks: inner,
        });
    }
}

fn flush_paragraph(para: &mut Vec<&str>, blocks: &mut Vec<Block>) {
    if para.is_empty() {
        return;
    }
    let text = para.join("\n");
    para.clear();
    blocks.push(Block::Paragraph {
        content: linkify(&text),
    });
}

static FINDER: LazyLock<LinkFinder> = LazyLock::new(|| {
    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url, LinkKind::Email]);
    finder
});

/// Split text into plain runs and link runs.
pub(crate) fn linkify(text: &str) -> Vec<Inline> {
    let mut out = Vec::new();
    for span in FINDER.spans(text) {
        let target = match span.kind() {
            Some(LinkKind::Url) => Some(span.as_str().to_owned()),
            Some(LinkKind::Email) => Some(format!("mailto:{}", span.as_str())),
            _ => None,
        }
        .filter(|t| is_allowed_link(t));
        match target {
            Some(link) => out.push(Inline {
                text: span.as_str().to_owned(),
                style: Default::default(),
                link: Some(link),
            }),
            None => match out.last_mut() {
                Some(last) if last.link.is_none() => last.text.push_str(span.as_str()),
                _ => out.push(Inline::plain(span.as_str())),
            },
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(text: &str) -> ReaderDoc {
        from_plain_text(text, TextFormat::default())
    }

    #[test]
    fn quotes_signatures_and_links() {
        let doc = fixed(
            "Hi Anna,\r\n\r\nsee https://example.com/x?y=1 or mail bob@example.com.\r\n\
             \r\nOn Monday Anna wrote:\r\n> Lunch?\r\n> > Earlier\r\n>>> deepest\r\n> back\r\n\r\n-- \r\nBob\r\nExample Corp\r\n",
        );
        insta::assert_yaml_snapshot!(doc);
    }

    #[test]
    fn decodes_flowed_text() {
        let text = "This is a long \r\nline that was \r\nwrapped.\r\n> quoted soft \r\n> break\r\n \
                    >not a quote, stuffed\r\n-- \r\nSig";
        let lines = decode_flowed(&text.replace("\r\n", "\n"), false);
        assert_eq!(
            lines,
            vec![
                Line {
                    depth: 0,
                    text: "This is a long line that was wrapped.".into()
                },
                Line {
                    depth: 1,
                    text: "quoted soft break".into()
                },
                Line {
                    depth: 0,
                    text: ">not a quote, stuffed".into()
                },
                Line {
                    depth: 0,
                    text: "-- ".into()
                },
                Line {
                    depth: 0,
                    text: "Sig".into()
                },
            ]
        );
    }

    #[test]
    fn flowed_delsp_removes_the_soft_space() {
        let lines = decode_flowed("Zusammen \ngeschrieben", true);
        assert_eq!(lines[0].text, "Zusammengeschrieben");
    }

    #[test]
    fn outlook_history_becomes_a_quote() {
        let doc =
            fixed("Thanks!\n\n-----Original Message-----\nFrom: Anna\nSent: Monday\n\nOld text\n");
        assert_eq!(doc.blocks.len(), 2);
        assert!(matches!(doc.blocks[1], Block::Quote { depth: 1, .. }));
        let doc = fixed("Danke\n\n________________________________\nVon: Anna\nAlt\n");
        assert!(matches!(doc.blocks[1], Block::Quote { depth: 1, .. }));
    }

    #[test]
    fn hostile_quote_depth_is_clamped() {
        let text = ">".repeat(100_000) + " x";
        let doc = fixed(&text);
        assert!(doc.plain_text().contains('x'));
    }

    #[test]
    fn many_signature_separators_do_not_recurse() {
        let text = "a\n".to_owned() + &"-- \nsig\n".repeat(50_000);
        let doc = fixed(&text);
        assert_eq!(doc.blocks.len(), 2);
    }

    #[test]
    fn linkify_rejects_unsafe_schemes() {
        let runs = linkify("go javascript://example.com/%0aalert(1) now");
        assert!(runs.iter().all(|r| r.link.is_none()), "{runs:?}");
    }
}
