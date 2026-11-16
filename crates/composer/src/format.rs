//! Markdown formatting keys of the body (`cmd-b`, `cmd-i`, `cmd-k`, …), as text edits.
//!
//! The body is Markdown, so formatting is text: bold wraps the selection in `**`, a
//! link turns it into `[text](url)`. Each key toggles: pressed again on formatted text
//! (with or without its markers selected) it takes the markers away. Everything here
//! is pure; the composer applies the [`Edit`] to the body as one undo step.
//!
//! - **No selection**: an empty pair of markers with the caret between them (pressed
//!   again right away, the pair goes again).
//! - **Spaces** at the ends of the selection stay outside the markers (`**word** `, not
//!   `**word **`, which Markdown wouldn't read as bold).
//! - **Several lines**: every line is formatted on its own, since emphasis can't span
//!   paragraphs. Blank lines are left alone.

use std::ops::Range;

/// A change to the body: write `text` over `range`, then select `select`. Ranges are
/// byte offsets; `select` is in the text after the change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Edit {
    pub range: Range<usize>,
    pub text: String,
    pub select: Range<usize>,
}

/// Inline Markdown that wraps text in a marker on both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Style {
    Bold,
    Italic,
    Strikethrough,
    Code,
}

impl Style {
    /// `_` for italics, so it can't be mistaken for half of a bold `**`.
    fn marker(self) -> &'static str {
        match self {
            Self::Bold => "**",
            Self::Italic => "_",
            Self::Strikethrough => "~~",
            Self::Code => "`",
        }
    }
}

/// The placeholder selected in a new link when no address is at hand, so typing
/// replaces it (as on GitHub).
const URL_PLACEHOLDER: &str = "url";

/// Formats the selection `selection` of `text` with `style`, or takes the formatting
/// away if it is there already. `None` when there is nothing to do (only spaces
/// selected).
pub(crate) fn toggle(text: &str, selection: Range<usize>, style: Style) -> Option<Edit> {
    let marker = style.marker();
    let n = marker.len();
    let selection = clamp(text, selection);

    if selection.is_empty() {
        let at = selection.start;
        // Right after inserting an empty pair, the same key takes it away again.
        if text[..at].ends_with(marker) && text[at..].starts_with(marker) {
            return Some(Edit {
                range: at - n..at + n,
                text: String::new(),
                select: at - n..at - n,
            });
        }
        return Some(Edit {
            range: at..at,
            text: format!("{marker}{marker}"),
            select: at + n..at + n,
        });
    }

    let segments = segments(text, selection.clone());
    let (first, last) = (segments.first()?.clone(), segments.last()?.clone());
    let wrapping: Vec<Wrapped> = segments
        .iter()
        .map(|segment| wrapped(text, segment.clone(), marker))
        .collect();
    let unwrap = wrapping.iter().all(|w| *w != Wrapped::No);

    // The edit covers the selection plus any markers just outside it.
    let start = match wrapping.first() {
        Some(Wrapped::Outside) if unwrap => first.start - n,
        _ => selection.start.min(first.start),
    };
    let end = match wrapping.last() {
        Some(Wrapped::Outside) if unwrap => last.end + n,
        _ => selection.end.max(last.end),
    };

    let mut out = String::with_capacity(end - start + 2 * n * segments.len());
    let mut copied = start;
    for (segment, wrapping) in segments.iter().zip(&wrapping) {
        let (outer, inner) = match wrapping {
            Wrapped::Outside => (segment.start - n..segment.end + n, segment.clone()),
            Wrapped::Inside => (segment.clone(), segment.start + n..segment.end - n),
            Wrapped::No => (segment.clone(), segment.clone()),
        };
        if unwrap {
            out.push_str(&text[copied..outer.start]);
            out.push_str(&text[inner]);
            copied = outer.end;
        } else {
            out.push_str(&text[copied..segment.start]);
            if *wrapping == Wrapped::No {
                out.push_str(marker);
                out.push_str(&text[segment.clone()]);
                out.push_str(marker);
            } else {
                out.push_str(&text[segment.clone()]);
            }
            copied = segment.end;
        }
    }
    out.push_str(&text[copied..end]);

    // One line: select the words (without markers), so another key can format them
    // too. Several lines: select everything that changed.
    let select = if segments.len() == 1 {
        let len = first.end - first.start;
        match (unwrap, wrapping[0]) {
            (true, Wrapped::Outside) => start..start + len,
            (true, Wrapped::Inside) => first.start..first.start + len - 2 * n,
            _ => first.start + n..first.start + n + len,
        }
    } else {
        start..start + out.len()
    };
    Some(Edit {
        range: start..end,
        text: out,
        select,
    })
}

