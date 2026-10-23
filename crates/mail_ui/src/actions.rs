//! Actions of the main window. Keys are bound in `assets/keymaps/default-macos.json`;
//! the command palette lists every action in [`PALETTE_NAMESPACES`].

use gpui::actions;

actions!(
    maze,
    [
        /// Quit Maze.
        Quit,
        /// Show the About panel.
        About,
        /// Hide Maze.
        Hide,
        /// Hide other applications.
        HideOthers,
        /// Show all applications.
        ShowAll,
        /// Minimize the window.
        Minimize,
        /// Zoom the window.
        Zoom,
        /// Add a mail account (IMAP, Gmail or Microsoft).
        AddAccount,
    ]
);

actions!(
    workspace,
    [
        /// Show or hide the folder pane (`cmd-b`).
        ToggleFolderPane,
        /// Expand or collapse the agent column (`cmd-?`).
        ToggleAgentColumn,
        /// Show or hide the message pane (`F8`).
        ToggleMessagePane,
    ]
);

actions!(
    command_palette,
    [
        /// Open or close the command palette (`cmd-shift-p`).
        Toggle,
    ]
);

actions!(
    search,
    [
        /// Focus the global search field (`cmd-k`).
        FocusSearch,
        /// Focus the Quick Filter of the message list (`cmd-shift-k`).
        QuickFilter,
    ]
);

actions!(
    mail,
    [
        /// Select the next message (`f`, down).
        NextMessage,
        /// Select the previous message (`b`, up).
        PreviousMessage,
        /// Select the next unread message (`n`).
        NextUnread,
        /// Select the previous unread message (`p`).
        PreviousUnread,
        /// Select the first message (home).
        FirstMessage,
        /// Select the last message (end).
        LastMessage,
        /// Move the selection down a page.
        PageDown,
        /// Move the selection up a page.
        PageUp,
        /// Toggle read/unread (`m`).
        ToggleRead,
        /// Toggle the star (`s`).
        ToggleStar,
        /// Archive the selected thread (`a`).
        Archive,
        /// Reply to the sender (`cmd-r`).
        Reply,
        /// Reply to all (`cmd-shift-r`).
        ReplyAll,
        /// Forward (`cmd-l`).
        Forward,
        /// Switch the message list between cards and table.
        ToggleTableView,
    ]
);

/// Action namespaces the command palette lists.
pub const PALETTE_NAMESPACES: &[&str] = &["maze", "workspace", "search", "mail"];

/// "mail::NextUnread" becomes "Mail: Next Unread", the way Zed's palette shows actions.
pub fn humanize_action_name(name: &str) -> String {
    let (namespace, action) = name.split_once("::").unwrap_or(("", name));
    let mut out = String::with_capacity(name.len() + 8);
    for (ix, word) in namespace.split('_').filter(|w| !w.is_empty()).enumerate() {
        if ix > 0 {
            out.push(' ');
        }
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    if !out.is_empty() {
        out.push_str(": ");
    }
    for (ix, c) in action.chars().enumerate() {
        if ix > 0 && c.is_uppercase() {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanizes_names() {
        assert_eq!(
            humanize_action_name("mail::NextUnread"),
            "Mail: Next Unread"
        );
        assert_eq!(
            humanize_action_name("command_palette::Toggle"),
            "Command Palette: Toggle"
        );
        assert_eq!(humanize_action_name("Quit"), "Quit");
    }
}
