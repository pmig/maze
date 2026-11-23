//! Presentational attributes as CSS, for the embedded engine (the original mode,
//! ADR-033).
//!
//! Browsers turn legacy attributes (`<font color>`, `cellpadding`, `valign`, …) into
//! CSS of the lowest author precedence, the HTML spec's "presentational hints", and
//! mail still relies on them. Blitz maps only some (`width`, `height`, `bgcolor`,
//! `hspace`, `vspace`, `border` on images, `align` as text alignment). This pass writes
//! the others as rules of a generated style sheet that goes before the message's own
//! styles. Each element gets a `data-maze-hint="<n>"` attribute (the sanitizer removes
//! every `data-` attribute of the message, so only this pass sets it); the rules
//! use `:where([data-maze-hint="<n>"])`, a selector without specificity: as with a
//! browser's hints, any CSS of the message wins over them.
//!
//! Blitz (0.3) leaves a row's children out of the table unless they are table cells,
//! so `<td style="display: block">` (GitHub's mail puts the whole message in one)
//! disappeared with everything in it. A browser lays such a cell out as a block in an
//! anonymous table cell; this pass does the same with elements: the `<td>` stays as
//! the cell while a new `<div>` inside it gets the cell's style, class, id and
//! presentational attributes. In GitHub's mail the block has `margin: 0 auto` and
//! centers itself in the cell, which takes the table's spare width.
//!
//! A browser gives a table's spare width to its auto-width columns in proportion to
//! their content, so an empty column (a spacer cell) next to one with content gets
//! none; Blitz shares it out evenly. Empty columns get `width: 0` when another
//! auto-width column has content. (Only empty columns of that kind: a fixed-width
//! column between two empty spacers stays centered, as Blitz shares evenly there too.)
//!
//! And it fits `colspan`s to the table's grid, for two things Google Flights' price bar
//! does. A browser gives a column that only spanning cells reach no width; Blitz shares
//! the table's width with it, so with two cells per row and then `<td colspan=5>` the
//! colored cells filled two fifths of the bar. And a browser draws a spanning cell over
//! a slot that a `rowspan` from an earlier row still takes; Blitz lays tables out on a
//! grid where cells can't overlap and moves such a cell to new columns after the
//! table's, which take width the same way. So a `colspan` ends before the first taken
//! slot and then shrinks to the columns some cell starts in.
//!
//! Values from the message are reduced to what the property needs (colors, font names,
//! whole numbers, the already rewritten `background` URL), so they can't end the rule.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use super::dom::{Dom, NodeId};

/// The attribute that ties an element to its rule.
const HINT_ATTRIBUTE: &str = "data-maze-hint";

/// Applies the hints below `root`. Returns the style sheet to put before the
/// message's content, or `None` when no element needed one.
pub(crate) fn apply(dom: &mut Dom, root: NodeId) -> Option<String> {
    // First, so the tables below see their cells as they will be laid out.
    for node in dom.descendants(root) {
        if matches!(dom.tag(node), Some("td" | "th")) {
            unwrap_block_cell(dom, node);
        }
    }
    let mut hints = Hints::default();
    for node in dom.descendants(root) {
        let Some(tag) = dom.tag(node) else { continue };
        match tag {
            "font" => font(dom, node, &mut hints),
            "table" => {
                table(dom, node, &mut hints);
                fit_columns(dom, node, &mut hints);
            }
            "td" | "th" => cell(dom, node, &mut hints),
            "tr" | "tbody" | "thead" | "tfoot" => row(dom, node, &mut hints),
            "img" => image(dom, node, &mut hints),
            _ => {}
        }
    }
    hints.finish(dom)
}

/// Declarations per element, in the order they were found.
#[derive(Default)]
struct Hints {
    declarations: HashMap<NodeId, Vec<String>>,
    order: Vec<NodeId>,
}

impl Hints {
    fn add(&mut self, node: NodeId, declaration: String) {
        let list = self.declarations.entry(node).or_insert_with(|| {
            self.order.push(node);
            Vec::new()
        });
        if !list.contains(&declaration) {
            list.push(declaration);
        }
    }

    fn has(&self, node: NodeId, property: &str) -> bool {
        self.declarations.get(&node).is_some_and(|list| {
            list.iter()
                .any(|declaration| declaration.starts_with(&format!("{property}:")))
        })
    }

