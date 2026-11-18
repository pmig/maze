//! Saved searches in the window (ADR-030): "Save Search…" from a search's results,
//! "Edit Saved Search…" and "Delete Saved Search…" from the folder pane's menu or the
//! palette; it also keeps the views of a saved search in step with its name and query.

use gpui::{
    AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, ParentElement as _,
    PromptLevel, SharedString, Styled as _, Window,
};
use gpui_kit::component::{
    Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    notification::Notification,
};
use mazemail_types::{FolderRole, PRODUCT_NAME, SavedSearchId};
use ui::keymap::shortcut_label;

use super::{MailWindow, folder_title};
use crate::mail_view::MailView;
use crate::saved_search::SavedSearchDialog;
use crate::source::{FolderKey, ThreadScope};

impl MailWindow {
    /// The saved search the active mail view shows, if it shows one.
    pub(super) fn current_saved_search(&self, cx: &gpui::App) -> Option<SavedSearchId> {
        match self.active_mail_view(cx)?.read(cx).scope(cx) {
            ThreadScope::Folder(FolderKey::SavedSearch(id)) => Some(id),
            _ => None,
        }
    }

    /// The Save Search or Edit Saved Search dialog, while it is open.
    pub fn saved_search_dialog(&self) -> Option<&Entity<SavedSearchDialog>> {
        self.saved_search_dialog.as_ref()
    }

    pub(super) fn no_saved_search_open(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        window.push_notification(
            Notification::info("Open a saved search first: select it in the folder pane.")
                .title(PRODUCT_NAME),
            cx,
        );
    }

    /// `search::SaveSearch`: the dialog for the search the active mail view shows (else
    /// the text in the search field). A saved search opens its Edit dialog instead.
    pub(super) fn save_search(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(id) = self.current_saved_search(cx) {
            self.edit_saved_search(id, window, cx);
            return;
        }
        let shown = self.active_mail_view(cx).and_then(|view| {
            view.read(cx)
                .thread_list()
                .read(cx)
                .search()
                .map(str::to_owned)
        });
        let typed = self.search_text(cx).trim().to_owned();
        let Some(query) = shown.or_else(|| (!typed.is_empty()).then_some(typed)) else {
            window.push_notification(
                Notification::info(format!(
                    "Search first ({}), then save the search.",
                    shortcut_label("cmd-shift-f", cx)
                ))
                .title(PRODUCT_NAME),
                cx,
            );
            return;
        };
        self.open_saved_search_dialog(None, "", &query, window, cx);
    }

    /// "Edit Saved Search…": the dialog with its name and query.
    pub(super) fn edit_saved_search(
        &mut self,
        id: SavedSearchId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let tree = self.source.folder_tree();
        let Some(saved) = tree.saved_search(id) else {
            return;
        };
        let (name, query) = (saved.name.to_string(), saved.query.expose().to_string());
        self.open_saved_search_dialog(Some(id), &name, &query, window, cx);
    }

    fn open_saved_search_dialog(
        &mut self,
        editing: Option<SavedSearchId>,
        name: &str,
        query: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let source = self.source.clone();
        let form = cx.new(|cx| SavedSearchDialog::new(source, editing, name, query, window, cx));
        self.saved_search_dialog = Some(form.clone());
        let focus = form.read(cx).name.focus_handle(cx);
        let this = cx.weak_entity();
        let (title, button) = match editing {
            Some(_) => ("Edit saved search", "Save"),
            None => ("Save search", "Save Search"),
        };
        window.open_dialog(cx, move |dialog, _, cx| {
            let submit = {
                let (form, this) = (form.clone(), this.clone());
                move |window: &mut Window, cx: &mut gpui::App| -> bool {
                    let Some(id) = form.update(cx, |form, cx| form.submit(cx)) else {
                        return false;
                    };
                    this.update(cx, |this, cx| this.saved_search_saved(id, window, cx))
                        .ok();
                    true
                }
            };
            let click = {
                let submit = submit.clone();
                move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut gpui::App| {
                    if submit(window, cx) {
                        window.close_dialog(cx);
                    }
                }
            };
            // Enter confirms: the dialog closes only once the search is saved.
            let confirm = move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut gpui::App| {
                submit(window, cx)
            };
            dialog
                .title(title)
                .on_ok(confirm)
                .width(ui::theme::scaled(480., cx))
                .child(form.clone())
                .footer(
                    h_flex().w_full().justify_end().gap_2().child(
                        Button::new("save-saved-search")
                            .debug_selector(|| "save-saved-search".to_owned())
                            .primary()
                            .small()
                            .label(button)
                            .on_click(click),
                    ),
                )
        });
        window.focus(&focus, cx);
    }