/// Makes the selection a link, or takes a link away.
///
/// - Selected text becomes `[text](url)`; the address is `clipboard` when that holds
///   one, or the placeholder `url`, selected so typing replaces it.
/// - A selected address becomes `[address](address)` with the text selected.
/// - A selected link, or the text of one, goes back to its text.
/// - Nothing selected: `[](address)` with the caret between the brackets, or
///   `[](url)` with the placeholder selected.
pub(crate) fn link(text: &str, selection: Range<usize>, clipboard: Option<&str>) -> Edit {
    let selection = clamp(text, selection);
    let clipboard = clipboard.map(str::trim).filter(|c| is_url(c));
    let selection = match segments(text, selection.clone()).as_slice() {
        [only] => only.clone(),
        _ => selection,
    };
    let selected = &text[selection.clone()];

    // `[text](url)` selected: back to `text`.
    if let Some(label) = link_label(selected) {
        let start = selection.start;
        return Edit {
            range: selection,
            text: label.to_owned(),
            select: start..start + label.len(),
        };
    }
    // The text of a link selected (between `[` and `](…)`): back to the text.
    if !selected.is_empty()
        && text[..selection.start].ends_with('[')
        && let Some(after) = text[selection.end..].strip_prefix("](")
        && let Some(close) = after.find(')')
        && !after[..close].contains(char::is_whitespace)
    {
        let start = selection.start - 1;
        return Edit {
            range: start..selection.end + 2 + close + 1,
            text: selected.to_owned(),
            select: start..start + selected.len(),
        };
    }

    let start = selection.start;
    if is_url(selected) {
        let text = format!("[{selected}]({selected})");
        return Edit {
            range: selection,
            text,
            select: start + 1..start + 1 + selected.len(),
        };
    }
    match clipboard {
        Some(url) => {
            let text = format!("[{selected}]({url})");
            let at = if selected.is_empty() {
                start + 1
            } else {
                start + text.len()
            };
            Edit {
                range: selection,
                text,
                select: at..at,
            }
        }
        None => {
            let text = format!("[{selected}]({URL_PLACEHOLDER})");
            let url = start + 1 + selected.len() + 2;
            Edit {
                range: selection,
                text,
                select: url..url + URL_PLACEHOLDER.len(),
            }
        }
    }
}

/// Where a segment's markers are, if it has them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wrapped {
    No,
    /// The markers are part of the segment (`**bold**` selected).
    Inside,
    /// The markers are just outside it (`bold` selected in `**bold**`).
    Outside,
}

fn wrapped(text: &str, segment: Range<usize>, marker: &str) -> Wrapped {
    let n = marker.len();
    let inner = &text[segment.clone()];
    if text[..segment.start].ends_with(marker)
        && text[segment.end..].starts_with(marker)
        && !doubled(text, segment.start - n, segment.end + n, marker)
    {
        Wrapped::Outside
    } else if inner.len() >= 2 * n
        && inner.starts_with(marker)
        && inner.ends_with(marker)
        && !doubled(text, segment.start, segment.end, marker)
    {
        Wrapped::Inside
    } else {
        Wrapped::No
    }
}

/// A one-character marker that is half of a longer run (`*` of `**`, `` ` `` of a code
/// fence) doesn't count as the marker around `start..end`.
fn doubled(text: &str, start: usize, end: usize, marker: &str) -> bool {
    marker.len() == 1 && (text[..start].ends_with(marker) || text[end..].starts_with(marker))
}

/// The selection's lines, each without the spaces at its ends; blank lines are left
/// out.
fn segments(text: &str, selection: Range<usize>) -> Vec<Range<usize>> {
    let mut segments = Vec::new();
    let mut line_start = selection.start;
    for line in text[selection].split('\n') {
        let leading = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            let start = line_start + leading;
            segments.push(start..start + trimmed.len());
        }
        line_start += line.len() + 1;
    }
    segments
}

/// `[label](url)` → `label`.
fn link_label(selected: &str) -> Option<&str> {
    let rest = selected.strip_prefix('[')?.strip_suffix(')')?;
    let (label, url) = rest.rsplit_once("](")?;
    (!url.contains(char::is_whitespace) && !label.contains('\n')).then_some(label)
}

fn is_url(text: &str) -> bool {
    let schemes = ["https://", "http://", "mailto:"];
    !text.is_empty()
        && !text.contains(char::is_whitespace)
        && schemes.iter().any(|s| {
            text.len() > s.len()
                && text
                    .get(..s.len())
                    .is_some_and(|scheme| scheme.eq_ignore_ascii_case(s))
        })
}

