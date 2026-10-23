//! Writing mail: opens compose windows (crate `composer`) over the core.
//!
//! - **New message** (`cmd-n`, the title bar's "+", the palette, the Message menu): the
//!   app-level `mail::NewMessage` action, with the first account's default identity.
//! - **Reply, Reply All, Forward** (`cmd-r`, `cmd-shift-r`, `cmd-l`): the main window
//!   asks with a [`ComposeRequest`] for the message it shows; the core prefills the
//!   draft (recipients, subject, quote, identity) before the window opens.
//!
//! Both arrive while a window is busy handling an event, so opening is deferred (see
//! `palette_actions_must_defer_window_updates` in `mail_ui`). [`CoreComposeBackend`]
//! is the composer's view of the core: identities, drafts and sending. When a message
//! only got queued (offline), the main window says so.

use std::sync::Arc;

use composer::{
    BoxFuture, ComposeBackend, ComposeError, ComposerEvent, ComposerOptions, Draft, IdentityChoice,
    SendOutcome,
};
use gpui::{App, AppContext as _};
use mail_ui::ComposeRequest;
use mazemail_core::{CoreHandle, ReplyKind, SendResult};
use mazemail_types::{Address, DraftId, PRODUCT_NAME};
use ui::kit::component::{WindowExt as _, notification::Notification};

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

/// Opens a compose window for a blank message.
pub(crate) fn new_message(cx: &mut App) {
    let Some(core) = core(cx) else { return };
    match core.new_draft() {
        Ok(draft) => open(core, draft, cx),
        Err(error) => notify(format!("A new message can't be started: {error}"), cx),
    }
}

/// Opens a compose window for a reply or forward, prefilled by the core.
pub(crate) fn respond(request: ComposeRequest, cx: &mut App) {
    let Some(core) = core(cx) else { return };
    let prefill = {
        let core = core.clone();
        cx.background_spawn(async move {
            match request {
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

fn open(core: CoreHandle, draft: Draft, cx: &mut App) {
    let backend: Arc<dyn ComposeBackend> = Arc::new(CoreComposeBackend::new(core));
    match composer::open_composer(backend, ComposerOptions::new(draft), cx) {
        Ok((_, view)) => {
            cx.subscribe(&view, |_, event: &ComposerEvent, cx| {
                let ComposerEvent::Sent(outcome) = event;
                let message = match outcome {
                    SendOutcome::Sent => "Message sent.".to_owned(),
                    SendOutcome::Queued { reason } => format!(
                        "The message is in the outbox and goes out when the server can be \
                         reached ({reason})."
                    ),
                };
                // The compose window is closing itself; tell the main window afterwards.
                cx.defer(move |cx| notify(message, cx));
            })
            .detach();
        }
        Err(error) => tracing::error!(%error, "cannot open the compose window"),
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
