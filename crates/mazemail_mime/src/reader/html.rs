//! Sanitized HTML DOM → [`ReaderDoc`].

use crate::html::dom::{Dom, NodeId};
use crate::html::tracking::{parse_pixels, parse_style};
use crate::html::urls::{Resource, classify_resource, is_allowed_link};

use super::model::{
    Block, DocSource, Image, ImageSource, Inline, InlineStyle, ListItem, ReaderDoc, TableCell,
    TableRow,
};

/// Element nesting beyond this is read as plain text, so hostile input can't exhaust
/// the stack.
const MAX_DEPTH: usize = 128;

/// Quote levels beyond this are clamped.
const MAX_QUOTE_DEPTH: u8 = 32;

/// Images this small in either dimension are spacers and left out.
const SPACER_MAX_PX: u32 = 2;

pub(crate) fn dom_to_reader(dom: &Dom) -> ReaderDoc {
    let mut sink = Sink::default();
    let ctx = Ctx::default();
    walk_children(dom, dom.root(), &ctx, &mut sink, 0);
    ReaderDoc {
        source: DocSource::Html,
        blocks: sink.finish(),
    }
}

/// Inherited inline state.
#[derive(Clone, Default)]
struct Ctx {
    style: InlineStyle,
    link: Option<String>,
    quote_depth: u8,
}

impl Ctx {
    fn with_style(&self, style: InlineStyle) -> Self {
        Self {
            style: self.style | style,
            ..self.clone()
        }
    }
}

/// Collects blocks and the paragraph in progress.
#[derive(Default)]
struct Sink {
    blocks: Vec<Block>,
    runs: Vec<Inline>,
    /// Whitespace was seen since the last character of the current paragraph.
    pending_space: bool,
}

impl Sink {
    fn push_text(&mut self, text: &str, ctx: &Ctx) {
        for c in text.chars() {
            if c.is_whitespace() {
                self.pending_space = true;
                continue;
            }
            if self.pending_space && !self.at_line_start() {
                self.push_space(ctx);
            }
            self.pending_space = false;
            self.push_char(c, ctx);
        }
    }

    /// Emit collapsed whitespace between two runs. It goes to whichever side is
    /// unformatted, so links and underlines don't start or end with a space.
    fn push_space(&mut self, ctx: &Ctx) {
        let plain = |style: InlineStyle, link: &Option<String>| style.is_empty() && link.is_none();
        match self.runs.last_mut() {
            Some(last) if plain(last.style, &last.link) || !plain(ctx.style, &ctx.link) => {
                last.text.push(' ');
            }
            _ => self.push_char(' ', ctx),
        }
    }

    fn line_break(&mut self, ctx: &Ctx) {
        self.pending_space = false;
        self.push_char('\n', ctx);
    }

    fn at_line_start(&self) -> bool {
        self.runs
            .last()
            .and_then(|r| r.text.chars().last())
            .is_none_or(|c| c == '\n')
    }

    fn push_char(&mut self, c: char, ctx: &Ctx) {
        match self.runs.last_mut() {
            Some(last) if last.style == ctx.style && last.link == ctx.link => last.text.push(c),
            _ => self.runs.push(Inline {
                text: c.to_string(),
                style: ctx.style,
                link: ctx.link.clone(),
            }),
        }
    }

    /// End the current paragraph.
    fn flush(&mut self) {
        self.pending_space = false;
        let runs = take_trimmed_runs(&mut self.runs);
        if !runs.is_empty() {
            self.blocks.push(Block::Paragraph { content: runs });
        }
    }

    fn take_inlines(&mut self) -> Vec<Inline> {
        self.pending_space = false;
        take_trimmed_runs(&mut self.runs)
    }

    fn push_block(&mut self, block: Block) {
        self.flush();
        self.blocks.push(block);
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush();
        self.blocks
    }
}

/// Take the runs, dropping leading/trailing line breaks and empty runs.
fn take_trimmed_runs(runs: &mut Vec<Inline>) -> Vec<Inline> {
    let mut runs = std::mem::take(runs);
    while let Some(first) = runs.first_mut() {
        let trimmed = first.text.trim_start_matches('\n');
        if trimmed.is_empty() {
            runs.remove(0);
        } else {
            first.text = trimmed.to_owned();
            break;
        }
    }
    while let Some(last) = runs.last_mut() {
        let trimmed_len = last.text.trim_end_matches(['\n', ' ']).len();
        if trimmed_len == 0 {
            runs.pop();
        } else {
            last.text.truncate(trimmed_len);
            break;
        }
    }
    runs
}

