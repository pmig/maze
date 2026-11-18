//! The Save Search and Edit Saved Search dialog (ADR-030): a name and a query. Saving
//! goes through [`MailSource::save_search`] or [`MailSource::edit_saved_search`]; a
//! name that is taken or a query that doesn't parse shows the reason in the dialog,
//! which stays open.

use std::sync::Arc;

use gpui::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement, Render,
    SharedString, Styled, Window, div, prelude::FluentBuilder as _,
};
use gpui_kit::component::{
    ActiveTheme as _,
    input::{Input, InputState},
    v_flex,
};
use mazemail_types::SavedSearchId;

use crate::source::MailSource;

/// The content of the dialog.
pub struct SavedSearchDialog {
    source: Arc<dyn MailSource>,
    /// The saved search being edited; `None` saves a new one.
    editing: Option<SavedSearchId>,
    pub(crate) name: Entity<InputState>,
    pub(crate) query: Entity<InputState>,
    error: Option<SharedString>,
}

impl std::fmt::Debug for SavedSearchDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SavedSearchDialog")
            .field("editing", &self.editing)
            .finish_non_exhaustive()
    }
}

impl SavedSearchDialog {
    /// A dialog for `name` and `query`; with `editing`, Save changes that saved search.
    pub fn new(
        source: Arc<dyn MailSource>,
        editing: Option<SavedSearchId>,
        name: &str,
        query: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let (name, query) = (name.to_owned(), query.to_owned());
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Name, e.g. Invoices")
                .default_value(name)
        });
        let query = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("from:anna has:attachment")
                .default_value(query)
        });
        Self {
            source,
            editing,
            name,
            query,
            error: None,
        }
    }

    /// The saved search being edited, if any.
    pub fn editing(&self) -> Option<SavedSearchId> {
        self.editing
    }

    /// Saves the search, or shows why not. Returns its id once it is saved, so the
    /// dialog may close.
    pub fn submit(&mut self, cx: &mut Context<'_, Self>) -> Option<SavedSearchId> {
        let name = self.name.read(cx).value().trim().to_owned();
        let query = self.query.read(cx).value().trim().to_owned();
        let result = match self.editing {
            Some(id) => self
                .source
                .edit_saved_search(id, &name, &query)
                .map(|()| id),
            None => self.source.save_search(&name, &query),
        };
        match result {
            Ok(id) => Some(id),
            Err(error) => {
                self.error = Some(error.to_string().into());
                cx.notify();
                None
            }
        }
    }

    /// The error shown, if saving failed.
    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }
}

impl Render for SavedSearchDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let label = |text: &'static str| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        v_flex()
            .gap_2()
            .text_sm()
            .child(
                "A saved search shows in the folder pane under Saved Searches. Selecting it \
                 lists the mail of every account that matches the query, kept up to date.",
            )
            .child(label("Name"))
            .child(
                div()
                    .debug_selector(|| "saved-search-name".to_owned())
                    .child(Input::new(&self.name)),
            )
            .child(label("Query"))
            .child(
                div()
                    .debug_selector(|| "saved-search-query".to_owned())
                    .child(Input::new(&self.query)),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "saved-search-error".to_owned())
                        .text_color(theme.danger)
                        .child(error),
                )
            })
    }
}
