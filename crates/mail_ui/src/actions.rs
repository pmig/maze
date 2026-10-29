//! Actions of the main window. Keys are bound in `assets/keymaps/default-macos.json`;
//! the command palette lists every action in [`PALETTE_NAMESPACES`].

use gpui::actions;

use crate::links::MailTo;

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
        /// Edit your settings (`settings.json`) in a tab (`cmd-,`).
        OpenSettings,
        /// Show Maze's default settings, read-only, in a tab.
        OpenDefaultSettings,
    ]
);

actions!(
    settings_editor,
    [
        /// Save the settings file and apply it (`cmd-s` in the settings tab).
        SaveSettings,
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
        /// Focus the search field that searches all messages (`cmd-shift-f`, `cmd-/`).
        FocusSearch,
        /// Focus the Quick Filter of the message list (`cmd-f`, `cmd-shift-k`).
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
        /// Archive the selected threads (`a`).
        Archive,
        /// Move the selected threads to Trash; in Trash, delete them for good after
        /// asking (`backspace`, `delete`).
        Delete,
        /// Move the selected threads to a folder picked by typing part of its name
        /// (`shift-m`, Quick Move).
        MoveToFolder,
        /// Undo the last archive, move, read or star change (`cmd-z`).
        Undo,
        /// Write a new message (`cmd-n`, the title bar's "+").
        NewMessage,
        /// Synchronize every account now (`cmd-shift-t` or `F5`, the title bar's sync
        /// button; Thunderbird's "Get All New Messages").
        SyncNow,
        /// Reply to the sender (`cmd-r`).
        Reply,
        /// Reply to all (`cmd-shift-r`).
        ReplyAll,
        /// Forward (`cmd-l`).
        Forward,
        /// Expand the selected thread, or move into it (`right`).
        ExpandThread,
        /// Collapse the selected thread, or go from a message to its thread (`left`).
        CollapseThread,
        /// Expand every thread (`*`).
        ExpandAllThreads,
        /// Collapse every thread (`\`).
        CollapseAllThreads,
        /// Select the whole thread of the selected message (`cmd-shift-a`).
        SelectThread,
        /// Extend the selection to the next row (`shift-down`).
        ExtendSelectionDown,
        /// Extend the selection to the previous row (`shift-up`).
        ExtendSelectionUp,
        /// Select every row of the list (`cmd-a`).
        SelectAll,
        /// Add the open folder to the favorites, or remove it.
        ToggleFavoriteFolder,
        /// Open the selected message in a tab of its own (`enter`, `cmd-o`, a
        /// double-click).
        OpenMessage,
        /// Open the selected folder in a new tab (the folder pane's menu).
        OpenFolderInNewTab,
        /// Pick a folder of any account by typing part of its name and open it
        /// (`cmd-k`).
        GoToFolder,
    ]
);

/// Write to the addresses of a `mailto:` link, clicked in a message. The window asks
/// the app for a draft ([`crate::ComposeRequest::MailTo`]). Not in the palette.
#[derive(Debug, Clone, PartialEq, Eq, gpui::Action)]
#[action(namespace = message_view, no_json)]
pub struct ComposeTo(pub MailTo);

actions!(
    view,
    [
        /// Switch the message list between cards and table (`cmd-alt-t`).
        ToggleTableView,
        /// Turn threads on or off: a thread tree, or one row per message (`cmd-alt-g`).
        ToggleThreads,
        /// Show an HTML message as the sender designed it (original) or in reader mode
        /// (`cmd-alt-o`).
        ToggleOriginalHtml,
        /// Sort the message list by date.
        SortByDate,
        /// Sort the message list by the time the server received the mail.
        SortByReceived,
        /// Sort the message list by star, starred first.
        SortByStar,
        /// Sort the message list by read state, unread first.
        SortByUnread,
        /// Sort the message list by attachment, mail with attachments first.
        SortByAttachment,
        /// Sort the message list by subject.
        SortBySubject,
        /// Sort the message list by correspondents.
        SortByCorrespondents,
        /// Sort the message list by sender.
        SortBySender,
        /// Sort the message list by recipients.
        SortByRecipients,
        /// Sort the message list by tags.
        SortByTags,
        /// Sort the message list by account.
        SortByAccount,
        /// Sort the message list by size, largest first.
        SortBySize,
        /// Reverse the order of the message list.
        ReverseSortOrder,
    ]
);

/// The sort action of every [`SortBy`](crate::source::SortBy), for menus and the
/// command palette.
pub fn sort_action(by: crate::source::SortBy) -> Box<dyn gpui::Action> {
    use crate::source::SortBy;
    match by {
        SortBy::Date => Box::new(SortByDate),
        SortBy::Received => Box::new(SortByReceived),
        SortBy::Star => Box::new(SortByStar),
        SortBy::Unread => Box::new(SortByUnread),
        SortBy::Attachment => Box::new(SortByAttachment),
        SortBy::Subject => Box::new(SortBySubject),
        SortBy::Correspondents => Box::new(SortByCorrespondents),
        SortBy::Sender => Box::new(SortBySender),
        SortBy::Recipients => Box::new(SortByRecipients),
        SortBy::Tags => Box::new(SortByTags),
        SortBy::Account => Box::new(SortByAccount),
        SortBy::Size => Box::new(SortBySize),
    }
}

/// Action namespaces the command palette lists.
pub const PALETTE_NAMESPACES: &[&str] = &[
    "maze",
    "workspace",
    "pane",
    "search",
    "mail",
    "view",
    "agent",
];

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
        assert_eq!(
            humanize_action_name("view::ToggleTableView"),
            "View: Toggle Table View"
        );
    }
}
