//! [`AgentPanel`]: the agent column's view.

mod controls;
mod sign_in;
mod thread;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use agent::{
    AgentManager, AgentStatus, AgentThreadInfo, AttachedThread, AuthMethodInfo, Entry,
    InstallProgress, NoticeLevel, PermissionChoice, PermissionEntry, PlanStatus,
    ProposalItemStatus, ProposalView, SessionEvent, SessionStatus, StartRequest, StartedSession,
    TerminalEntry, TerminalStatus, ToolCallEntry, ToolStatus, Transcript,
};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{InputEvent, InputState, Textarea, TextareaState},
    menu::{DropdownMenu as _, PopupMenu, PopupMenuItem},
    progress::Progress,
    text::TextView,
    v_flex,
};
use mazemail_types::ThreadId;
use settings::Settings;
use ui::assets::IconName;
use ui::gpui_tokio::Tokio;

use self::sign_in::SignInState;
use crate::launcher::{Launcher, SystemLauncher};
use crate::{
    AcceptAllProposals, AcceptProposal, CycleMode, CycleModel, CycleThinking, FocusPrompt,
    NewThread, NextProposal, PreviousProposal, RejectAllProposals, RejectProposal, ShowHistory,
    StopAgent,
};

/// The mail thread selected in the window, attached to new conversations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailContext {
    /// The thread.
    pub thread: ThreadId,
    /// Its subject, for the context chip.
    pub subject: SharedString,
}

/// What the body shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    /// The picker, or the running conversation.
    Main,
    /// Earlier conversations about the selected thread.
    History,
}

#[derive(Debug, Clone, PartialEq)]
enum Install {
    Running {
        stage: &'static str,
        done: u64,
        total: Option<u64>,
    },
    Failed(SharedString),
}

struct Session {
    handle: agent::SessionHandle,
    agent_id: String,
    name: SharedString,
    is_pi: bool,
    status: SessionStatus,
    transcript: Transcript,
    sign_in: SignInState,
    /// Model, mode, thinking and other choices the agent offers.
    controls: Vec<agent::Control>,
    /// The variable of the user's saved key the agent was started with.
    key_env: Option<String>,
    sandboxed: bool,
    policy: Option<SharedString>,
    attached: Option<AttachedThread>,
    context_note: Option<SharedString>,
    expanded: HashSet<usize>,
    ended: Option<Option<SharedString>>,
    _events: Task<()>,
}

/// The agent column.
pub struct AgentPanel {
    manager: Arc<AgentManager>,
    settings: Settings,
    catalog: Vec<AgentStatus>,
    selected: String,
    show_all: bool,
    context: Option<MailContext>,
    session: Option<Session>,
    view: View,
    history: Vec<AgentThreadInfo>,
    proposals: Vec<ProposalView>,
    proposal_cursor: usize,
    installs: HashMap<String, Install>,
    /// Agents the background update moved to a new version: id → version.
    updated: HashMap<String, String>,
    notice: Option<(SharedString, bool)>,
    /// No account shares mail with agents (checked on settings changes, not per frame).
    no_shared_accounts: bool,
    starting: bool,
    /// Agent id → environment variable of its saved key.
    keys: BTreeMap<String, String>,
    /// Agents signed in with an account in this run.
    accounts: HashSet<String>,
    /// The agent whose terminal login was opened; signed in once it starts.
    pending_account: Option<String>,
    key_input: Entity<InputState>,
    launcher: Arc<dyn Launcher>,
    prompt: Entity<TextareaState>,
    focus: FocusHandle,
    review_focus: FocusHandle,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
    _tasks: Vec<Task<()>>,
}

impl std::fmt::Debug for AgentPanel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentPanel")
            .field("selected", &self.selected)
            .field("session", &self.session.as_ref().map(|s| &s.agent_id))
            .finish_non_exhaustive()
    }
}

