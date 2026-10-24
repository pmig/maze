//! Writing mail: opens drafts (crate `composer`) in tabs of the main window, over the
//! core (ADR-018).
//!
//! - **New message** (`cmd-n`, the title bar's "+", the palette, the Message menu): the
//!   main window asks with [`ComposeRequest::New`]; from another window (a draft in a
//!   window of its own) the app-level `mail::NewMessage` action does the same.
//! - **Reply, Reply All, Forward** (`cmd-r`, `cmd-shift-r`, `cmd-l`): the main window
//!   asks with a [`ComposeRequest`] for the message of its active tab; the core prefills
//!   the draft (recipients, subject, quote, identity) before the tab opens.
//! - Every new draft gets a random two-word name (`brave-otter`) no open tab has; it is
//!   stored with the draft and titles its tab.
//! - Draft tabs come back after a restart once the core is open ([`register_restore`]);
//!   a draft that was sent or deleted meanwhile is dropped.
//! - "Open in New Window" moves a draft from its tab to a window of its own.
//!
//! Requests arrive while a window is busy handling an event, so opening is deferred (see
//! `palette_actions_must_defer_window_updates` in `mail_ui`). [`CoreComposeBackend`]
//! is the composer's view of the core: identities, drafts and sending. When a message
//! only got queued (offline), the main window says so.

use std::sync::Arc;

use composer::{
    BoxFuture, COMPOSE_KIND, ComposeBackend, ComposeError, Composer, ComposerEvent,
    ComposerOptions, Draft, IdentityChoice, SendOutcome, SerializedDraft,
};
use gpui::{App, AppContext as _, Entity};
use mail_ui::ComposeRequest;
use mazemail_core::{CoreHandle, ReplyKind, SendResult};
use mazemail_types::{Address, DraftId, PRODUCT_NAME};
use ui::assets::IconName;
use ui::kit::component::{WindowExt as _, notification::Notification};
use ui::panes::{ItemKind, Restore, SerializedItem};

use super::AppState;

/// The composer's view of the core.
#[derive(Debug, Clone)]
pub(crate) struct CoreComposeBackend {
    core: CoreHandle,
}

impl CoreComposeBackend {
    pub(crate) fn new(core: CoreHandle) -> Self {
        Self { core }
    }
}

fn compose_error(error: &mazemail_core::Error) -> ComposeError {
    ComposeError::new(format!("{}.", error.to_string().trim_end_matches('.')))
}

impl ComposeBackend for CoreComposeBackend {
    fn identities(&self) -> Vec<IdentityChoice> {
        match self.core.identities() {
            Ok(identities) => identities
                .into_iter()
                .map(|i| IdentityChoice {
                    id: i.id,
                    name: i.display_name,
                    email: i.email,
                    account: i.account_name,
                })
                .collect(),
            Err(error) => {
                tracing::warn!(%error, "listing identities for the composer");
                Vec::new()
            }
        }
    }

    fn save_draft(
        &self,
        id: Option<DraftId>,
        draft: Draft,
    ) -> BoxFuture<Result<DraftId, ComposeError>> {
        let core = self.core.clone();
        Box::pin(async move {
            match id {
                Some(id) => core.update_draft(id, draft).await.map(|()| id),
                None => core.create_draft(draft).await,
            }
            .map_err(|error| compose_error(&error))
        })
    }

    fn delete_draft(&self, id: DraftId) -> BoxFuture<Result<(), ComposeError>> {
        let core = self.core.clone();
        Box::pin(async move {
            match core.delete_draft(id).await {
                Ok(()) | Err(mazemail_core::Error::NotFound { .. }) => Ok(()),
                Err(error) => Err(compose_error(&error)),
            }
        })
    }

    fn send(
        &self,
        id: Option<DraftId>,
        draft: Draft,
    ) -> BoxFuture<Result<SendOutcome, ComposeError>> {
        let core = self.core.clone();
        Box::pin(async move {
            match core.send_draft(id, draft).await {
                Ok(SendResult::Sent) => Ok(SendOutcome::Sent),
                Ok(SendResult::Queued { reason }) => Ok(SendOutcome::Queued { reason }),
                Err(error) => Err(compose_error(&error)),
            }
        })
    }

    fn complete_address(&self, prefix: &str) -> Vec<Address> {
        self.core.complete_addresses(prefix, 8).unwrap_or_default()
    }
}

/// The open core, or `None` (logged) while Maze is still opening.
fn core(cx: &App) -> Option<CoreHandle> {
    let core = cx
        .try_global::<AppState>()
        .and_then(|state| state.core.clone());
    if core.is_none() {
        tracing::info!("compose: the mail data isn't open yet");
    }
    core
}

/// Opens a draft tab for a blank message.
pub(crate) fn new_message(cx: &mut App) {
    let Some(core) = core(cx) else { return };
    match core.new_draft() {
        Ok(draft) => open(core, draft, cx),
        Err(error) => notify(format!("A new message can't be started: {error}"), cx),
    }
}