fn walk_children(dom: &Dom, node: NodeId, ctx: &Ctx, sink: &mut Sink, depth: usize) {
    let children = dom.children(node);
    for (i, &child) in children.iter().enumerate() {
        if is_reply_header_boundary(dom, child) {
            // Outlook puts the quoted history after its reply header, as siblings
            // rather than inside a blockquote. Fold the rest of this container.
            let mut inner = Sink::default();
            let quote_ctx = Ctx {
                quote_depth: ctx.quote_depth.saturating_add(1).min(MAX_QUOTE_DEPTH),
                ..Ctx::default()
            };
            for &rest in &children[i..] {
                walk(dom, rest, &quote_ctx, &mut inner, depth + 1);
            }
            let blocks = inner.finish();
            if !blocks.is_empty() {
                sink.push_block(Block::Quote {
                    depth: quote_ctx.quote_depth,
                    blocks,
                });
            }
            return;
        }
        walk(dom, child, ctx, sink, depth + 1);
    }
}

fn walk(dom: &Dom, node: NodeId, ctx: &Ctx, sink: &mut Sink, depth: usize) {
    if let Some(text) = dom.text(node) {
        sink.push_text(text, ctx);
        return;
    }
    let Some(tag) = dom.tag(node) else {
        return;
    };
    if depth > MAX_DEPTH {
        sink.push_text(&dom.text_content(node), ctx);
        return;
    }
    match tag {
        "style" | "script" | "head" | "title" | "template" | "noscript" | "wbr" => {}
        "br" => sink.line_break(ctx),
        "img" => image(dom, node, ctx, sink),
        "b" | "strong" => walk_children(dom, node, &ctx.with_style(InlineStyle::BOLD), sink, depth),
        "i" | "em" | "cite" | "dfn" | "var" => {
            walk_children(dom, node, &ctx.with_style(InlineStyle::ITALIC), sink, depth);
        }
        "u" | "ins" => {
            walk_children(
                dom,
                node,
                &ctx.with_style(InlineStyle::UNDERLINE),
                sink,
                depth,
            );
        }
        "s" | "strike" | "del" => {
            walk_children(dom, node, &ctx.with_style(InlineStyle::STRIKE), sink, depth);
        }
        "code" | "tt" | "kbd" | "samp" => {
            walk_children(dom, node, &ctx.with_style(InlineStyle::CODE), sink, depth);
        }
        "span" | "font" => {
            let style = dom
                .attr(node, "style")
                .map(css_inline_style)
                .unwrap_or_default();
            walk_children(dom, node, &ctx.with_style(style), sink, depth);
        }
        "a" => {
            let link = dom
                .attr(node, "href")
                .map(str::trim)
                .filter(|href| !href.starts_with('#') && is_allowed_link(href))
                .map(str::to_owned);
            let inner = Ctx {
                link: link.or_else(|| ctx.link.clone()),
                ..ctx.clone()
            };
            walk_children(dom, node, &inner, sink, depth);
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => heading(dom, node, tag, ctx, sink, depth),
        "ul" | "ol" => list(dom, node, tag == "ol", ctx, sink, depth),
        "blockquote" => quote(dom, node, ctx, sink, depth),
        "pre" => {
            let text = dom.text_content(node);
            let text = text.trim_end_matches(['\n', '\r']);
            if !text.trim().is_empty() {
                sink.push_block(Block::Preformatted {
                    text: text.to_owned(),
                });
            }
        }
        "hr" => sink.push_block(Block::Rule),
        "table" => table(dom, node, ctx, sink, depth),
        _ if is_block_element(tag) => {
            sink.flush();
            walk_children(dom, node, ctx, sink, depth);
            sink.flush();
        }
        _ => walk_children(dom, node, ctx, sink, depth),
    }
}

fn is_block_element(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "div"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "center"
            | "address"
            | "main"
            | "nav"
            | "aside"
            | "figure"
            | "figcaption"
            | "details"
            | "summary"
            | "dl"
            | "dd"
            | "dt"
            | "hgroup"
            | "caption"
            | "li"
            | "tr"
            | "td"
            | "th"
            | "tbody"
            | "thead"
            | "tfoot"
            | "body"
            | "html"
    )
}