    /// Tags the elements and builds one rule per distinct set of declarations.
    fn finish(self, dom: &mut Dom) -> Option<String> {
        let mut rules: HashMap<String, usize> = HashMap::new();
        let mut sheet = String::new();
        for node in self.order {
            let Some(list) = self.declarations.get(&node) else {
                continue;
            };
            let body = list.join(";");
            let next = rules.len();
            let n = *rules.entry(body.clone()).or_insert_with(|| {
                let _ = writeln!(sheet, ":where([{HINT_ATTRIBUTE}=\"{next}\"]){{{body}}}");
                next
            });
            dom.set_attr(node, HINT_ATTRIBUTE, &n.to_string());
        }
        (!sheet.is_empty()).then_some(sheet)
    }
}

fn font(dom: &Dom, node: NodeId, hints: &mut Hints) {
    if let Some(color) = dom.attr(node, "color").and_then(color) {
        hints.add(node, format!("color:{color}"));
    }
    if let Some(families) = dom.attr(node, "face").and_then(font_families) {
        hints.add(node, format!("font-family:{families}"));
    }
    if let Some(size) = dom.attr(node, "size").and_then(font_size) {
        hints.add(node, format!("font-size:{size}"));
    }
}

fn table(dom: &Dom, node: NodeId, hints: &mut Hints) {
    // `border` without a value (or one that isn't a number) means 1.
    let border = dom
        .attr(node, "border")
        .map(|value| pixels(value).unwrap_or(1));
    if let Some(width) = border.filter(|&width| width > 0) {
        hints.add(node, format!("border:{width}px outset grey"));
    }
    if let Some(spacing) = dom.attr(node, "cellspacing").and_then(pixels) {
        hints.add(node, format!("border-spacing:{spacing}px"));
    }
    match dom
        .attr(node, "align")
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("center") => {
            hints.add(node, "margin-left:auto".to_owned());
            hints.add(node, "margin-right:auto".to_owned());
        }
        Some(side @ ("left" | "right")) => hints.add(node, format!("float:{side}")),
        _ => {}
    }
    background(dom, node, hints);

    let padding = dom.attr(node, "cellpadding").and_then(pixels);
    if border.is_none_or(|width| width == 0) && padding.is_none() {
        return;
    }
    for cell in own_cells(dom, node) {
        if border.is_some_and(|width| width > 0) {
            hints.add(cell, "border:1px inset grey".to_owned());
        }
        if let Some(padding) = padding {
            hints.add(cell, format!("padding:{padding}px"));
        }
    }
}

fn cell(dom: &Dom, node: NodeId, hints: &mut Hints) {
    if let Some(align) = dom.attr(node, "valign").and_then(vertical_align) {
        hints.add(node, format!("vertical-align:{align}"));
    }
    if dom.attr(node, "nowrap").is_some() {
        hints.add(node, "white-space:nowrap".to_owned());
    }
    background(dom, node, hints);
}

/// `valign` on a row or row group applies to its cells that have none of their own.
fn row(dom: &Dom, node: NodeId, hints: &mut Hints) {
    background(dom, node, hints);
    let Some(align) = dom.attr(node, "valign").and_then(vertical_align) else {
        return;
    };
    let rows = if dom.tag(node) == Some("tr") {
        vec![node]
    } else {
        element_children(dom, node, &["tr"])
    };
    for row in rows {
        for cell in element_children(dom, row, &["td", "th"]) {
            if dom.attr(cell, "valign").is_none() && !hints.has(cell, "vertical-align") {
                hints.add(cell, format!("vertical-align:{align}"));
            }
        }
    }
}

fn image(dom: &Dom, node: NodeId, hints: &mut Hints) {
    match dom
        .attr(node, "align")
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some(side @ ("left" | "right")) => hints.add(node, format!("float:{side}")),
        Some(align) => {
            if let Some(align) = vertical_align(align) {
                hints.add(node, format!("vertical-align:{align}"));
            }
        }
        None => {}
    }
}