    /// Submits the open Save Search or Edit Saved Search dialog, as its button does;
    /// returns whether it was saved (and closed).
    pub fn submit_saved_search_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(form) = self.saved_search_dialog.clone() else {
            return false;
        };
        let Some(id) = form.update(cx, |form, cx| form.submit(cx)) else {
            return false;
        };
        window.close_dialog(cx);
        self.saved_search_saved(id, window, cx);
        true
    }

    /// A saved search was saved: the folder pane shows it, the views of it follow its
    /// name and query and the active view opens it.
    fn saved_search_saved(
        &mut self,
        id: SavedSearchId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.saved_search_dialog = None;
        self.folder_pane.update(cx, |pane, cx| pane.reload(cx));
        self.refresh_saved_search_views(cx);
        let key = FolderKey::SavedSearch(id);
        if self.current_saved_search(cx) != Some(id) {
            self.search
                .update(cx, |search, cx| search.set_value("", window, cx));
            let title = folder_title(self.folder_pane.read(cx), &key)
                .unwrap_or_else(|| SharedString::from("Saved Search"));
            let view = self.open_folder(ThreadScope::Folder(key), title, None, false, window, cx);
            MailView::focus_list(&view, window, cx);
        }
        self.sync_folder_selection(cx);
        cx.notify();
    }

    /// "Delete Saved Search…": asks, then deletes it. Views that showed it show the
    /// unified Inbox.
    pub(super) fn delete_saved_search(
        &mut self,
        id: SavedSearchId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let tree = self.source.folder_tree();
        let Some(saved) = tree.saved_search(id) else {
            return;
        };
        let question = format!("Delete the saved search “{}”?", saved.name);
        let answer = window.prompt(
            PromptLevel::Warning,
            &question,
            Some("Its mail stays where it is."),
            &["Cancel", "Delete"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(1) {
                this.update_in(cx, |this, window, cx| {
                    this.remove_saved_search(id, window, cx);
                })
                .ok();
            }
        })
        .detach();
    }

    /// Deletes a saved search without asking (after the question).
    pub fn remove_saved_search(
        &mut self,
        id: SavedSearchId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Err(error) = self.source.delete_saved_search(id) {
            tracing::warn!(%error, "deleting a saved search failed");
            window.push_notification(
                Notification::error(format!("The saved search couldn't be deleted: {error}"))
                    .title(PRODUCT_NAME),
                cx,
            );
            return;
        }
        self.folder_pane.update(cx, |pane, cx| pane.reload(cx));
        let inbox = FolderKey::Unified(FolderRole::Inbox);
        let title = folder_title(self.folder_pane.read(cx), &inbox)
            .unwrap_or_else(|| SharedString::from("Inbox"));
        let views = self.panes.read(cx).items_of::<MailView>(cx);
        for view in views {
            if view.read(cx).scope(cx) == ThreadScope::Folder(FolderKey::SavedSearch(id)) {
                view.update(cx, |view, cx| {
                    view.open(ThreadScope::Folder(inbox.clone()), title.clone(), None, cx);
                });
            }
        }
        self.sync_folder_selection(cx);
        cx.notify();
    }

    /// Gives every view of a saved search its current name and re-runs its query.
    pub(super) fn refresh_saved_search_views(&mut self, cx: &mut Context<'_, Self>) {
        let tree = self.source.folder_tree();
        let views = self.panes.read(cx).items_of::<MailView>(cx);
        for view in views {
            let ThreadScope::Folder(FolderKey::SavedSearch(id)) = view.read(cx).scope(cx) else {
                continue;
            };
            let Some(saved) = tree.saved_search(id) else {
                continue;
            };
            let title = saved.name.clone();
            view.update(cx, |view, cx| view.refresh_saved_search(title, cx));
        }
    }
}