/// Bold/italic/underline/strike from an inline `style`.
fn css_inline_style(style: &str) -> InlineStyle {
    let mut out = InlineStyle::empty();
    for (prop, value) in parse_style(style) {
        match prop.as_str() {
            "font-weight" => {
                let heavy = value == "bold"
                    || value == "bolder"
                    || value.parse::<u32>().is_ok_and(|w| w >= 600);
                if heavy {
                    out |= InlineStyle::BOLD;
                }
            }
            "font-style" if value == "italic" || value == "oblique" => out |= InlineStyle::ITALIC,
            "text-decoration" | "text-decoration-line" => {
                if value.contains("underline") {
                    out |= InlineStyle::UNDERLINE;
                }
                if value.contains("line-through") {
                    out |= InlineStyle::STRIKE;
                }
            }
            _ => {}
        }
    }
    out
}

/// Outlook's reply/forward header: everything from here on is quoted history.
fn is_reply_header_boundary(dom: &Dom, node: NodeId) -> bool {
    let Some(tag) = dom.tag(node) else {
        return false;
    };
    match dom.attr(node, "id") {
        Some("divRplyFwdMsg" | "appendonsend") => return true,
        Some("stopSpelling") if tag == "hr" => return true,
        _ => {}
    }
    if tag != "div" {
        return false;
    }
    // Outlook desktop: <div style="border:none;border-top:solid #E1E1E1 1.0pt;...">
    // wrapping the "From: / Sent: / To: / Subject:" block.
    dom.attr(node, "style").is_some_and(|style| {
        let style = style.to_ascii_lowercase().replace(' ', "");
        style.contains("border-top:solid#e1e1e1") || style.contains("border-top:solid#b5c4df")
    })
}

fn image(dom: &Dom, node: NodeId, ctx: &Ctx, sink: &mut Sink) {
    let Some(src) = dom.attr(node, "src") else {
        return;
    };
    let source = match classify_resource(src) {
        Resource::Cid(id) => ImageSource::Cid(id),
        Resource::Remote(url) => ImageSource::Remote(url),
        Resource::DataImage => ImageSource::Data(src.trim().to_owned()),
        Resource::Rejected => return,
    };
    let width = dom.attr(node, "width").and_then(parse_pixels);
    let height = dom.attr(node, "height").and_then(parse_pixels);
    if width.is_some_and(|w| w <= SPACER_MAX_PX) || height.is_some_and(|h| h <= SPACER_MAX_PX) {
        return;
    }
    let alt = dom
        .attr(node, "alt")
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_owned);
    sink.push_block(Block::Image(Image {
        source,
        alt,
        width,
        height,
        link: ctx.link.clone(),
    }));
}

fn heading(dom: &Dom, node: NodeId, tag: &str, ctx: &Ctx, sink: &mut Sink, depth: usize) {
    sink.flush();
    let level = tag.get(1..).and_then(|n| n.parse::<u8>().ok()).unwrap_or(1);
    let mut inner = Sink::default();
    walk_children(dom, node, ctx, &mut inner, depth);
    let content = inner.take_inlines();
    if !content.is_empty() {
        sink.blocks.push(Block::Heading { level, content });
    }
    // Images and other blocks inside the heading follow it.
    sink.blocks.extend(inner.finish());
}

fn list(dom: &Dom, node: NodeId, ordered: bool, ctx: &Ctx, sink: &mut Sink, depth: usize) {
    sink.flush();
    let mut items = Vec::new();
    let mut loose = Sink::default();
    for &child in dom.children(node) {
        if dom.tag(child) == Some("li") {
            push_loose_item(&mut loose, &mut items);
            let mut item = Sink::default();
            walk_children(dom, child, ctx, &mut item, depth + 1);
            items.push(ListItem {
                blocks: item.finish(),
            });
        } else {
            // Text or stray elements directly inside <ul>: keep them as an item.
            walk(dom, child, ctx, &mut loose, depth + 1);
        }
    }
    push_loose_item(&mut loose, &mut items);
    if items.is_empty() {
        return;
    }
    let start = ordered
        .then(|| {
            dom.attr(node, "start")
                .and_then(|s| s.trim().parse::<u32>().ok())
        })
        .flatten()
        .filter(|&s| s != 1);
    sink.blocks.push(Block::List {
        ordered,
        start,
        items,
    });
}