/// `background="<url>"`: the resource pass has pointed it at the resource origin (or
/// removed it), so it only needs quoting.
fn background(dom: &Dom, node: NodeId, hints: &mut Hints) {
    if let Some(url) = dom.attr(node, "background").and_then(css_url) {
        hints.add(node, format!("background-image:{url}"));
    }
}

/// `url("…")` for a URL the resource pass produced; `None` for anything that would
/// need escaping.
fn css_url(url: &str) -> Option<String> {
    (!url.is_empty() && !url.contains(['"', '\\', '\n', '\r', '<']))
        .then(|| format!("url(\"{url}\")"))
}

/// Lays a cell with a block `display` (anything but `none` and `table-cell`) out the
/// way a browser does: the cell's content moves into a new `<div>` that takes its
/// style, class, id and presentational attributes (as declarations before the style,
/// which wins over them); the `<td>` keeps only its spans and `valign`.
fn unwrap_block_cell(dom: &mut Dom, node: NodeId) {
    let Some(style) = dom.attr(node, "style") else {
        return;
    };
    let block = style.split(';').any(|declaration| {
        let Some((property, value)) = declaration.split_once(':') else {
            return false;
        };
        let value = value.trim().trim_end_matches("!important").trim();
        property.trim().eq_ignore_ascii_case("display")
            && !value.eq_ignore_ascii_case("none")
            && !value.eq_ignore_ascii_case("table-cell")
    });
    if !block {
        return;
    }
    let mut declarations = Vec::new();
    for (attribute, property) in [("width", "width"), ("height", "height")] {
        if let Some(length) = dom.attr(node, attribute).and_then(length) {
            declarations.push(format!("{property}:{length}"));
        }
    }
    if let Some(color) = dom.attr(node, "bgcolor").and_then(color) {
        declarations.push(format!("background-color:{color}"));
    }
    if let Some(url) = dom.attr(node, "background").and_then(css_url) {
        declarations.push(format!("background-image:{url}"));
    }
    if let Some(align) = dom.attr(node, "align").map(str::to_ascii_lowercase)
        && matches!(align.as_str(), "left" | "right" | "center" | "justify")
    {
        declarations.push(format!("text-align:{align}"));
    }
    if dom.attr(node, "nowrap").is_some() {
        declarations.push("white-space:nowrap".to_owned());
    }
    declarations.push(style.to_owned());
    let style = declarations.join(";");
    let moved: Vec<(&str, String)> = ["class", "id"]
        .into_iter()
        .filter_map(|name| Some((name, dom.attr(node, name)?.to_owned())))
        .collect();

    let block = dom.wrap_children(node, "div");
    dom.set_attr(block, "style", &style);
    for (name, value) in moved {
        dom.set_attr(block, name, &value);
    }
    for name in [
        "style",
        "class",
        "id",
        "width",
        "height",
        "bgcolor",
        "background",
        "align",
        "nowrap",
    ] {
        dom.remove_attr(node, name);
    }
}

/// A `width`/`height` attribute as CSS: whole pixels or a percentage.
fn length(value: &str) -> Option<String> {
    let number = pixels(value)?;
    Some(
        if value
            .trim()
            .trim_start_matches(char::is_numeric)
            .starts_with('%')
        {
            format!("{number}%")
        } else {
            format!("{number}px")
        },
    )
}

/// Most columns a table can have, as in the HTML spec (`colspan` up to 1000).
const MAX_SPAN: u32 = 1000;