/// `selection` inside `text` and on character boundaries.
fn clamp(text: &str, selection: Range<usize>) -> Range<usize> {
    let mut start = selection.start.min(text.len());
    let mut end = selection.end.clamp(start, text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    while !text.is_char_boundary(end) {
        end += 1;
    }
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies `edit` and writes the result with the selection as `«…»` (or `|` for
    /// the caret), so a test reads like the text it expects.
    fn apply(text: &str, edit: &Edit) -> String {
        let mut out = text.to_owned();
        out.replace_range(edit.range.clone(), &edit.text);
        let Range { start, end } = edit.select.clone();
        if start == end {
            out.insert(start, '|');
        } else {
            out.insert(end, '»');
            out.insert(start, '«');
        }
        out
    }

    /// `text` written with the selection as `«…»` or `|`, as plain text and a range.
    fn parse(marked: &str) -> (String, Range<usize>) {
        if let Some(at) = marked.find('|') {
            return (marked.replacen('|', "", 1), at..at);
        }
        let start = marked.find('«').unwrap();
        let text = marked.replacen('«', "", 1);
        let end = text.find('»').unwrap();
        (text.replacen('»', "", 1), start..end)
    }

    fn fmt(marked: &str, style: Style) -> String {
        let (text, selection) = parse(marked);
        match toggle(&text, selection, style) {
            Some(edit) => apply(&text, &edit),
            None => "(nothing)".into(),
        }
    }

    fn lnk(marked: &str, clipboard: Option<&str>) -> String {
        let (text, selection) = parse(marked);
        apply(&text, &link(&text, selection, clipboard))
    }

    #[test]
    fn bold_wraps_the_selection_and_keeps_it_selected() {
        assert_eq!(fmt("Say «hello» now", Style::Bold), "Say **«hello»** now");
        assert_eq!(fmt("«Grüße» 🎉", Style::Bold), "**«Grüße»** 🎉");
    }

    #[test]
    fn pressed_again_it_takes_the_markers_away() {
        assert_eq!(fmt("Say **«hello»** now", Style::Bold), "Say «hello» now");
        assert_eq!(fmt("Say «**hello**» now", Style::Bold), "Say «hello» now");
    }

    #[test]
    fn spaces_at_the_ends_stay_outside() {
        assert_eq!(fmt("Say« hello »now", Style::Bold), "Say **«hello»** now");
        assert_eq!(fmt("«   »", Style::Bold), "(nothing)");
    }

    #[test]
    fn without_a_selection_an_empty_pair_goes_in_and_out() {
        assert_eq!(fmt("Say |now", Style::Bold), "Say **|**now");
        assert_eq!(fmt("Say **|**now", Style::Bold), "Say |now");
        assert_eq!(fmt("|", Style::Code), "`|`");
    }

    #[test]
    fn each_style_has_its_marker() {
        assert_eq!(fmt("«a»", Style::Italic), "_«a»_");
        assert_eq!(fmt("«a»", Style::Strikethrough), "~~«a»~~");
        assert_eq!(fmt("«a»", Style::Code), "`«a»`");
    }

    #[test]
    fn styles_combine() {
        // Italic inside bold: the bold markers aren't italic ones.
        assert_eq!(fmt("**«a»**", Style::Italic), "**_«a»_**");
        assert_eq!(fmt("**_«a»_**", Style::Italic), "**«a»**");
        // Code in a code fence's backticks isn't inline code.
        assert_eq!(fmt("``«a»``", Style::Code), "```«a»```");
    }

    #[test]
    fn every_line_is_formatted_on_its_own() {
        assert_eq!(fmt("«one\n\n  two»", Style::Bold), "«**one**\n\n  **two**»");
        assert_eq!(fmt("«**one**\n**two**»", Style::Bold), "«one\ntwo»");
        // Partly bold: the rest gets bold too, nothing loses it.
        assert_eq!(fmt("«**one**\ntwo»", Style::Bold), "«**one**\n**two**»");
    }

    #[test]
    fn selected_text_becomes_a_link_with_the_address_to_type() {
        assert_eq!(lnk("See «the docs».", None), "See [the docs](«url»).");
    }

    #[test]
    fn an_address_on_the_clipboard_is_used() {
        assert_eq!(
            lnk("See «the docs».", Some(" https://maze.example/docs\n")),
            "See [the docs](https://maze.example/docs)|."
        );
        // Not an address: the placeholder again.
        assert_eq!(lnk("«docs»", Some("just words")), "[docs](«url»)");
    }

    #[test]
    fn a_selected_address_links_to_itself() {
        assert_eq!(
            lnk("Go to «https://maze.example» now", None),
            "Go to [«https://maze.example»](https://maze.example) now"
        );
    }

    #[test]
    fn without_a_selection_the_link_is_empty() {
        assert_eq!(lnk("Hi |", None), "Hi [](«url»)");
        assert_eq!(
            lnk("Hi |", Some("mailto:anna@example.com")),
            "Hi [|](mailto:anna@example.com)"
        );
    }

    #[test]
    fn a_link_goes_back_to_its_text() {
        assert_eq!(lnk("See «[docs](https://x.example)».", None), "See «docs».");
        assert_eq!(lnk("See [«docs»](https://x.example).", None), "See «docs».");
    }
}