fn push_loose_item(loose: &mut Sink, items: &mut Vec<ListItem>) {
    let blocks = std::mem::take(loose).finish();
    if !blocks.is_empty() {
        items.push(ListItem { blocks });
    }
}

fn quote(dom: &Dom, node: NodeId, ctx: &Ctx, sink: &mut Sink, depth: usize) {
    sink.flush();
    let quote_depth = ctx.quote_depth.saturating_add(1).min(MAX_QUOTE_DEPTH);
    let inner_ctx = Ctx {
        quote_depth,
        ..Ctx::default()
    };
    let mut inner = Sink::default();
    walk_children(dom, node, &inner_ctx, &mut inner, depth);
    let blocks = inner.finish();
    if !blocks.is_empty() {
        sink.blocks.push(Block::Quote {
            depth: quote_depth,
            blocks,
        });
    }
}

/// Rows of a table: `tr` children of the table and of its row groups.
fn table_rows(dom: &Dom, table: NodeId) -> Vec<NodeId> {
    let mut rows = Vec::new();
    for &child in dom.children(table) {
        match dom.tag(child) {
            Some("tr") => rows.push(child),
            Some("thead" | "tbody" | "tfoot") => rows.extend(
                dom.children(child)
                    .iter()
                    .copied()
                    .filter(|&r| dom.tag(r) == Some("tr")),
            ),
            _ => {}
        }
    }
    rows
}

fn row_cells(dom: &Dom, row: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    dom.children(row)
        .iter()
        .copied()
        .filter(move |&c| matches!(dom.tag(c), Some("td" | "th")))
}

/// Elements that don't belong in a data table cell.
fn is_layout_content(tag: &str) -> bool {
    matches!(
        tag,
        "table"
            | "img"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "blockquote"
            | "pre"
            | "hr"
    )
}

/// Decide whether a table carries data or only lays out the message.
///
/// Mail is full of layout tables (newsletters nest them several levels deep). A table
/// is treated as data only when it has more than one column, no layout content in its
/// cells (images, nested tables, headings, lists) and some evidence of being tabular:
/// header cells, a caption, a border, or at least two rows without layout sizing.
fn is_layout_table(dom: &Dom, table: NodeId, rows: &[NodeId]) -> bool {
    if matches!(dom.attr(table, "role"), Some("presentation" | "none")) {
        return true;
    }
    let max_cols = rows
        .iter()
        .map(|&r| row_cells(dom, r).count())
        .max()
        .unwrap_or(0);
    if max_cols <= 1 {
        return true;
    }
    let mut has_header = dom
        .children(table)
        .iter()
        .any(|&c| matches!(dom.tag(c), Some("caption" | "thead")));
    for &row in rows {
        for cell in row_cells(dom, row) {
            has_header |= dom.tag(cell) == Some("th");
            if dom
                .descendants(cell)
                .into_iter()
                .any(|d| dom.tag(d).is_some_and(is_layout_content))
            {
                return true;
            }
        }
    }
    if has_header {
        return false;
    }
    let bordered = dom
        .attr(table, "border")
        .and_then(|b| b.trim().parse::<u32>().ok())
        .is_some_and(|b| b > 0)
        || dom.attr(table, "style").is_some_and(|s| {
            parse_style(s)
                .iter()
                .any(|(p, v)| p.starts_with("border") && !v.contains("none") && v != "0")
        });
    if bordered {
        return false;
    }
    let sized_for_layout = dom.attr(table, "width").is_some()
        || dom.attr(table, "cellpadding").is_some()
        || dom.attr(table, "align").is_some();
    rows.len() < 2 || sized_for_layout
}

fn table(dom: &Dom, node: NodeId, ctx: &Ctx, sink: &mut Sink, depth: usize) {
    sink.flush();
    let rows = table_rows(dom, node);
    if is_layout_table(dom, node, &rows) {
        // Flatten: every cell becomes a run of blocks, in reading order.
        for &row in &rows {
            for cell in row_cells(dom, row) {
                sink.flush();
                walk_children(dom, cell, ctx, sink, depth + 1);
                sink.flush();
            }
        }
        // Content the parser left directly in the table (rare after sanitizing).
        for &child in dom.children(node) {
            if dom.tag(child) == Some("caption") {
                walk(dom, child, ctx, sink, depth + 1);
            }
        }
        return;
    }
    let mut out_rows = Vec::with_capacity(rows.len());
    for &row in &rows {
        let cells = row_cells(dom, row)
            .map(|cell| {
                let mut inner = Sink::default();
                walk_children(dom, cell, ctx, &mut inner, depth + 1);
                TableCell {
                    header: dom.tag(cell) == Some("th"),
                    content: flatten_to_inlines(inner.finish()),
                }
            })
            .collect::<Vec<_>>();
        if !cells.is_empty() {
            out_rows.push(TableRow { cells });
        }
    }
    if !out_rows.is_empty() {
        sink.blocks.push(Block::Table { rows: out_rows });
    }
}