impl AgentPanel {
    /// A panel over `manager`; loads the registry and the pending proposals.
    pub fn new(
        manager: Arc<AgentManager>,
        settings: Settings,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder("Ask the agent…  ⏎ send, ⇧⏎ new line")
        });
        let key_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("Paste a key or token")
        });
        let subscriptions = vec![
            cx.subscribe_in(&prompt, window, Self::on_prompt_event),
            cx.subscribe_in(
                &key_input,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        this.save_key(window, cx);
                    }
                },
            ),
        ];
        let selected = settings
            .agent
            .default
            .as_deref()
            .map(agent::catalog::resolve_alias)
            .unwrap_or("claude-acp")
            .to_owned();
        let catalog = manager.catalog(&settings, true);
        let mut this = Self {
            manager,
            settings,
            catalog,
            selected,
            show_all: false,
            context: None,
            session: None,
            view: View::Main,
            history: Vec::new(),
            proposals: Vec::new(),
            proposal_cursor: 0,
            installs: HashMap::new(),
            updated: HashMap::new(),
            notice: None,
            no_shared_accounts: false,
            starting: false,
            keys: BTreeMap::new(),
            accounts: HashSet::new(),
            pending_account: None,
            key_input,
            launcher: Arc::new(SystemLauncher),
            prompt,
            focus: cx.focus_handle(),
            review_focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
            _tasks: Vec::new(),
        };
        this.check_shared_accounts();
        if Tokio::is_initialized(cx) {
            this.load_registry(false, cx);
            this.refresh_proposals(cx);
            this.load_keys(cx);
        }
        this
    }

    /// Opens links and terminal logins through `launcher` (tests use a recorder).
    pub fn set_launcher(&mut self, launcher: Arc<dyn Launcher>) {
        self.launcher = launcher;
    }

    // ---- inputs from the window ---------------------------------------------------

    /// New settings (agent list, sandbox, default agent).
    pub fn set_settings(&mut self, settings: Settings, cx: &mut Context<'_, Self>) {
        self.settings = settings;
        self.check_shared_accounts();
        self.reload_catalog(cx);
    }

    fn check_shared_accounts(&mut self) {
        self.no_shared_accounts = self
            .manager
            .host()
            .account_policy()
            .agent_accounts()
            .is_empty();
    }

    /// The thread selected in the window. New conversations attach it.
    pub fn set_context(&mut self, context: Option<MailContext>, cx: &mut Context<'_, Self>) {
        if self.context == context {
            return;
        }
        self.context = context;
        self.load_history(cx);
        cx.notify();
    }

    /// Reloads the pending proposals (the core reported a change).
    pub fn refresh_proposals(&mut self, cx: &mut Context<'_, Self>) {
        let host = Arc::clone(self.manager.host());
        let task = Tokio::spawn(cx, async move { host.pending_proposals().await });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(Ok(proposals)) => {
                    this.proposals = proposals;
                    let pending = this.pending_items().len();
                    this.proposal_cursor = this.proposal_cursor.min(pending.saturating_sub(1));
                    cx.notify();
                }
                Ok(Err(error)) => {
                    this.show_notice(format!("Proposals can't be loaded: {error}"), true, cx)
                }
                Err(_) => {}
            })
            .ok();
        }));
    }

    // ---- state for tests and the window --------------------------------------------

    /// The running conversation's transcript.
    pub fn transcript(&self) -> Option<&Transcript> {
        self.session.as_ref().map(|s| &s.transcript)
    }

    /// The running session's state.
    pub fn session_status(&self) -> Option<SessionStatus> {
        self.session.as_ref().map(|s| s.status)
    }

    /// Pending proposals.
    pub fn proposals(&self) -> &[ProposalView] {
        &self.proposals
    }

    /// The agents in the picker.
    pub fn catalog(&self) -> &[AgentStatus] {
        &self.catalog
    }

    /// The agent the picker starts.
    pub fn selected_agent(&self) -> &str {
        &self.selected
    }

    /// Pending approvals and proposal items, for the collapsed strip's badge.
    pub fn attention_count(&self) -> usize {
        let approvals = self
            .session
            .as_ref()
            .map_or(0, |s| s.transcript.pending_permissions().count());
        approvals + self.pending_items().len()
    }

    /// The prompt input.
    pub fn prompt(&self) -> &Entity<TextareaState> {
        &self.prompt
    }

    #[cfg(test)]
    pub(crate) fn review_focus_for_tests(&self) -> FocusHandle {
        self.review_focus.clone()
    }

    #[cfg(test)]
    pub(crate) fn updated_for_tests(&self, id: &str) -> Option<String> {
        self.updated.get(id).cloned()
    }

    #[cfg(test)]
    pub(crate) fn start_for_tests(&mut self, agent_id: &str, cx: &mut Context<'_, Self>) {
        self.start(agent_id.to_owned(), None, cx);
    }

    #[cfg(test)]
    pub(crate) fn key_input_for_tests(&self) -> Entity<InputState> {
        self.key_input.clone()
    }

    // ---- sessions -----------------------------------------------------------------

    /// Shows a started session (from [`AgentManager::start`], or a test transport).
    pub fn attach_session(
        &mut self,
        started: StartedSession,
        name: impl Into<SharedString>,
        is_pi: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let events = started.handle.events();
        let task = cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                let alive = this
                    .update(cx, |this, cx| this.on_session_event(event, cx))
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });
        self.session = Some(Session {
            agent_id: started.handle.agent_id().to_owned(),
            handle: started.handle,
            name: name.into(),
            is_pi,
            status: SessionStatus::Starting,
            transcript: started.transcript,
            sign_in: SignInState::default(),
            controls: Vec::new(),
            key_env: started.key_env,
            sandboxed: started.sandboxed,
            policy: started.policy_description.map(Into::into),
            attached: started.attached,
            context_note: started.context_note.map(Into::into),
            expanded: HashSet::new(),
            ended: None,
            _events: task,
        });
        self.view = View::Main;
        self.starting = false;
        cx.notify();
    }

    fn start(
        &mut self,
        agent_id: String,
        resume: Option<AgentThreadInfo>,
        cx: &mut Context<'_, Self>,
    ) {
        if self.starting {
            return;
        }
        let Some(status) = self.status_of(&agent_id).cloned() else {
            self.show_notice(format!("There is no agent called {agent_id}."), true, cx);
            return;
        };
        self.selected.clone_from(&agent_id);
        self.starting = true;
        self.notice = None;
        let manager = Arc::clone(&self.manager);
        let request = StartRequest {
            agent_id,
            mail_thread: self.context.as_ref().map(|c| c.thread),
            resume,
            settings: self.settings.clone(),
        };
        let name: SharedString = status.entry.name.clone().into();
        let is_pi = status.entry.is_pi();
        let task = Tokio::spawn(cx, async move { manager.start(request).await });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.starting = false;
                match result {
                    Ok(Ok(started)) => this.attach_session(started, name, is_pi, cx),
                    Ok(Err(error)) => this.show_notice(error.to_string(), true, cx),
                    Err(_) => this.show_notice("Starting the agent failed.", true, cx),
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_session_event(&mut self, event: SessionEvent, cx: &mut Context<'_, Self>) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        match event {
            SessionEvent::Status(status) => {
                let previous = session.status;
                session.status = status;
                if status != SessionStatus::SignInRequired {
                    session.sign_in.request = None;
                    session.sign_in.pending = None;
                    session.sign_in.terminal = None;
                    session.sign_in.error = None;
                }
                if status == SessionStatus::Ready {
                    // After a terminal login, the first answered prompt proves it worked
                    // (Claude Code only checks its login when it gets a prompt).
                    if previous == SessionStatus::Working
                        && self.pending_account.as_deref() == Some(session.agent_id.as_str())
                    {
                        self.pending_account = None;
                        self.accounts.insert(session.agent_id.clone());
                    }
                    self.load_history(cx);
                }
            }
            SessionEvent::AgentInfo { .. } => {}
            SessionEvent::Controls(controls) => session.controls = controls,
            event @ (SessionEvent::SignInRequired(_)
            | SessionEvent::SignIn(_)
            | SessionEvent::Link { .. }) => {
                self.on_sign_in_event(event, cx);
                return;
            }
            SessionEvent::Update(update) => {
                session.transcript.apply(&update);
                self.scroll.scroll_to_bottom();
            }
            SessionEvent::Ended(error) => session.ended = Some(error.map(Into::into)),
        }
        cx.notify();
    }

    fn new_thread(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(session) = self.session.take() {
            session.handle.shutdown();
        }
        self.view = View::Main;
        self.notice = None;
        self.load_history(cx);
        cx.notify();
    }

    fn send_prompt(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let text = self.prompt.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if session.ended.is_some() || session.status == SessionStatus::Working {
            return;
        }
        session.handle.prompt(text);
        self.prompt
            .update(cx, |prompt, cx| prompt.set_value("", window, cx));
        cx.notify();
    }

    fn on_prompt_event(
        &mut self,
        _: &Entity<TextareaState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let InputEvent::PressEnter { shift: false, .. } = event {
            self.send_prompt(window, cx);
        }
    }

    /// Answers a permission prompt.
    pub fn respond(&mut self, id: u64, choice: PermissionChoice, cx: &mut Context<'_, Self>) {
        if let Some(session) = &self.session {
            session.handle.respond(id, choice);
        }
        cx.notify();
    }

    fn attach_current_thread(&mut self, cx: &mut Context<'_, Self>) {
        // Exports the thread (AI policy permitting), rewrites thread.md in the scratch
        // dir and tells the session, which announces it with the next prompt.
        if self.session.is_none() {
            return;
        }
        let Some(thread) = self.context.as_ref().map(|c| c.thread) else {
            return;
        };
        let host = Arc::clone(self.manager.host());
        let task = Tokio::spawn(cx, async move { host.thread_export(thread).await });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                let Some(session) = this.session.as_mut() else {
                    return;
                };
                match result {
                    Ok(Ok(Some(export))) => {
                        let dir = this.manager.paths().session_dir(session.handle.id());
                        match std::fs::write(
                            dir.join(agent::scratch::THREAD_FILE),
                            export.markdown.expose(),
                        ) {
                            Ok(()) => {
                                let attached = AttachedThread {
                                    public_id: export.public_id,
                                };
                                session.handle.set_attached(Some(attached.clone()));
                                session.attached = Some(attached);
                                session.context_note = None;
                            }
                            Err(error) => {
                                session.context_note = Some(
                                    format!("The thread couldn't be attached: {error}").into(),
                                );
                            }
                        }
                    }
                    Ok(Ok(None)) => {
                        session.context_note =
                            Some("This thread's account doesn't share mail with agents.".into());
                    }
                    _ => {
                        session.context_note = Some("The thread couldn't be attached.".into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- agents and install ---------------------------------------------------------

    fn status_of(&self, id: &str) -> Option<&AgentStatus> {
        let id = agent::catalog::resolve_alias(id);
        self.catalog.iter().find(|s| s.entry.id == id)
    }

    fn reload_catalog(&mut self, cx: &mut Context<'_, Self>) {
        self.catalog = self.manager.catalog(&self.settings, true);
        cx.notify();
    }

    fn load_registry(&mut self, refresh: bool, cx: &mut Context<'_, Self>) {
        let manager = Arc::clone(&self.manager);
        let task = Tokio::spawn(cx, async move { manager.load_registry(refresh).await });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if let Ok(Err(error)) = result {
                    tracing::warn!(%error, "the agent registry isn't available");
                    if refresh {
                        this.show_notice(error.to_string(), true, cx);
                    }
                }
                this.reload_catalog(cx);
            })
            .ok();
        }));
    }

    fn install(&mut self, id: String, cx: &mut Context<'_, Self>) {
        if matches!(self.installs.get(&id), Some(Install::Running { .. })) {
            return;
        }
        self.installs.insert(
            id.clone(),
            Install::Running {
                stage: "Starting",
                done: 0,
                total: None,
            },
        );
        let (progress_tx, progress_rx) = async_channel::unbounded::<InstallProgress>();
        let manager = Arc::clone(&self.manager);
        let settings = self.settings.clone();
        let install_id = id.clone();
        let task = Tokio::spawn(cx, async move {
            let report = move |progress: InstallProgress| {
                let _ = progress_tx.try_send(progress);
            };
            manager.install(&install_id, &settings, &report).await
        });
        let progress_id = id.clone();
        self._tasks.push(cx.spawn(async move |this, cx| {
            while let Ok(progress) = progress_rx.recv().await {
                let (stage, done, total) = match progress {
                    InstallProgress::Runtime { done, total } => ("Getting Node", done, total),
                    InstallProgress::Download { done, total } => ("Downloading", done, total),
                    InstallProgress::Installing => ("Installing", 0, None),
                };
                let id = progress_id.clone();
                let alive = this
                    .update(cx, |this, cx| {
                        this.installs
                            .insert(id, Install::Running { stage, done, total });
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        }));
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(_)) => {
                        this.installs.remove(&id);
                    }
                    Ok(Err(error)) => {
                        this.installs
                            .insert(id, Install::Failed(error.to_string().into()));
                    }
                    Err(_) => {
                        this.installs
                            .insert(id, Install::Failed("Installing failed.".into()));
                    }
                }
                this.reload_catalog(cx);
            })
            .ok();
        }));
        cx.notify();
    }

    /// Keeps installed agents up to date in the background (`agent.auto_update`): now,
    /// then every few hours. The registry is fetched at most once a day.
    pub fn start_auto_updates(&mut self, cx: &mut Context<'_, Self>) {
        const EVERY: std::time::Duration = std::time::Duration::from_secs(4 * 3600);
        self._tasks.push(cx.spawn(async move |this, cx| {
            loop {
                let Ok(task) = this.update(cx, |this, cx| {
                    let manager = Arc::clone(&this.manager);
                    let settings = this.settings.clone();
                    Tokio::spawn(cx, async move { manager.auto_update(&settings).await })
                }) else {
                    break;
                };
                let result = task.await;
                let alive = this
                    .update(cx, |this, cx| this.updates_done(result, cx))
                    .is_ok();
                if !alive {
                    break;
                }
                cx.background_executor().timer(EVERY).await;
            }
        }));
    }

    fn updates_done(
        &mut self,
        result: Result<
            Result<agent::UpdateReport, agent::registry::RegistryError>,
            ui::gpui_tokio::JoinError,
        >,
        cx: &mut Context<'_, Self>,
    ) {
        match result {
            Ok(Ok(report)) => {
                for (id, _, version) in report.updated {
                    self.installs.remove(&id);
                    self.updated.insert(id, version);
                }
                for (id, name, error) in report.failed {
                    if id == "node" {
                        self.show_notice(format!("{name} couldn't be updated: {error}"), true, cx);
                    } else {
                        self.installs.insert(
                            id,
                            Install::Failed(format!("Update failed: {error}").into()),
                        );
                    }
                }
            }
            Ok(Err(error)) => {
                tracing::debug!(%error, "no agent updates: the registry isn't available")
            }
            Err(_) => {}
        }
        self.reload_catalog(cx);
    }

    // ---- history --------------------------------------------------------------------

    fn load_history(&mut self, cx: &mut Context<'_, Self>) {
        if !Tokio::is_initialized(cx) {
            return;
        }
        let Some(thread) = self.context.as_ref().map(|c| c.thread) else {
            self.history.clear();
            return;
        };
        let host = Arc::clone(self.manager.host());
        let task = Tokio::spawn(cx, async move { host.agent_threads(Some(thread)).await });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if let Ok(Ok(history)) = result {
                    this.history = history;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn resume(&mut self, info: AgentThreadInfo, cx: &mut Context<'_, Self>) {
        if let Some(session) = self.session.take() {
            session.handle.shutdown();
        }
        self.start(info.agent_id.clone(), Some(info), cx);
    }

    // ---- proposals ------------------------------------------------------------------

    /// Pending items in display order: (proposal id, item id).
    fn pending_items(&self) -> Vec<(i64, i64)> {
        self.proposals
            .iter()
            .flat_map(|p| p.pending().map(move |item| (p.id, item.id)))
            .collect()
    }

    /// Accepts or rejects one item.
    pub fn decide_item(&mut self, item: i64, accept: bool, cx: &mut Context<'_, Self>) {
        let host = Arc::clone(self.manager.host());
        let task = Tokio::spawn(
            cx,
            async move { host.decide_proposal_item(item, accept).await },
        );
        self.after_decision(task, cx);
    }

    /// Accepts or rejects every pending item of every proposal.
    pub fn decide_all(&mut self, accept: bool, cx: &mut Context<'_, Self>) {
        let host = Arc::clone(self.manager.host());
        let ids: Vec<i64> = self.proposals.iter().map(|p| p.id).collect();
        let task = Tokio::spawn(cx, async move {
            for id in ids {
                host.decide_proposal(id, accept).await?;
            }
            Ok(())
        });
        self.after_decision(task, cx);
    }

    fn after_decision(
        &mut self,
        task: Task<Result<Result<(), agent::HostError>, ui::gpui_tokio::JoinError>>,
        cx: &mut Context<'_, Self>,
    ) {
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if let Ok(Err(error)) = result {
                    this.show_notice(format!("The change couldn't be applied: {error}"), true, cx);
                }
                this.refresh_proposals(cx);
            })
            .ok();
        }));
    }

    fn decide_cursor(&mut self, accept: bool, cx: &mut Context<'_, Self>) {
        if let Some((_, item)) = self.pending_items().get(self.proposal_cursor).copied() {
            self.decide_item(item, accept, cx);
        }
    }

    fn move_cursor(&mut self, delta: isize, cx: &mut Context<'_, Self>) {
        let len = self.pending_items().len();
        if len == 0 {
            return;
        }
        let next = self.proposal_cursor as isize + delta;
        self.proposal_cursor = next.clamp(0, len as isize - 1) as usize;
        cx.notify();
    }

    fn show_notice(
        &mut self,
        text: impl Into<SharedString>,
        error: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.notice = Some((text.into(), error));
        cx.notify();
    }

    // ---- rendering ------------------------------------------------------------------

    fn header(&self, cx: &mut Context<'_, Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().colors;
        let (title, placeholder): (SharedString, bool) = match (&self.session, self.view) {
            (_, View::History) => ("Conversations".into(), false),
            (Some(session), View::Main) => match session.transcript.title() {
                Some(title) => (title.into(), false),
                None => (format!("New {} Thread", session.name).into(), true),
            },
            (None, View::Main) => ("Agents".into(), true),
        };
        let sandbox_on = self.settings.agent_sandbox.enabled;
        let (lock_icon, lock_tip): (IconName, SharedString) = match &self.session {
            Some(session) if session.sandboxed => (
                IconName::ShieldCheck,
                session
                    .policy
                    .clone()
                    .map_or_else(|| "Sandboxed".into(), |p| format!("Sandboxed\n{p}").into()),
            ),
            Some(_) => (
                IconName::ShieldAlert,
                "This agent runs outside Maze's sandbox".into(),
            ),
            None if sandbox_on => (
                IconName::ShieldCheck,
                "Agents run in a sandbox: they can't read your home directory or the mail store, \
                 and reach mail only through the mazemail command line"
                    .into(),
            ),
            None => (IconName::ShieldAlert, "The agent sandbox is off".into()),
        };
        // "New thread" lists the agents that can start, like Zed's "+" menu.
        let ready: Vec<(String, String)> = self
            .catalog
            .iter()
            .filter(|s| s.is_ready())
            .map(|s| (s.entry.id.clone(), s.entry.name.clone()))
            .collect();
        let current = self.session.as_ref().map(|s| s.agent_id.clone());
        let panel = cx.entity().downgrade();
        h_flex()
            .flex_none()
            .h(px(32.))
            .pl_2()
            .pr_1()
            .gap_1()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.tab_bar)
            .child(
                Icon::new(IconName::Bot)
                    .small()
                    .text_color(theme.muted_foreground),
            )
            .child(
                div()
                    .debug_selector(|| "agent-title".to_owned())
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .when(placeholder, |this| this.text_color(theme.muted_foreground))
                    .when(!placeholder, |this| this.font_semibold())
                    .child(title),
            )
            .child(
                Button::new("agent-padlock")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(lock_icon))
                    .tooltip(lock_tip),
            )
            .child(
                Button::new("agent-history")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::Clock))
                    .selected(self.view == View::History)
                    .tooltip("Conversations about this thread")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.view = if this.view == View::History {
                            View::Main
                        } else {
                            View::History
                        };
                        this.load_history(cx);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .debug_selector(|| "agent-new-thread".to_owned())
                    .child(
                        Button::new("agent-new-thread")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::Plus))
                            .tooltip("New thread (⌘N)")
                            .dropdown_menu(move |menu: PopupMenu, _, _| {
                                let mut menu = menu.label("New Thread");
                                for (id, name) in &ready {
                                    let panel = panel.clone();
                                    let id = id.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(name.clone())
                                            .checked(current.as_ref() == Some(&id))
                                            .on_click(move |_, _, cx| {
                                                panel
                                                    .update(cx, |panel, cx| {
                                                        panel.new_thread(cx);
                                                        panel.start(id.clone(), None, cx);
                                                    })
                                                    .ok();
                                            }),
                                    );
                                }
                                let panel = panel.clone();
                                menu.separator()
                                    .item(PopupMenuItem::new("All Agents…").on_click(
                                        move |_, _, cx| {
                                            panel
                                                .update(cx, |panel, cx| {
                                                    panel.new_thread(cx);
                                                    panel.show_all = true;
                                                    cx.notify();
                                                })
                                                .ok();
                                        },
                                    ))
                            }),
                    ),
            )
            .child(
                Button::new("agent-collapse")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::PanelRightClose))
                    .tooltip("Collapse agent column (⌘?)")
                    .on_click(|_, window, cx| {
                        match cx.build_action("workspace::ToggleAgentColumn", None) {
                            Ok(action) => window.dispatch_action(action, cx),
                            Err(error) => tracing::warn!(%error, "no agent column toggle"),
                        }
                    }),
            )
    }

    fn banner(id: &'static str, text: SharedString, error: bool, cx: &App) -> AnyElement {
        let theme = cx.theme().colors;
        h_flex()
            .id(id)
            .mx_2()
            .mt_2()
            .p_2()
            .gap_2()
            .items_start()
            .rounded_md()
            .border_1()
            .border_color(if error { theme.danger } else { theme.border })
            .bg(theme.background)
            .child(
                Icon::new(if error {
                    IconName::TriangleAlert
                } else {
                    IconName::Info
                })
                .small()
                .text_color(if error {
                    theme.danger
                } else {
                    theme.muted_foreground
                }),
            )
            .child(div().flex_1().min_w_0().text_xs().child(text))
            .into_any_element()
    }

    fn banners(&self, cx: &mut Context<'_, Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if !self.settings.agent_sandbox.enabled {
            out.push(Self::banner(
                "agent-sandbox-off",
                "The agent sandbox is off (agent_sandbox.enabled): agents can read your whole \
                 home directory."
                    .into(),
                true,
                cx,
            ));
        }
        if let Some((text, error)) = &self.notice {
            out.push(Self::banner("agent-notice", text.clone(), *error, cx));
        }
        let pi = match &self.session {
            Some(session) => session.is_pi,
            None => self.selected == "pi-acp",
        };
        if pi {
            out.push(Self::banner(
                "agent-pi-notice",
                agent::PI_NOTICE.into(),
                false,
                cx,
            ));
        }
        if self.no_shared_accounts {
            out.push(Self::banner(
                "agent-ai-off",
                "No account shares mail with agents yet. Set an account to \"any\" under \
                 \"agent.accounts\" in settings; until then agents see no mail."
                    .into(),
                false,
                cx,
            ));
        }
        out
    }

    fn picker(&self, cx: &mut Context<'_, Self>) -> AnyElement {
        let theme = cx.theme().colors;
        let mut list = v_flex().gap_1();
        for (ix, status) in self.catalog.iter().enumerate() {
            if !self.show_all
                && !status.entry.built_in
                && status.installed.is_none()
                && !matches!(status.entry.source, agent::AgentSource::Custom(_))
            {
                continue;
            }
            list = list.child(self.agent_row(ix, status, cx));
        }
        let hidden = self
            .catalog
            .iter()
            .filter(|s| !s.entry.built_in && s.installed.is_none())
            .count();
        v_flex()
            .p_2()
            .gap_2()
            .child(div().text_xs().text_color(theme.muted_foreground).child(
                "Pick an agent. It runs in a sandbox and sees mail only through the \
                            mazemail command line; changes it proposes wait for your review.",
            ))
            .when_some(self.context.as_ref(), |this, context| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(SharedString::from(format!(
                            "Context: @thread {}",
                            context.subject
                        ))),
                )
            })
            .child(list)
            .when(hidden > 0 && !self.show_all, |this| {
                this.child(
                    Button::new("agent-show-all")
                        .ghost()
                        .xsmall()
                        .label(format!("More agents from the registry ({hidden})"))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.show_all = true;
                            cx.notify();
                        })),
                )
            })
            .into_any_element()
    }

    fn agent_row(&self, ix: usize, status: &AgentStatus, cx: &mut Context<'_, Self>) -> AnyElement {
        let theme = cx.theme().colors;
        let id = status.entry.id.clone();
        let selected = id == self.selected;
        let action: AnyElement = match self.installs.get(&id) {
            Some(Install::Running { stage, done, total }) => {
                let percent = total
                    .filter(|t| *t > 0)
                    .map(|t| (*done as f32 / t as f32 * 100.).min(100.));
                v_flex()
                    .w(px(90.))
                    .gap_0p5()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(*stage),
                    )
                    .child(
                        Progress::new(("agent-progress", ix))
                            .loading(percent.is_none())
                            .value(percent.unwrap_or(0.)),
                    )
                    .into_any_element()
            }
            _ if status.is_ready() => {
                let start_id = id.clone();
                h_flex()
                    .gap_1()
                    .when(status.update_available(), |this| {
                        let update_id = id.clone();
                        this.child(
                            Button::new(("agent-update", ix))
                                .ghost()
                                .xsmall()
                                .label("Update")
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.install(update_id.clone(), cx);
                                })),
                        )
                    })
                    .child(
                        Button::new(("agent-start", ix))
                            .primary()
                            .xsmall()
                            .label("Start")
                            .disabled(self.starting)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.start(start_id.clone(), None, cx);
                            })),
                    )
                    .into_any_element()
            }
            _ => {
                let install_id = id.clone();
                Button::new(("agent-install", ix))
                    .outline()
                    .xsmall()
                    .label("Install")
                    .disabled(status.entry.registry_agent().is_none())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.install(install_id.clone(), cx);
                    }))
                    .into_any_element()
            }
        };
        let failed = match self.installs.get(&id) {
            Some(Install::Failed(error)) => Some(error.clone()),
            _ => None,
        };
        let version: Option<SharedString> =
            status
                .installed
                .as_ref()
                .map(|i| match self.updated.get(&i.id) {
                    Some(updated) if *updated == i.version => {
                        format!("Updated to v{updated}").into()
                    }
                    _ => format!("v{}", i.version).into(),
                });
        let select_id = id;
        v_flex()
            .id(("agent-row", ix))
            .p_2()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(if selected {
                theme.primary
            } else {
                theme.border
            })
            .bg(theme.background)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.selected.clone_from(&select_id);
                cx.notify();
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_semibold()
                                            .child(status.entry.name.clone()),
                                    )
                                    .when_some(version, |this, version| {
                                        this.child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(version),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(status.entry.description.clone()),
                            ),
                    )
                    .child(action),
            )
            .when_some(status.entry.notice, |this, notice| {
                this.when(selected, |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(notice),
                    )
                })
            })
            .when_some(failed, |this, error| {
                this.child(div().text_xs().text_color(theme.danger).child(error))
            })
            .when_some(self.sign_in_status(&status.entry.id), |this, signed_in| {
                let has_key = self.keys.contains_key(&status.entry.id);
                let remove_id = status.entry.id.clone();
                this.child(
                    h_flex()
                        .debug_selector(move || format!("agent-row-signed-in-{ix}"))
                        .gap_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(Icon::new(IconName::KeyRound).xsmall())
                        .child(div().flex_1().min_w_0().truncate().child(signed_in))
                        .when(has_key, |this| {
                            this.child(
                                Button::new(("agent-row-remove-key", ix))
                                    .ghost()
                                    .xsmall()
                                    .label("Remove key")
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.remove_key(remove_id.clone(), cx);
                                    })),
                            )
                        }),
                )
            })
            .into_any_element()
    }

    fn history_view(&self, cx: &mut Context<'_, Self>) -> AnyElement {
        let theme = cx.theme().colors;
        let mut list = v_flex().p_2().gap_1().child(
            div()
                .text_xs()
                .font_semibold()
                .text_color(theme.muted_foreground)
                .child("CONVERSATIONS ABOUT THIS THREAD"),
        );
        if self.context.is_none() {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Select a mail thread to see its conversations."),
            );
        } else if self.history.is_empty() {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("No conversations yet."),
            );
        }
        for (ix, info) in self.history.iter().enumerate() {
            let agent_name: SharedString = self.status_of(&info.agent_id).map_or_else(
                || info.agent_id.clone().into(),
                |s| s.entry.name.clone().into(),
            );
            let title: SharedString = info
                .title
                .clone()
                .unwrap_or_else(|| "Untitled conversation".to_owned())
                .into();
            let resume = info.clone();
            list = list.child(
                v_flex()
                    .id(("agent-history-row", ix))
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.resume(resume.clone(), cx);
                    }))
                    .child(div().text_sm().truncate().child(title))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(agent_name),
                    ),
            );
        }
        list.into_any_element()
    }

    fn proposals_view(&self, cx: &mut Context<'_, Self>) -> Option<AnyElement> {
        let pending = self.pending_items();
        if pending.is_empty() {
            return None;
        }
        let theme = cx.theme().colors;
        let cursor = pending.get(self.proposal_cursor).map(|(_, item)| *item);
        let mut section = v_flex()
            .id("agent-proposals")
            .key_context("AgentProposals")
            .track_focus(&self.review_focus)
            .m_2()
            .p_2()
            .gap_2()
            .rounded_lg()
            .border_1()
            .border_color(theme.primary)
            .bg(theme.background)
            .on_action(cx.listener(|this, _: &AcceptProposal, _, cx| this.decide_cursor(true, cx)))
            .on_action(cx.listener(|this, _: &RejectProposal, _, cx| this.decide_cursor(false, cx)))
            .on_action(cx.listener(|this, _: &NextProposal, _, cx| this.move_cursor(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousProposal, _, cx| this.move_cursor(-1, cx)))
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .font_semibold()
                            .child(SharedString::from(format!(
                                "Proposed changes · {}",
                                pending.len()
                            ))),
                    )
                    .child(
                        Button::new("agent-accept-all")
                            .primary()
                            .xsmall()
                            .label("Accept all ⌘⇧↵")
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.decide_all(true, cx)
                                }),
                            ),
                    )
                    .child(
                        Button::new("agent-reject-all")
                            .ghost()
                            .xsmall()
                            .label("Reject all")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.decide_all(false, cx)
                            })),
                    ),
            );
        for proposal in &self.proposals {
            if proposal.pending().next().is_none() {
                continue;
            }
            let mut group = v_flex().gap_0p5().child(
                h_flex()
                    .gap_1()
                    .text_xs()
                    .child(div().font_semibold().child(proposal.title.clone()))
                    .when_some(proposal.source.clone(), |this, source| {
                        this.child(div().text_color(theme.muted_foreground).child(source))
                    }),
            );
            for item in &proposal.items {
                let item_id = item.id;
                let highlighted = cursor == Some(item_id);
                let row = h_flex()
                    .id(("agent-proposal-item", item_id as u64))
                    .gap_1()
                    .px_1()
                    .rounded_sm()
                    .when(highlighted, |this| this.bg(theme.muted))
                    .text_xs()
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        if let Some(position) =
                            this.pending_items().iter().position(|(_, i)| *i == item_id)
                        {
                            this.proposal_cursor = position;
                        }
                        window.focus(&this.review_focus, cx);
                        cx.notify();
                    }))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(item.summary.clone())
                            .when_some(item.reason.as_ref(), |this, reason| {
                                this.child(
                                    div()
                                        .text_color(theme.muted_foreground)
                                        .truncate()
                                        .child(reason.expose().clone()),
                                )
                            }),
                    );
                let row = match item.status {
                    ProposalItemStatus::Pending => row
                        .child(
                            Button::new(("agent-accept", item_id as u64))
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(IconName::Check))
                                .tooltip("Accept (y)")
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.decide_item(item_id, true, cx);
                                })),
                        )
                        .child(
                            Button::new(("agent-reject", item_id as u64))
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(IconName::Close))
                                .tooltip("Reject (n)")
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.decide_item(item_id, false, cx);
                                })),
                        ),
                    ProposalItemStatus::Applied => {
                        row.child(div().text_color(theme.success).child("applied"))
                    }
                    ProposalItemStatus::Rejected => {
                        row.child(div().text_color(theme.muted_foreground).child("rejected"))
                    }
                    ProposalItemStatus::Failed => {
                        row.child(div().text_color(theme.danger).child("failed"))
                    }
                };
                group = group.child(row);
            }
            section = section.child(group);
        }
        Some(section.into_any_element())
    }

    fn conversation(&self, session: &Session, cx: &mut Context<'_, Self>) -> AnyElement {
        let theme = cx.theme().colors;
        let mut body = v_flex().py_2().gap_2();
        if session.status == SessionStatus::SignInRequired {
            body = body.child(self.sign_in_card(session, cx));
        } else if let Some(status) = self.sign_in_status(&session.agent_id) {
            body = body.child(
                h_flex()
                    .debug_selector(|| "agent-signed-in".to_owned())
                    .px_4()
                    .gap_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(IconName::KeyRound).xsmall())
                    .child(status),
            );
        }
        if session.transcript.entries.is_empty() && session.status != SessionStatus::SignInRequired
        {
            let hint: SharedString = match session.status {
                SessionStatus::Starting => format!("Starting {}…", session.name).into(),
                _ => match &session.attached {
                    Some(_) => {
                        "Ask about the selected thread, e.g. “Summarize this” or “Draft a reply”."
                            .into()
                    }
                    None => "Ask the agent to search, summarize or triage your mail.".into(),
                },
            };
            body = body.child(
                v_flex()
                    .debug_selector(|| "agent-empty".to_owned())
                    .py_8()
                    .px_4()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(
                        Icon::new(if session.status == SessionStatus::Starting {
                            IconName::LoaderCircle
                        } else {
                            IconName::Bot
                        })
                        .large()
                        .text_color(theme.muted_foreground),
                    )
                    .child(div().text_center().child(hint)),
            );
        }
        for (ix, entry) in session.transcript.entries.iter().enumerate() {
            body = body.child(self.entry(ix, entry, session, cx));
        }
        if session.status == SessionStatus::Working {
            body = body.child(
                h_flex()
                    .debug_selector(|| "agent-working".to_owned())
                    .px_4()
                    .gap_1p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(IconName::LoaderCircle).xsmall())
                    .child("Working…"),
            );
        }
        if let Some(ended) = &session.ended {
            let (text, error): (SharedString, bool) = match ended {
                Some(error) => (format!("The agent stopped: {error}").into(), true),
                None => ("The conversation ended.".into(), false),
            };
            body = body.child(
                h_flex()
                    .mx_2()
                    .p_2()
                    .gap_2()
                    .items_start()
                    .rounded_md()
                    .border_1()
                    .border_color(if error {
                        theme.danger.opacity(0.4)
                    } else {
                        theme.border
                    })
                    .when(error, |this| this.bg(theme.danger.opacity(0.08)))
                    .text_xs()
                    .child(
                        Icon::new(if error {
                            IconName::CircleX
                        } else {
                            IconName::Info
                        })
                        .small()
                        .text_color(if error {
                            theme.danger
                        } else {
                            theme.muted_foreground
                        }),
                    )
                    .child(div().flex_1().min_w_0().child(text)),
            );
        }
        body.into_any_element()
    }

    fn footer(&self, session: &Session, cx: &mut Context<'_, Self>) -> AnyElement {
        let theme = cx.theme().colors;
        let working = session.status == SessionStatus::Working;
        let disabled =
            session.ended.is_some() || session.status != SessionStatus::Ready && !working;
        let attached_differs = match (&self.context, &session.attached) {
            (Some(_), None) => true,
            (Some(context), Some(attached)) => context.thread.to_string() != attached.public_id,
            _ => false,
        };
        let chip: Option<SharedString> = match (&session.attached, &self.context) {
            (Some(_), Some(context)) if !attached_differs => {
                Some(format!("@thread {}", context.subject).into())
            }
            (Some(attached), _) => Some(format!("@thread {}", attached.public_id).into()),
            _ => None,
        };
        v_flex()
            .flex_none()
            .px_2()
            .pt_1p5()
            .pb_2()
            .gap_1p5()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.background)
            .when_some(session.context_note.clone(), |this, note| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(note),
                )
            })
            .child(
                h_flex()
                    .gap_1()
                    .when_some(chip, |this, chip| {
                        this.child(
                            div()
                                .px_1()
                                .rounded_sm()
                                .bg(theme.muted)
                                .text_xs()
                                .truncate()
                                .child(chip),
                        )
                    })
                    .when(attached_differs, |this| {
                        this.child(
                            Button::new("agent-attach")
                                .ghost()
                                .xsmall()
                                .label("Attach selected thread")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.attach_current_thread(cx);
                                })),
                        )
                    }),
            )
            .child(
                div().key_context("AgentPrompt").child(
                    Textarea::new(&self.prompt)
                        .small()
                        .appearance(false)
                        .disabled(disabled),
                ),
            )
            .child({
                let (left, right) = self.control_bar(session, cx);
                h_flex()
                    .debug_selector(|| "agent-prompt-bar".to_owned())
                    .gap_0p5()
                    .flex_wrap()
                    .children(left)
                    .child(div().flex_1())
                    .children(right)
                    .child(if working {
                        Button::new("agent-stop")
                            .danger()
                            .small()
                            .icon(Icon::new(IconName::Square))
                            .tooltip("Stop")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                if let Some(session) = &this.session {
                                    session.handle.cancel();
                                }
                                cx.notify();
                            }))
                    } else {
                        Button::new("agent-send")
                            .primary()
                            .small()
                            .icon(Icon::new(IconName::ArrowUp))
                            .tooltip("Send (⏎)")
                            .disabled(disabled)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.send_prompt(window, cx);
                            }))
                    })
            })
            .into_any_element()
    }
}