/// Fits the `colspan`s of `table` to its grid: a span ends before a slot an earlier
/// `rowspan` takes and covers only columns a cell starts in. Then gives empty columns
/// `width: 0` when another auto-width column has content (see the module docs).
/// Tables with `<col>`/`<colgroup>` or `table-layout: fixed` keep theirs: there a
/// column without content can have a width in a browser too.
fn fit_columns(dom: &mut Dom, table: NodeId, hints: &mut Hints) {
    let has_columns = !element_children(dom, table, &["col", "colgroup"]).is_empty();
    let fixed = dom.attr(table, "style").is_some_and(|style| {
        super::tracking::parse_style(style)
            .iter()
            .any(|(property, value)| property == "table-layout" && value == "fixed")
    });
    if has_columns || fixed {
        return;
    }
    // Place the cells on the grid, row group by row group (a `rowspan` ends with its
    // group) and remember where each one starts.
    let mut placed: Vec<(NodeId, u32, u32, u32)> = Vec::new();
    let mut starts: BTreeSet<u32> = BTreeSet::new();
    for group in row_groups(dom, table) {
        let mut taken: HashSet<(usize, u32)> = HashSet::new();
        let rows_in_group = group.len();
        for (row_index, row) in group.into_iter().enumerate() {
            let mut column = 0;
            for cell in element_children(dom, row, &["td", "th"]) {
                while taken.contains(&(row_index, column)) {
                    column += 1;
                }
                let wanted = span(dom.attr(cell, "colspan"));
                let mut span = 1;
                while span < wanted && !taken.contains(&(row_index, column + span)) {
                    span += 1;
                }
                // `rowspan=0` reaches the end of the group.
                let rows = match dom.attr(cell, "rowspan").and_then(pixels) {
                    Some(0) => rows_in_group - row_index,
                    Some(rows) => usize::try_from(rows.min(MAX_SPAN)).unwrap_or(1),
                    None => 1,
                };
                for taken_row in row_index..(row_index + rows).min(rows_in_group) {
                    for taken_column in column..column.saturating_add(span) {
                        taken.insert((taken_row, taken_column));
                    }
                }
                placed.push((cell, column, wanted, span));
                starts.insert(column);
                column = column.saturating_add(span);
            }
        }
    }
    // Per column that some cell starts in: whether it has content and whether its
    // width is automatic.
    let mut columns: HashMap<u32, (bool, bool)> = HashMap::new();
    for &(cell, start, wanted, span) in &placed {
        let reached = starts.range(start..start.saturating_add(span)).count();
        let reached = u32::try_from(reached).unwrap_or(span);
        if reached < wanted {
            dom.set_attr(cell, "colspan", &reached.to_string());
        }
        let column = columns.entry(start).or_insert((false, true));
        column.0 |= reached > 1 || !is_empty(dom, cell);
        column.1 &= reached == 1 && !has_width(dom, cell);
    }
    let auto_with_content = columns.values().any(|&(content, auto)| content && auto);
    if !auto_with_content {
        return;
    }
    for &(cell, start, _, _) in &placed {
        if columns.get(&start) == Some(&(false, true)) {
            hints.add(cell, "width:0".to_owned());
        }
    }
}

/// A cell with nothing to show: no elements, only (also non-breaking) white space.
fn is_empty(dom: &Dom, cell: NodeId) -> bool {
    dom.children(cell).iter().all(|&child| {
        dom.text(child)
            .is_some_and(|text| text.chars().all(|c| c.is_whitespace() || c == '\u{a0}'))
    })
}

/// Whether a cell sets its width (attribute or inline style).
fn has_width(dom: &Dom, cell: NodeId) -> bool {
    dom.attr(cell, "width").is_some()
        || dom.attr(cell, "style").is_some_and(|style| {
            super::tracking::parse_style(style)
                .iter()
                .any(|(property, _)| property == "width" || property == "min-width")
        })
}

/// `colspan` as the HTML spec reads it: 1 to 1000, 1 when missing or zero.
fn span(value: Option<&str>) -> u32 {
    value
        .and_then(pixels)
        .filter(|&span| span > 0)
        .map_or(1, |span| span.min(MAX_SPAN))
}

/// The rows of `table` per row group, in order (rows directly in the table form a
/// group of their own).
fn row_groups(dom: &Dom, table: NodeId) -> Vec<Vec<NodeId>> {
    let mut groups = Vec::new();
    let mut loose = Vec::new();
    for child in element_children(dom, table, &["thead", "tbody", "tfoot", "tr"]) {
        if dom.tag(child) == Some("tr") {
            loose.push(child);
        } else {
            if !loose.is_empty() {
                groups.push(std::mem::take(&mut loose));
            }
            groups.push(element_children(dom, child, &["tr"]));
        }
    }
    if !loose.is_empty() {
        groups.push(loose);
    }
    groups
}

/// The cells of `table` itself, not those of tables nested in them.
fn own_cells(dom: &Dom, table: NodeId) -> Vec<NodeId> {
    let mut cells = Vec::new();
    for child in element_children(dom, table, &["tbody", "thead", "tfoot", "tr"]) {
        let rows = if dom.tag(child) == Some("tr") {
            vec![child]
        } else {
            element_children(dom, child, &["tr"])
        };
        for row in rows {
            cells.extend(element_children(dom, row, &["td", "th"]));
        }
    }
    cells
}

