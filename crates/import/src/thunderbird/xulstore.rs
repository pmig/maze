//! `xulstore.json`: Thunderbird's persisted window state, read as a layout suggestion
//! (CONCEPT §17: offered, not forced).

use serde::Serialize;
use serde_json::Value;

/// The document key of the main window.
const MESSENGER: &str = "chrome://messenger/content/messenger.xhtml";

/// The parts of Thunderbird's layout Maze can offer to reproduce.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LayoutSuggestion {
    /// Folder pane modes (`all`, `unified`, `favorite`, `recent`, `smart`, `tags`).
    pub folder_pane_modes: Vec<String>,
    pub folder_pane_compact: bool,
    /// Width of the folder pane in pixels.
    pub folder_pane_width: Option<u32>,
    /// Message pane size in pixels (width in vertical layout, height otherwise).
    pub message_pane_width: Option<u32>,
    pub message_pane_height: Option<u32>,
    /// `mail.pane_config.dynamic`: 0 classic, 1 wide, 2 vertical.
    pub pane_layout: Option<i64>,
    /// `mail.threadpane.listview`: 0 cards, 1 table.
    pub thread_pane_list_view: Option<i64>,
}

fn attr<'a>(doc: &'a Value, element: &str, name: &str) -> Option<&'a str> {
    doc.get(element)?.get(name)?.as_str()
}

fn pixels(value: Option<&str>) -> Option<u32> {
    value?
        .trim()
        .trim_end_matches("px")
        .parse::<f64>()
        .ok()
        .map(|v| v.round() as u32)
}

/// Reads the suggestion from the text of `xulstore.json`; `None` if it isn't JSON or
/// has no main window entry.
pub fn parse(text: &str) -> Option<LayoutSuggestion> {
    let root: Value = serde_json::from_str(text).ok()?;
    let doc = root.get(MESSENGER)?;
    Some(LayoutSuggestion {
        folder_pane_modes: attr(doc, "folderTree", "mode")
            .map(|m| {
                m.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        folder_pane_compact: attr(doc, "folderTree", "compact") == Some("true"),
        folder_pane_width: pixels(attr(doc, "folderPaneSplitter", "width")),
        message_pane_width: pixels(attr(doc, "messagePaneSplitter", "width")),
        message_pane_height: pixels(attr(doc, "messagePaneSplitter", "height")),
        pane_layout: None,
        thread_pane_list_view: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_modes_and_sizes() {
        let s = parse(
            r#"{"chrome://messenger/content/messenger.xhtml":{
                "folderTree":{"mode":"all,favorite","compact":"true"},
                "folderPaneSplitter":{"width":"312"},
                "messagePaneSplitter":{"height":"420.5"}}}"#,
        )
        .unwrap();
        assert_eq!(s.folder_pane_modes, vec!["all", "favorite"]);
        assert!(s.folder_pane_compact);
        assert_eq!(s.folder_pane_width, Some(312));
        assert_eq!(s.message_pane_height, Some(421));
        assert!(parse("{}").is_none());
        assert!(parse("not json").is_none());
    }
}
