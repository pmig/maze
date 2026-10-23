//! The agent column (PLAN §4.12): once the core is open, an [`AgentManager`] over the
//! core ([`CoreAgentHost`]) and an [`AgentPanel`] in the main window's agent column.
//!
//! - The panel follows the window's selection (the selected thread is attached to new
//!   conversations), the core's settings reloads and its proposal changes.
//! - Session tokens left over from a previous run are revoked first.
//! - Agent processes run on the core's runtime; quitting stops the runtime, which kills
//!   their process groups.

use std::sync::Arc;

use agent::{AgentManager, AgentPaths, ManagerConfig, ReqwestHttp};
use agent_ui::{AgentPanel, MailContext};
use gpui::{App, AppContext as _, Entity};
use mail_ui::MailWindow;
use mazemail::agent_host::CoreAgentHost;
use mazemail_core::CoreHandle;
use mazemail_core::api::TokenAuthorizer;
use settings::Paths;
use tokio::sync::broadcast;

use super::AppState;

/// Registers the app-level agent actions ("Agent: Toggle Agent Panel").
pub(crate) fn init(cx: &mut App) {
    cx.on_action(|_: &agent_ui::ToggleAgentPanel, cx| {
        // Usually dispatched from the palette while the window handles that event;
        // GPUI can't update a window from inside its own event handling.
        cx.defer(|cx| {
            let Some((handle, _)) = cx.global::<AppState>().window.clone() else {
                return;
            };
            let toggled = handle.update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(mail_ui::actions::ToggleAgentColumn), cx);
            });
            if let Err(error) = toggled {
                tracing::debug!(%error, "no window for the agent column toggle");
            }
        });
    });
}

/// Creates the agent panel in the main window. Deferred: callers may be in the middle
/// of a window update.
pub(crate) fn start(core: &CoreHandle, authorizer: &TokenAuthorizer, cx: &mut App) {
    let core = core.clone();
    let authorizer = authorizer.clone();
    cx.defer(move |cx| {
        if let Err(error) = open_panel(&core, &authorizer, cx) {
            tracing::error!(%error, "the agent column couldn't start");
        }
    });
}

fn open_panel(core: &CoreHandle, authorizer: &TokenAuthorizer, cx: &mut App) -> anyhow::Result<()> {
    let Some((handle, window)) = cx.global::<AppState>().window.clone() else {
        return Ok(());
    };
    let host = Arc::new(CoreAgentHost::new(core.clone(), authorizer));
    let tokens = agent::AgentHost::token_store(host.as_ref());
    core.runtime().spawn(async move {
        agent::tokens::revoke_stale_session_tokens(tokens.as_ref()).await;
    });
    let manager = Arc::new(AgentManager::new(ManagerConfig {
        paths: AgentPaths::new(core.data_dir()),
        home: std::env::home_dir(),
        cli_program: std::env::current_exe().ok(),
        socket: Paths::current()
            .ok()
            .map(|paths| paths.socket_path().path().to_owned()),
        host,
        http: Arc::new(ReqwestHttp::new()?),
    }));
    let settings = core.settings().settings.clone();
    let panel = handle.update(cx, |_, window, cx| {
        cx.new(|cx| AgentPanel::new(manager, settings, window, cx))
    })?;
    window.update(cx, |window, cx| {
        window.set_agent_panel(panel.clone().into(), cx)
    });
    follow_selection(&window, &panel, cx);
    follow_settings(core, &panel, cx);
    follow_proposals(core.events(), &panel, cx);
    Ok(())
}

fn context_of(window: &Entity<MailWindow>, cx: &App) -> Option<MailContext> {
    window
        .read(cx)
        .agent_context(cx)
        .map(|(thread, subject)| MailContext { thread, subject })
}

fn follow_selection(window: &Entity<MailWindow>, panel: &Entity<AgentPanel>, cx: &mut App) {
    let context = context_of(window, cx);
    panel.update(cx, |panel, cx| panel.set_context(context, cx));
    let panel = panel.downgrade();
    cx.observe(window, move |window, cx| {
        let context = context_of(&window, cx);
        panel
            .update(cx, |panel, cx| panel.set_context(context, cx))
            .ok();
    })
    .detach();
}

fn follow_settings(core: &CoreHandle, panel: &Entity<AgentPanel>, cx: &mut App) {
    let mut settings = core.settings_watch();
    let panel = panel.downgrade();
    cx.spawn(async move |cx| {
        while settings.changed().await.is_ok() {
            let current = settings.borrow().settings.clone();
            let alive = panel
                .update(cx, |panel, cx| panel.set_settings(current, cx))
                .is_ok();
            if !alive {
                break;
            }
        }
    })
    .detach();
}

fn follow_proposals(
    mut events: broadcast::Receiver<mazemail_core::CoreEventBatch>,
    panel: &Entity<AgentPanel>,
    cx: &mut App,
) {
    let panel = panel.downgrade();
    cx.spawn(async move |cx| {
        loop {
            let refresh = match events.recv().await {
                Ok(batch) => batch.proposals,
                Err(broadcast::error::RecvError::Lagged(_)) => true,
                Err(broadcast::error::RecvError::Closed) => break,
            };
            if refresh {
                let alive = panel
                    .update(cx, |panel, cx| panel.refresh_proposals(cx))
                    .is_ok();
                if !alive {
                    break;
                }
            }
        }
    })
    .detach();
}