/// Opens a draft tab for what the main window asked: a new message, or a reply or
/// forward, prefilled by the core.
pub(crate) fn respond(request: ComposeRequest, cx: &mut App) {
    let Some(core) = core(cx) else { return };
    let prefill = {
        let core = core.clone();
        cx.background_spawn(async move {
            match request {
                ComposeRequest::New => core.new_draft(),
                ComposeRequest::Reply { message, all } => {
                    let kind = if all {
                        ReplyKind::All
                    } else {
                        ReplyKind::Sender
                    };
                    core.reply_draft(message, kind).await
                }
                ComposeRequest::Forward { message } => core.forward_draft(message).await,
            }
        })
    };
    cx.spawn(async move |cx| {
        let draft = prefill.await;
        cx.update(|cx| match draft {
            Ok(draft) => open(core, draft, cx),
            Err(error) => notify(format!("The message can't be answered: {error}"), cx),
        });
    })
    .detach();
}

fn backend(core: CoreHandle) -> Arc<dyn ComposeBackend> {
    Arc::new(CoreComposeBackend::new(core))
}

/// Opens `draft` in a tab of the main window's active pane, named with two words.
fn open(core: CoreHandle, mut draft: Draft, cx: &mut App) {
    let Some((handle, main)) = cx.global::<AppState>().window.clone() else {
        tracing::info!("compose: no main window");
        return;
    };
    let opened = handle.update(cx, |_, window, cx| {
        if draft.name.trim().is_empty() {
            draft.name = main.update(cx, |main, cx| main.new_draft_name(cx));
        }
        let options = ComposerOptions::in_tab(draft);
        let composer = cx.new(|cx| Composer::new(backend(core.clone()), options, window, cx));
        follow(&composer, core, cx);
        main.update(cx, |main, cx| main.add_item(Box::new(composer), window, cx));
    });
    if let Err(error) = opened {
        tracing::error!(%error, "cannot open a draft tab");
    }
}

/// Reacts to what a draft reports: sent (a notification), or moved to a window.
fn follow(composer: &Entity<Composer>, core: CoreHandle, cx: &mut App) {
    cx.subscribe(composer, move |_, event: &ComposerEvent, cx| match event {
        ComposerEvent::Sent(outcome) => {
            let message = match outcome {
                SendOutcome::Sent => "Message sent.".to_owned(),
                SendOutcome::Queued { reason } => format!(
                    "The message is in the outbox and goes out when the server can be \
                     reached ({reason})."
                ),
            };
            // The composer is closing itself; tell the main window afterwards.
            cx.defer(move |cx| notify(message, cx));
        }
        ComposerEvent::OpenInNewWindow(options) => {
            let options = (**options).clone();
            let core = core.clone();
            cx.defer(move |cx| open_window(core, options, cx));
        }
    })
    .detach();
}

/// A draft in a window of its own ("Open in New Window").
fn open_window(core: CoreHandle, options: ComposerOptions, cx: &mut App) {
    match composer::open_composer(backend(core.clone()), options, cx) {
        Ok((_, view)) => follow(&view, core, cx),
        Err(error) => tracing::error!(%error, "cannot open the compose window"),
    }
}

/// Lets the main window restore draft tabs (now that the core is open): a saved draft
/// comes back in its tab; one that was sent or deleted meanwhile is dropped.
pub(crate) fn register_restore(core: &CoreHandle, cx: &mut App) {
    let Some((handle, main)) = cx.global::<AppState>().window.clone() else {
        return;
    };
    let restore_core = core.clone();
    let kind = ItemKind::new(move |item: &SerializedItem, window, cx| {
        let Ok(SerializedDraft { draft: id }) =
            serde_json::from_value::<SerializedDraft>(item.data.clone())
        else {
            return Restore::Gone;
        };
        match restore_core.draft(id) {
            Ok(draft) => {
                let options = ComposerOptions {
                    draft_id: Some(id),
                    ..ComposerOptions::in_tab(draft)
                };
                let core = restore_core.clone();
                let composer =
                    cx.new(|cx| Composer::new(backend(core.clone()), options, window, cx));
                follow(&composer, core, cx);
                Restore::Item(Box::new(composer))
            }
            Err(mazemail_core::Error::NotFound { .. }) => Restore::Gone,
            Err(error) => {
                tracing::warn!(%error, "a draft tab couldn't be restored");
                Restore::Gone
            }
        }
    })
    .icon(IconName::SquarePen)
    .title(|item, _| item.title.clone().map(Into::into));
    let registered = handle.update(cx, |_, window, cx| {
        main.update(cx, |main, cx| {
            main.register_item_kind(COMPOSE_KIND, kind, window, cx);
        });
    });
    if let Err(error) = registered {
        tracing::debug!(%error, "no main window for draft tabs");
    }
}

/// Shows `message` as a notification in the main window.
fn notify(message: String, cx: &mut App) {
    let Some(window) = super::main_window(cx) else {
        tracing::info!("{message}");
        return;
    };
    let shown = window.update(cx, |_, window, cx| {
        window.push_notification(Notification::info(message).title(PRODUCT_NAME), cx);
    });
    if let Err(error) = shown {
        tracing::debug!(%error, "no main window for a compose notification");
    }
}