/// Join paragraphs into one inline sequence separated by line breaks.
fn flatten_to_inlines(blocks: Vec<Block>) -> Vec<Inline> {
    let mut out: Vec<Inline> = Vec::new();
    for block in blocks {
        let content = match block {
            Block::Paragraph { content } | Block::Heading { content, .. } => content,
            Block::Preformatted { text } => vec![Inline::plain(text)],
            Block::Image(Image { alt: Some(alt), .. }) => vec![Inline::plain(alt)],
            _ => continue,
        };
        if !out.is_empty() {
            out.push(Inline::plain("\n"));
        }
        out.extend(content);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(html: &str) -> ReaderDoc {
        dom_to_reader(&Dom::parse_fragment(html))
    }

    fn texts(doc: &ReaderDoc) -> Vec<String> {
        doc.blocks
            .iter()
            .map(|b| match b {
                Block::Paragraph { content } => content.iter().map(|i| i.text.as_str()).collect(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn collapses_whitespace_and_keeps_breaks() {
        let doc = read("<p>  Hello \n\n  <b>big</b>   world<br>  next line </p><div>x</div>");
        assert_eq!(texts(&doc), vec!["Hello big world\nnext line", "x"]);
    }

    #[test]
    fn inline_styles_and_links() {
        let doc = read(
            r#"<p><b>B</b><i>I</i><span style="font-weight:700;text-decoration:underline">S</span>
            <a href="https://e.com">link</a><a href="javascript:x">bad</a><code>c</code></p>"#,
        );
        insta::assert_yaml_snapshot!(doc);
    }

    #[test]
    fn lists_quotes_headings() {
        let doc = read(
            "<h2>Title</h2><ol start=3><li>one<ul><li>nested</li></ul></li><li>two</li></ol>\
             <blockquote>outer<blockquote>inner</blockquote></blockquote><hr><pre>  a\n  b\n</pre>",
        );
        insta::assert_yaml_snapshot!(doc);
    }

    #[test]
    fn flattens_layout_tables_and_keeps_data_tables() {
        let doc = read(
            r#"<table width="600" role="presentation"><tr><td><table><tr><td>Header</td></tr></table></td></tr>
            <tr><td><p>Body text</p></td></tr></table>
            <table border="1"><tr><th>Name</th><th>Qty</th></tr><tr><td>Apples</td><td>3</td></tr></table>
            <table><tr><td><img src="cid:logo" width="80" height="20"></td><td>Logo text</td></tr></table>"#,
        );
        insta::assert_yaml_snapshot!(doc);
    }

    #[test]
    fn outlook_reply_header_starts_a_quote() {
        let doc = read(
            r#"<div><p>My answer</p><div style="border:none;border-top:solid #E1E1E1 1.0pt;padding:3.0pt 0in 0in 0in">
            <p><b>From:</b> Anna</p></div><p>Original text</p></div>"#,
        );
        assert_eq!(doc.blocks.len(), 2);
        let Block::Quote { depth, blocks } = &doc.blocks[1] else {
            panic!("expected quote: {doc:#?}");
        };
        assert_eq!(*depth, 1);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn drops_spacers_and_unusable_images() {
        let doc = read(
            r#"<img src="cid:spacer" width="1" height="10"><img src="relative.png"><img src="https://e.com/a.png" alt=" A ">"#,
        );
        assert_eq!(doc.blocks.len(), 1);
        assert!(matches!(&doc.blocks[0], Block::Image(Image { alt: Some(a), .. }) if a == "A"));
    }

    #[test]
    fn deep_nesting_is_read_as_text() {
        let html = "<div><b>".repeat(5_000) + "deep" + &"</b></div>".repeat(5_000);
        let doc = read(&html);
        assert!(doc.plain_text().contains("deep"));
        let quotes = "<blockquote>".repeat(100) + "q";
        assert!(read(&quotes).plain_text().contains('q'));
    }
}