impl Focusable for AgentPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for AgentPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let banners = self.banners(cx);
        let proposals = self.proposals_view(cx);
        let body: AnyElement = match (self.view, self.session.as_ref()) {
            (View::History, _) => self.history_view(cx),
            (View::Main, Some(session)) => self.conversation(session, cx),
            (View::Main, None) => self.picker(cx),
        };
        let footer = match (self.view, self.session.as_ref()) {
            (View::Main, Some(session)) => Some(self.footer(session, cx)),
            _ => None,
        };
        v_flex()
            .id("agent-panel")
            .key_context("AgentPanel")
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.sidebar)
            .on_action(cx.listener(|this, _: &NewThread, _, cx| this.new_thread(cx)))
            .on_action(cx.listener(|this, _: &ShowHistory, _, cx| {
                this.view = View::History;
                this.load_history(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &FocusPrompt, window, cx| {
                let focus = this.prompt.focus_handle(cx);
                window.focus(&focus, cx);
            }))
            .on_action(cx.listener(|this, _: &AcceptAllProposals, _, cx| this.decide_all(true, cx)))
            .on_action(
                cx.listener(|this, _: &RejectAllProposals, _, cx| this.decide_all(false, cx)),
            )
            .on_action(cx.listener(|this, _: &CycleMode, _, cx| {
                this.cycle_control(agent::ControlCategory::Mode, cx);
            }))
            .on_action(cx.listener(|this, _: &CycleModel, _, cx| {
                this.cycle_control(agent::ControlCategory::Model, cx);
            }))
            .on_action(cx.listener(|this, _: &CycleThinking, _, cx| {
                this.cycle_control(agent::ControlCategory::ThoughtLevel, cx);
            }))
            .on_action(cx.listener(|this, _: &StopAgent, _, cx| {
                if let Some(session) = &this.session {
                    session.handle.cancel();
                }
                cx.notify();
            }))
            .child(self.header(cx))
            .child(
                div()
                    .id("agent-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(banners)
                    .children(proposals)
                    .child(body),
            )
            .children(footer)
    }
}