fn element_children(dom: &Dom, node: NodeId, tags: &[&str]) -> Vec<NodeId> {
    dom.children(node)
        .iter()
        .copied()
        .filter(|&child| dom.tag(child).is_some_and(|tag| tags.contains(&tag)))
        .collect()
}

/// The HTML spec's rules for parsing non-negative integers: white space, an optional
/// `+` or `-`, digits, anything after them ignored (`"4px"` is 4, `"-0"` and `"+0"` are
/// 0, `"-1"` and `"px"` are errors). Capped, so a huge value can't overflow.
fn pixels(value: &str) -> Option<u32> {
    let value = value.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (negative, value) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    let digits: String = value
        .chars()
        .take_while(char::is_ascii_digit)
        .take(6)
        .collect();
    let number: u32 = digits.parse().ok()?;
    (!negative || number == 0).then_some(number)
}

/// A CSS color from a `color` attribute: `#rgb`, `#rrggbb` (also without `#`), a
/// color name or `rgb(…)`/`rgba(…)` with numbers only.
fn color(value: &str) -> Option<String> {
    let value = value.trim();
    let hex = value.strip_prefix('#').unwrap_or(value);
    if matches!(hex.len(), 3 | 6 | 8) && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(format!("#{hex}"));
    }
    if !value.is_empty() && value.len() <= 30 && value.chars().all(|c| c.is_ascii_alphabetic()) {
        return Some(value.to_ascii_lowercase());
    }
    let lower = value.to_ascii_lowercase();
    let arguments = lower
        .strip_prefix("rgba(")
        .or_else(|| lower.strip_prefix("rgb("))?
        .strip_suffix(')')?;
    arguments
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, ',' | '.' | '%' | ' '))
        .then_some(lower)
}

/// `face="Arial, Helvetica, sans-serif"` as a `font-family` list: names quoted,
/// generic families as keywords, anything but letters, digits, spaces, `-` and `_`
/// dropped.
fn font_families(value: &str) -> Option<String> {
    const GENERIC: &[&str] = &[
        "serif",
        "sans-serif",
        "monospace",
        "cursive",
        "fantasy",
        "system-ui",
    ];
    let families: Vec<String> = value
        .split(',')
        .filter_map(|family| {
            let name: String = family
                .chars()
                .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
                .collect();
            let name = name.trim();
            if name.is_empty() {
                None
            } else if GENERIC.contains(&name.to_ascii_lowercase().as_str()) {
                Some(name.to_ascii_lowercase())
            } else {
                Some(format!("\"{name}\""))
            }
        })
        .collect();
    (!families.is_empty()).then(|| families.join(","))
}

/// `<font size>`: 1 to 7, or relative to 3 with `+`/`-`, as the sizes browsers use for
/// the keywords `x-small` to `xxx-large` with a 16 px default.
fn font_size(value: &str) -> Option<&'static str> {
    const SIZES: [&str; 7] = ["10px", "13px", "16px", "18px", "24px", "32px", "48px"];
    let value = value.trim();
    let (sign, digits) = match value.as_bytes().first()? {
        b'+' => (1, &value[1..]),
        b'-' => (-1, &value[1..]),
        _ => (0, value),
    };
    let number = i64::from(pixels(digits)?);
    let size = if sign == 0 { number } else { 3 + sign * number };
    let index = usize::try_from(size.clamp(1, 7) - 1).ok()?;
    SIZES.get(index).copied()
}

