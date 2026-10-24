//! Signing in from the agent column: the sign-in card (terminal logins, the agent's
//! own methods, links it wants opened, an API key or token), and what it does.

use std::path::PathBuf;

use agent::{AgentThreadInfo, LoginScript, SignInProgress, SignInRequest};
use gpui::ClipboardItem;
use gpui_kit::component::input::Input;

use super::*;

/// Sign-in state of the running session.
#[derive(Debug, Default)]
pub(super) struct SignInState {
    /// What the agent asked for.
    pub(super) request: Option<SignInRequest>,
    /// The method whose `authenticate` is running.
    pub(super) pending: Option<String>,
    /// The terminal login that was opened.
    pub(super) terminal: Option<OpenedLogin>,
    /// The last error.
    pub(super) error: Option<SharedString>,
    /// Links the agent opened (`true`: Maze opened it) or printed.
    pub(super) links: Vec<(String, bool)>,
    /// A key is being saved.
    pub(super) saving_key: bool,
}

/// A terminal login that is open in Terminal.
#[derive(Debug, Clone)]
pub(super) struct OpenedLogin {
    pub(super) method: String,
    pub(super) display: String,
    pub(super) sandboxed: bool,
    pub(super) path: PathBuf,
}

impl AgentPanel {
    /// Session events about signing in.
    pub(super) fn on_sign_in_event(&mut self, event: SessionEvent, cx: &mut Context<'_, Self>) {
        let launcher = Arc::clone(&self.launcher);
        let Some(session) = self.session.as_mut() else {
            return;
        };
        match event {
            SessionEvent::SignInRequired(request) => {
                session.sign_in.request = Some(request);
                session.sign_in.pending = None;
            }
            SessionEvent::SignIn(SignInProgress::Started(method)) => {
                session.sign_in.pending = Some(method);
                session.sign_in.error = None;
            }
            SessionEvent::SignIn(SignInProgress::Failed(error)) => {
                session.sign_in.pending = None;
                session.sign_in.error = Some(error.into());
            }
            SessionEvent::SignIn(SignInProgress::Succeeded) => {
                session.sign_in.pending = None;
                session.sign_in.error = None;
                self.accounts.insert(session.agent_id.clone());
            }
            SessionEvent::Link { url, open } => {
                if session.sign_in.links.iter().any(|(known, _)| *known == url) {
                    return;
                }
                // Links are only opened during a sign-in; others wait for a click.
                let open = open && session.status == SessionStatus::SignInRequired;
                if open {
                    launcher.open_url(&url, cx);
                }
                session.sign_in.links.push((url, open));
            }
            _ => {}
        }
        cx.notify();
    }

    /// Asks the agent to sign in with one of its own methods.
    fn authenticate(&mut self, method: String, cx: &mut Context<'_, Self>) {
        if let Some(session) = self.session.as_mut() {
            session.sign_in.error = None;
            session.sign_in.pending = Some(method.clone());
            session.handle.authenticate(method);
        }
        cx.notify();
    }

    /// Writes the terminal login script and opens it in Terminal.
    fn open_terminal_login(&mut self, method: &AuthMethodInfo, cx: &mut Context<'_, Self>) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let agent_id = session.agent_id.clone();
        match self
            .manager
            .terminal_login(&agent_id, method, &self.settings)
        {
            Ok(login) => {
                let LoginScript {
                    path,
                    display,
                    sandboxed,
                } = login;
                session.sign_in.error = match self.launcher.open_terminal(&path) {
                    Ok(()) => None,
                    Err(error) => Some(
                        format!(
                            "Terminal couldn't be opened ({error}). Run {} in a terminal.",
                            path.display()
                        )
                        .into(),
                    ),
                };
                session.sign_in.terminal = Some(OpenedLogin {
                    method: method.name.clone(),
                    display,
                    sandboxed,
                    path,
                });
                self.pending_account = Some(agent_id);
            }
            Err(error) => {
                session.sign_in.error = Some(format!("The login can't start: {error}").into());
            }
        }
        cx.notify();
    }

