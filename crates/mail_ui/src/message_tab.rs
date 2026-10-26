//! A message in a tab of its own (a double-click on a row, `enter` or `cmd-o`): only
//! the message view, full width, showing HTML mail as designed by default
//! (`reader.tab_mode`, ADR-020). The tab is titled with the subject; the layout file
//! keeps the message id only. When the message is gone (deleted, or moved away by
//! another device) the tab closes.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Window, div,
};
use mazemail_types::{Flags, MessageId, ThreadId};
use serde::{Deserialize, Serialize};
use time::UtcOffset;
use ui::assets::IconName;
use ui::panes::{Item, ItemEvent, SerializedItem};

use crate::actions::{Archive, Delete, Forward, Reply, ReplyAll, ToggleRead, ToggleStar};
use crate::message_view::{MessageView, ViewPlace};
use crate::source::{MailSource, SourceError};

/// The layout kind of message tabs (`{"kind": "message", "data": {"message": 17}}`).
pub const MESSAGE_KIND: &str = "message";

/// What a message tab keeps in the layout file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedMessage {
    /// The message.
    pub message: MessageId,
}

/// The subject a message without one shows.
const NO_SUBJECT: &str = "(no subject)";

/// One message, full width.
pub struct MessageTab {
    source: Arc<dyn MailSource>,
    message: MessageId,
    view: Entity<MessageView>,
    subject: Option<SharedString>,
}

impl std::fmt::Debug for MessageTab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageTab")
            .field("message", &self.message)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<ItemEvent> for MessageTab {}

impl MessageTab {
    /// A tab for `message`.
    pub fn new(
        source: Arc<dyn MailSource>,
        local_offset: UtcOffset,
        message: MessageId,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let view =
            cx.new(|cx| MessageView::new(Some(source.clone()), local_offset, ViewPlace::Tab, cx));
        let menu_source = source.clone();
        view.update(cx, |view, _| {
            view.set_context_menu(Rc::new(move |menu, _, _| {
                let flags = menu_source
                    .message_rows(&[message])
                    .first()
                    .map(|row| row.flags)
                    .unwrap_or_default();
                menu.menu("Reply", Box::new(Reply))
                    .menu("Reply All", Box::new(ReplyAll))
                    .menu("Forward", Box::new(Forward))
                    .separator()
                    .menu(
                        if flags.contains(Flags::SEEN) {
                            "Mark as Unread"
                        } else {
                            "Mark as Read"
                        },
                        Box::new(ToggleRead),
                    )
                    .menu(
                        if flags.contains(Flags::FLAGGED) {
                            "Unstar"
                        } else {
                            "Star"
                        },
                        Box::new(ToggleStar),
                    )
                    .menu("Archive", Box::new(Archive))
                    .menu("Delete", Box::new(Delete))
            }));
        });
        let mut this = Self {
            source,
            message,
            view,
            subject: None,
        };
        this.reload(cx);
        this
    }

    /// Whether `message` can be shown: `false` only once the mail is open and the
    /// message is gone.
    pub fn exists(source: &dyn MailSource, message: MessageId) -> bool {
        source.placeholder().is_some()
            || !matches!(
                source.message_document(message),
                Err(SourceError::MessageNotFound(_) | SourceError::ThreadNotFound(_))
            )
    }

    /// The message.
    pub fn message(&self) -> MessageId {
        self.message
    }

    /// The message view.
    pub fn view(&self) -> &Entity<MessageView> {
        &self.view
    }

    /// Reads the message again; closes the tab when it is gone.
    pub fn reload(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(placeholder) = self.source.placeholder() {
            // The mail is still opening (or failed): wait.
            self.view
                .update(cx, |view, cx| view.show_error(placeholder, cx));
            return;
        }
        match self.source.message_document(self.message) {
            Ok(document) => {
                let subject = document.subject.expose().trim().to_owned();
                self.subject = Some(if subject.is_empty() {
                    NO_SUBJECT.into()
                } else {
                    subject.into()
                });
                self.view
                    .update(cx, |view, cx| view.show(Some(document), cx));
                cx.emit(ItemEvent::UpdateTab);
            }
            Err(SourceError::MessageNotFound(_) | SourceError::ThreadNotFound(_)) => {
                cx.emit(ItemEvent::Close);
            }
            Err(error) => {
                self.view
                    .update(cx, |view, cx| view.show_error(error.to_string(), cx));
            }
        }
    }

    /// The thread the agent column attaches: this message's.
    pub fn agent_context(&self, cx: &App) -> Option<(ThreadId, SharedString)> {
        self.view
            .read(cx)
            .document()
            .map(|doc| (doc.thread, doc.subject.expose().clone()))
    }

    fn flags(&self) -> Flags {
        self.source
            .message_rows(&[self.message])
            .first()
            .map(|row| row.flags)
            .unwrap_or_default()
    }

    /// `m`: marks the message read or unread.
    pub fn toggle_read(&mut self) -> Result<(), SourceError> {
        if self.flags().contains(Flags::SEEN) {
            self.source
                .set_message_flags(&[self.message], Flags::empty(), Flags::SEEN)
        } else {
            self.source
                .set_message_flags(&[self.message], Flags::SEEN, Flags::empty())
        }
    }

    /// `s`: stars or unstars the message.
    pub fn toggle_star(&mut self) -> Result<(), SourceError> {
        if self.flags().contains(Flags::FLAGGED) {
            self.source
                .set_message_flags(&[self.message], Flags::empty(), Flags::FLAGGED)
        } else {
            self.source
                .set_message_flags(&[self.message], Flags::FLAGGED, Flags::empty())
        }
    }

    /// `a`: archives the message and closes the tab.
    pub fn archive(&mut self, cx: &mut Context<'_, Self>) -> Result<(), SourceError> {
        self.source.archive_messages(&[self.message])?;
        cx.emit(ItemEvent::Close);
        Ok(())
    }

    /// Moves the message to Trash (or deletes it for good when it is in Trash) and
    /// closes the tab.
    pub fn delete(&mut self, cx: &mut Context<'_, Self>) -> Result<(), SourceError> {
        self.source.delete_messages(&[self.message])?;
        cx.emit(ItemEvent::Close);
        Ok(())
    }
}

impl Focusable for MessageTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }
}

impl Render for MessageTab {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        div().size_full().child(self.view.clone())
    }
}

impl Item for MessageTab {
    fn tab_title(&self, _: &App) -> SharedString {
        self.subject.clone().unwrap_or_else(|| "Message".into())
    }

    fn tab_icon(&self, _: &App) -> Option<IconName> {
        Some(IconName::Mail)
    }

    fn tab_tooltip(&self, _: &App) -> Option<SharedString> {
        self.subject.clone()
    }

    fn serialize(&self, _: &App) -> Option<SerializedItem> {
        let data = serde_json::to_value(SerializedMessage {
            message: self.message,
        })
        .ok()?;
        Some(SerializedItem::new(MESSAGE_KIND, data))
    }

    fn clone_on_split(&self, _: &mut Window, cx: &mut Context<'_, Self>) -> Option<Entity<Self>> {
        let (source, message) = (self.source.clone(), self.message);
        let offset = self.view.read(cx).offset();
        Some(cx.new(|cx| Self::new(source, offset, message, cx)))
    }
}