/// `valign`/`align` values that map to `vertical-align`.
fn vertical_align(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "top" | "texttop" => Some("top"),
        "middle" | "center" | "absmiddle" => Some("middle"),
        "bottom" | "absbottom" => Some("bottom"),
        "baseline" => Some("baseline"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    /// The style sheet and the rewritten fragment.
    fn run(html: &str) -> (String, String) {
        let mut dom = Dom::parse_fragment(html);
        let root = dom.root();
        let sheet = apply(&mut dom, root).unwrap_or_default();
        (sheet, dom.serialize_children(root))
    }

    #[test]
    fn font_attributes_become_css() {
        let (sheet, html) =
            run(r#"<font face="Arial, 'Helvetica', sans-serif" size="+2" color="ff0000">x</font>"#);
        assert_eq!(
            sheet,
            ":where([data-maze-hint=\"0\"]){color:#ff0000;font-family:\"Arial\",\"Helvetica\",sans-serif;font-size:24px}\n"
        );
        assert!(html.contains(r#"<font face="Arial, 'Helvetica', sans-serif" size="+2" color="ff0000" data-maze-hint="0">"#), "{html}");
    }

    #[test]
    fn table_attributes_reach_its_own_cells_only() {
        let (sheet, html) = run(
            r#"<table border="2" cellpadding="8" cellspacing="4" align="center"><tr><td>a</td><td><table><tr><td>inner</td></tr></table></td></tr></table>"#,
        );
        assert_eq!(
            sheet,
            ":where([data-maze-hint=\"0\"]){border:2px outset grey;border-spacing:4px;margin-left:auto;margin-right:auto}\n\
             :where([data-maze-hint=\"1\"]){border:1px inset grey;padding:8px}\n"
        );
        // Two outer cells share a rule; the nested table's cell gets nothing.
        assert_eq!(html.matches(r#"data-maze-hint="1""#).count(), 2, "{html}");
        assert!(html.contains("<td>inner</td>"), "{html}");
    }

    #[test]
    fn border_values_as_the_spec_parses_them() {
        // Zero however it's spelled: no border (WPT table-border-2).
        for value in ["0", "0foo", "0%", "+0", "+0foo", "-0", " -0%"] {
            let (sheet, _) = run(&format!(
                "<table border=\"{value}\"><tr><td>a</td></tr></table>"
            ));
            assert_eq!(sheet, "", "border={value}");
        }
        // Not a number at all, or negative: the default, 1 px.
        for value in ["", "foo", "-1"] {
            let (sheet, _) = run(&format!(
                "<table border=\"{value}\"><tr><td>a</td></tr></table>"
            ));
            assert!(
                sheet.contains("border:1px outset grey"),
                "border={value}: {sheet}"
            );
        }
    }

    #[test]
    fn border_zero_and_cellpadding_zero() {
        let (sheet, _) = run(r#"<table border="0" cellpadding="0"><tr><td>a</td></tr></table>"#);
        assert_eq!(sheet, ":where([data-maze-hint=\"0\"]){padding:0px}\n");
    }

    #[test]
    fn valign_from_rows_unless_the_cell_has_its_own() {
        let (sheet, html) =
            run(r#"<table><tr valign="top"><td>a</td><td valign="bottom">b</td></tr></table>"#);
        assert_eq!(
            sheet,
            ":where([data-maze-hint=\"0\"]){vertical-align:top}\n\
             :where([data-maze-hint=\"1\"]){vertical-align:bottom}\n"
        );
        assert!(html.contains(r#"<td data-maze-hint="0">a</td>"#), "{html}");
    }

    #[test]
    fn image_align_floats_or_aligns() {
        let (sheet, _) =
            run(r#"<img src="a.png" align="left"><img src="b.png" align="absmiddle">"#);
        assert_eq!(
            sheet,
            ":where([data-maze-hint=\"0\"]){float:left}\n:where([data-maze-hint=\"1\"]){vertical-align:middle}\n"
        );
    }

    #[test]
    fn background_and_nowrap_on_cells() {
        let (sheet, _) = run(
            r#"<table><tr><td background="https://message.mazemail.localhost/m1/remote/0" nowrap>a</td></tr></table>"#,
        );
        assert_eq!(
            sheet,
            ":where([data-maze-hint=\"0\"]){white-space:nowrap;background-image:url(\"https://message.mazemail.localhost/m1/remote/0\")}\n"
        );
    }

    #[test]
    fn block_cells_become_a_cell_with_a_block() {
        let (_, html) = run(concat!(
            r#"<table><tr><td></td><td class="container" id="c" width="580" bgcolor="fff" "#,
            r#"style="display: block !important; margin: 0 auto" valign="top">a</td>"#,
            r#"<td style="display:none">b</td></tr></table>"#,
        ));
        assert!(
            html.contains(concat!(
                r#"<td valign="top" data-maze-hint="1"><div style="width:580px;background-color:#fff;"#,
                r#"display: block !important; margin: 0 auto" class="container" id="c">a</div></td>"#,
            )),
            "{html}"
        );
        assert!(
            html.contains(r#"<td style="display:none">b</td>"#),
            "{html}"
        );
    }

    #[test]
    fn empty_columns_next_to_content_get_no_width() {
        // GitHub's mail: an empty spacer, then the (now auto-width) cell with content.
        let (sheet, html) = run(
            r#"<table width="100%"><tr><td> </td><td style="display:block; width:580px">a</td></tr></table>"#,
        );
        assert_eq!(sheet, ":where([data-maze-hint=\"0\"]){width:0}\n");
        assert!(html.contains(r#"<td data-maze-hint="0"> </td>"#), "{html}");
        // Spacers around a fixed-width column stay: Blitz centers it like a browser.
        let (sheet, _) = run(
            "<table width=\"100%\"><tr><td>&nbsp;</td><td width=\"600\">a</td><td>&nbsp;</td></tr></table>",
        );
        assert_eq!(sheet, "");
    }

    #[test]
    fn hostile_values_cant_end_the_rule() {
        let (sheet, _) = run(
            r#"<font color="red;}body{display:none" face="x}{;&quot;" size="9999999999"><table background="x&quot;);}"><tr><td>a</td></tr></table></font>"#,
        );
        assert_eq!(
            sheet,
            ":where([data-maze-hint=\"0\"]){font-family:\"x\";font-size:48px}\n"
        );
    }

    #[test]
    fn colspans_shrink_to_columns_with_cells() {
        // Google Flights' price bar segments: two cells per row, then a row that
        // claims five columns.
        let (_, html) = run(concat!(
            "<table><tr><td>head</td><td>tail</td></tr>",
            "<tr><td colspan=5>filler</td></tr></table>",
        ));
        assert!(html.contains(r#"<td colspan="2">filler</td>"#), "{html}");
    }

    #[test]
    fn colspans_end_before_a_rowspan() {
        // The segment with the price marker: the marker spans all three rows, so the
        // last row's filler can only take the first column.
        let (_, html) = run(concat!(
            "<table><tr><td>f</td><td rowspan=3>ball</td><td>f</td></tr>",
            "<tr><td>head</td><td>tail</td></tr>",
            "<tr><td colspan=5>filler</td></tr></table>",
        ));
        assert!(html.contains(r#"<td colspan="1">filler</td>"#), "{html}");
        // Rows after the `rowspan` ends are free again.
        let (_, html) = run(concat!(
            "<table><tr><td>a</td><td rowspan=2>b</td></tr><tr><td>c</td></tr>",
            "<tr><td colspan=2>d</td></tr></table>",
        ));
        assert!(html.contains(r#"<td colspan="2">d</td>"#), "{html}");
    }

    #[test]
    fn colspans_that_reach_cells_stay() {
        let html = concat!(
            "<table><tr><td>a</td><td colspan=3>b</td><td>c</td></tr>",
            "<tr><td>1</td><td>2</td><td>3</td><td>4</td><td>5</td></tr></table>",
        );
        let (_, out) = run(html);
        assert!(out.contains(r#"<td colspan="3">b</td>"#), "{out}");
    }

    #[test]
    fn colspans_stay_with_columns_or_fixed_layout() {
        let (_, html) = run(
            "<table><colgroup><col width=10><col width=90></colgroup><tr><td>a</td></tr><tr><td colspan=2>b</td></tr></table>",
        );
        assert!(html.contains(r#"<td colspan="2">b</td>"#), "{html}");
        let (_, html) = run(
            r#"<table style="table-layout: fixed"><tr><td>a</td></tr><tr><td colspan=2>b</td></tr></table>"#,
        );
        assert!(html.contains(r#"<td colspan="2">b</td>"#), "{html}");
    }

    #[test]
    fn nothing_to_do_without_attributes() {
        let mut dom = Dom::parse_fragment("<p style=\"color:red\">plain</p>");
        let root = dom.root();
        assert_eq!(apply(&mut dom, root), None);
    }
}