    /// Starts the agent again for the same conversation (after a terminal login or a
    /// new key), with the failed prompt back in the input.
    pub(super) fn retry_session(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(session) = self.session.take() else {
            return;
        };
        if self.prompt.read(cx).value().trim().is_empty()
            && let Some(text) = session
                .transcript
                .entries
                .iter()
                .rev()
                .find_map(|e| match e {
                    Entry::User { text } => Some(text.clone()),
                    _ => None,
                })
        {
            self.prompt
                .update(cx, |prompt, cx| prompt.set_value(text, window, cx));
        }
        let info = AgentThreadInfo {
            key: session.handle.agent_thread(),
            agent_id: session.agent_id.clone(),
            title: None,
            mail_thread: self.context.as_ref().map(|c| c.thread),
            acp_session_id: None,
            updated: 0,
        };
        session.handle.shutdown();
        self.start(session.agent_id, Some(info), cx);
    }

    /// Saves the pasted key (encrypted store) and starts the agent again with it.
    pub(super) fn save_key(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let key = self.key_input.read(cx).value().to_string();
        let provider = session
            .sign_in
            .request
            .as_ref()
            .and_then(|r| r.methods.iter().find_map(|m| m.api_key_provider.clone()));
        let agent_id = session.agent_id.clone();
        session.sign_in.saving_key = true;
        session.sign_in.error = None;
        self.key_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        let manager = Arc::clone(&self.manager);
        let task = Tokio::spawn(cx, async move {
            manager.save_key(&agent_id, provider.as_deref(), &key).await
        });
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                if let Some(session) = this.session.as_mut() {
                    session.sign_in.saving_key = false;
                }
                match result {
                    Ok(Ok(env)) => {
                        if let Some(session) = this.session.as_ref() {
                            this.keys.insert(session.agent_id.clone(), env);
                        }
                        this.retry_session(window, cx);
                    }
                    Ok(Err(error)) => {
                        if let Some(session) = this.session.as_mut() {
                            session.sign_in.error =
                                Some(format!("The key wasn't saved: {error}").into());
                        }
                    }
                    Err(_) => {
                        if let Some(session) = this.session.as_mut() {
                            session.sign_in.error = Some("The key wasn't saved.".into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Deletes the saved key of `agent_id`.
    pub(super) fn remove_key(&mut self, agent_id: String, cx: &mut Context<'_, Self>) {
        let manager = Arc::clone(&self.manager);
        let id = agent_id.clone();
        let task = Tokio::spawn(cx, async move { manager.remove_key(&id).await });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(())) => {
                        this.keys.remove(&agent_id);
                        this.show_notice(
                            "Key removed. Conversations you start from now on run without it.",
                            false,
                            cx,
                        );
                    }
                    Ok(Err(error)) => {
                        this.show_notice(format!("The key wasn't removed: {error}"), true, cx)
                    }
                    Err(_) => this.show_notice("The key wasn't removed.", true, cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Loads which agents have a saved key.
    pub(super) fn load_keys(&mut self, cx: &mut Context<'_, Self>) {
        let manager = Arc::clone(&self.manager);
        let task = Tokio::spawn(cx, async move { manager.key_envs().await });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if let Ok(Ok(keys)) = result {
                    this.keys = keys;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// How the agent is signed in, when Maze knows.
    pub(super) fn sign_in_status(&self, agent_id: &str) -> Option<SharedString> {
        if let Some(env) = self.keys.get(agent_id) {
            return Some(match env.as_str() {
                "CLAUDE_CODE_OAUTH_TOKEN" => "Signed in with a Claude token".into(),
                _ => format!("Signed in with an API key ({env})").into(),
            });
        }
        self.accounts.contains(agent_id).then(|| {
            if agent_id == "claude-acp" {
                "Signed in with your Claude account".into()
            } else {
                "Signed in with your account".into()
            }
        })
    }

    /// The sign-in card.
    pub(super) fn sign_in_card(&self, session: &Session, cx: &mut Context<'_, Self>) -> AnyElement {
        let theme = cx.theme().colors;
        let state = &session.sign_in;
        let methods: Vec<AuthMethodInfo> = state
            .request
            .as_ref()
            .map(|r| r.methods.clone())
            .unwrap_or_default();
        let provider = methods.iter().find_map(|m| m.api_key_provider.clone());
        let key_auth = agent::auth::key_auth(&session.agent_id, provider.as_deref());
        let muted = |text: SharedString| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let status: SharedString = match &session.key_env {
            Some(env) => format!(
                "Not signed in: {} got your saved key ({env}) but didn't accept it. Paste \
                 another one or sign in another way.",
                session.name
            )
            .into(),
            None => "Not signed in.".into(),
        };
        let mut card = v_flex()
            .id("agent-sign-in")
            .debug_selector(|| "agent-sign-in".to_owned())
            .m_2()
            .p_3()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::Info).small().text_color(theme.primary))
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(SharedString::from(format!("Sign in to {}", session.name))),
                    ),
            )
            .child(
                div()
                    .debug_selector(|| "agent-sign-in-status".to_owned())
                    .text_xs()
                    .child(status),
            )
            .when_some(
                state.request.as_ref().and_then(|r| r.message.clone()),
                |this, message| {
                    this.child(
                        div()
                            .debug_selector(|| "agent-sign-in-message".to_owned())
                            .p_1p5()
                            .rounded_sm()
                            .bg(theme.muted)
                            .text_xs()
                            .child(message),
                    )
                },
            );

        // Terminal logins.
        let terminal: Vec<(usize, &AuthMethodInfo)> = methods
            .iter()
            .enumerate()
            .filter(|(_, m)| m.terminal.is_some())
            .collect();
        if !terminal.is_empty() {
            let mut row = h_flex().gap_1().flex_wrap();
            for (n, (ix, method)) in terminal.iter().enumerate() {
                let method = (*method).clone();
                let button = Button::new(("agent-sign-in-terminal", *ix))
                    .small()
                    .icon(Icon::new(IconName::SquareTerminal))
                    .label(SharedString::from(format!("Sign in… {}", method.name)))
                    .when_some(method.description.clone(), |this, d| this.tooltip(d))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.open_terminal_login(&method, cx);
                    }));
                let button = if n == 0 {
                    button.primary()
                } else {
                    button.outline()
                };
                row = row.child(
                    div()
                        .debug_selector(move || format!("agent-sign-in-terminal-{n}"))
                        .child(button),
                );
            }
            card = card.child(row).child(muted(
                "Opens Terminal and runs the agent's own login there, with the agent's home in \
                 Maze and in Maze's sandbox, so the login stays separate from the copy you use \
                 in a terminal. Links it shows open in your browser."
                    .into(),
            ));
        }
        if let Some(opened) = &state.terminal {
            card = card.child(
                v_flex()
                    .debug_selector(|| "agent-sign-in-terminal-open".to_owned())
                    .gap_1()
                    .p_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(theme.border)
                    .child(div().text_xs().child(SharedString::from(format!(
                        "{} is open in Terminal. Finish signing in there, then retry.",
                        opened.method
                    ))))
                    .child(
                        div()
                            .text_xs()
                            .font_family("monospace")
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(opened.display.clone())),
                    )
                    .when(!opened.sandboxed, |this| {
                        this.child(muted(
                            "The agent sandbox is off, so the login runs unsandboxed.".into(),
                        ))
                    })
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                div()
                                    .debug_selector(|| "agent-sign-in-retry".to_owned())
                                    .child(
                                        Button::new("agent-sign-in-retry")
                                            .primary()
                                            .small()
                                            .icon(Icon::new(IconName::RefreshCw))
                                            .label("Done, retry")
                                            .on_click(cx.listener(
                                                |this, _: &ClickEvent, window, cx| {
                                                    this.retry_session(window, cx);
                                                },
                                            )),
                                    ),
                            )
                            .child({
                                let path = opened.path.clone();
                                Button::new("agent-sign-in-reopen")
                                    .ghost()
                                    .small()
                                    .label("Open Terminal again")
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        if let Err(error) = this.launcher.open_terminal(&path)
                                            && let Some(session) = this.session.as_mut()
                                        {
                                            session.sign_in.error = Some(
                                                format!("Terminal couldn't be opened: {error}")
                                                    .into(),
                                            );
                                        }
                                        cx.notify();
                                    }))
                            }),
                    ),
            );
        }

        // The agent's own methods.
        let native: Vec<(usize, &AuthMethodInfo)> = methods
            .iter()
            .enumerate()
            .filter(|(_, m)| m.terminal.is_none())
            .collect();
        if !native.is_empty() {
            let mut row = h_flex().gap_1().flex_wrap();
            for (n, (ix, method)) in native.iter().enumerate() {
                let id = method.id.clone();
                let pending = state.pending.as_deref() == Some(method.id.as_str());
                let button = Button::new(("agent-sign-in-method", *ix))
                    .small()
                    .label(method.name.clone())
                    .when(pending, |this| this.icon(Icon::new(IconName::LoaderCircle)))
                    .when_some(method.description.clone(), |this, d| this.tooltip(d))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.authenticate(id.clone(), cx);
                    }));
                let button = if n == 0 && terminal.is_empty() {
                    button.primary()
                } else {
                    button.outline()
                };
                row = row.child(
                    div()
                        .debug_selector(move || format!("agent-sign-in-method-{n}"))
                        .child(button),
                );
            }
            card = card.child(row);
        }
        if let Some(pending) = &state.pending {
            let name = methods
                .iter()
                .find(|m| &m.id == pending)
                .map_or_else(|| pending.clone(), |m| m.name.clone());
            card = card.child(
                h_flex()
                    .debug_selector(|| "agent-sign-in-pending".to_owned())
                    .gap_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(IconName::LoaderCircle).xsmall())
                    .child(SharedString::from(format!(
                        "Signing in with {name}… finish in your browser if it opened."
                    ))),
            );
        }

        // Links.
        for (ix, (url, opened)) in state.links.iter().enumerate() {
            let open_url = url.clone();
            let copy_url = url.clone();
            card = card.child(
                v_flex()
                    .debug_selector(move || format!("agent-sign-in-link-{ix}"))
                    .gap_0p5()
                    .child(muted(if *opened {
                        "Opened in your browser:".into()
                    } else {
                        "Sign-in link:".into()
                    }))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(theme.link)
                                    .child(SharedString::from(url.clone())),
                            )
                            .child(
                                Button::new(("agent-link-open", ix))
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(IconName::ExternalLink))
                                    .tooltip("Open in your browser")
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.launcher.open_url(&open_url, cx);
                                    })),
                            )
                            .child(
                                Button::new(("agent-link-copy", ix))
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(IconName::Copy))
                                    .tooltip("Copy link")
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy_url.clone(),
                                        ));
                                    }),
                            ),
                    ),
            );
        }

        // An API key or token.
        if let Some(key_auth) = key_auth {
            card = card
                .child(div().h(px(1.)).bg(theme.border))
                .child(muted(key_auth.help.into()))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .debug_selector(|| "agent-key-input".to_owned())
                                .child(Input::new(&self.key_input).small().mask_toggle()),
                        )
                        .child(
                            div().debug_selector(|| "agent-key-save".to_owned()).child(
                                Button::new("agent-key-save")
                                    .small()
                                    .outline()
                                    .icon(Icon::new(IconName::KeyRound))
                                    .label("Save key and retry")
                                    .disabled(state.saving_key)
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.save_key(window, cx);
                                    })),
                            ),
                        ),
                )
                .child(muted(
                    "Maze keeps the key encrypted in its database and passes it only to this \
                     agent."
                        .into(),
                ));
            if self.keys.contains_key(&session.agent_id) {
                let agent_id = session.agent_id.clone();
                card = card.child(
                    div()
                        .debug_selector(|| "agent-key-remove".to_owned())
                        .child(
                            Button::new("agent-key-remove")
                                .ghost()
                                .xsmall()
                                .label("Remove saved key")
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.remove_key(agent_id.clone(), cx);
                                })),
                        ),
                );
            }
        }
        if methods.is_empty() && key_auth.is_none() {
            card = card.child(
                h_flex().gap_1().child(muted(
                    "The agent offers no way to sign in from Maze. Sign it in as its \
                     documentation says, then retry."
                        .into(),
                )),
            );
        }
        card = card.child(
            h_flex().justify_end().child(
                Button::new("agent-sign-in-restart")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::RefreshCw))
                    .label("Retry")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.retry_session(window, cx);
                    })),
            ),
        );
        if let Some(error) = &state.error {
            card = card.child(
                h_flex()
                    .debug_selector(|| "agent-sign-in-error".to_owned())
                    .gap_1()
                    .items_start()
                    .child(
                        Icon::new(IconName::TriangleAlert)
                            .xsmall()
                            .text_color(theme.danger),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(theme.danger)
                            .child(error.clone()),
                    ),
            );
        }
        card.into_any_element()
    }
}
