//! The "Edit Account" dialog: a new name, password or servers for an account, and a new
//! browser sign-in for Google and Microsoft accounts (`mazemail_core::accounts::edit`).
//!
//! It opens from the folder pane's account menu ("Edit Account…"), from "Sync activity"
//! when an account needs a sign-in or has an error, and from the command palette
//! ("Maze: Edit Account", for the account of the shown mail or folder), all through
//! [`mail_ui::actions::EditAccountFor`]. The fields start with the account's settings;
//! saving sends only what changed ([`changes`]) to [`CoreHandle::update_account`], which
//! checks the new settings against the server first, so a wrong password or an
//! unreachable server keeps the dialog open with the error. The password field stays
//! empty: leave it to keep the saved password, type one to replace it. Changing the
//! servers or the user name needs the password again (the core refuses otherwise),
//! and the dialog says so as soon as such a change is typed.
//!
//! Google and Microsoft accounts have fixed servers: their dialog has the name, the
//! tenant for Microsoft and "Sign in with … again" ([`CoreHandle::sign_in_again`]).
//! "Test Connection" logs in with the fields' settings (or the current sign-in) and
//! saves nothing ([`CoreHandle::test_account_changes`]).

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    App, AppContext as _, Entity, ParentElement as _, SharedString, Styled as _, Window, div,
    prelude::FluentBuilder as _,
};
use mail_ui::DataChanges;
use mazemail_core::{
    AccountAuth, AccountChanges, AccountDetails, CoreHandle, ImapSecurity, ImapServer,
    ProviderKind, SmtpServerSettings, SyncState, smtp_default_port,
};
use mazemail_types::{AccountId, PRODUCT_NAME, Sensitive};
use ui::kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, StyledExt as _,
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    notification::Notification,
    v_flex,
};

use super::add_account::{TENANT_HINT, connection_ok, parse_tenant, security_label, value};

/// What the fields say, as typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Fields {
    pub(crate) name: String,
    pub(crate) password: String,
    pub(crate) host: String,
    pub(crate) port: String,
    pub(crate) security: ImapSecurity,
    pub(crate) username: String,
    pub(crate) smtp_host: String,
    pub(crate) smtp_port: String,
    pub(crate) smtp_security: ImapSecurity,
}

/// The fields for an account as it is.
pub(crate) fn fields_of(details: &AccountDetails) -> Fields {
    let server = details.info.server.clone();
    let smtp = details.smtp.clone();
    Fields {
        name: details.info.name.clone(),
        password: String::new(),
        host: server.as_ref().map(|s| s.host.clone()).unwrap_or_default(),
        port: server
            .as_ref()
            .map(|s| s.port.to_string())
            .unwrap_or_default(),
        security: server.map(|s| s.security).unwrap_or_default(),
        username: details.username.clone(),
        smtp_host: smtp.as_ref().map(|s| s.host.clone()).unwrap_or_default(),
        smtp_port: smtp
            .as_ref()
            .map(|s| s.port.to_string())
            .unwrap_or_default(),
        smtp_security: smtp.map_or(ImapSecurity::Starttls, |s| s.security),
    }
}

fn port(text: &str, default: u16, which: &str) -> Result<u16, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(default);
    }
    text.parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| format!("The {which} port must be a number from 1 to 65535."))
}

fn signs_in_with(details: &AccountDetails) -> Option<ProviderKind> {
    match details.info.auth {
        Some(AccountAuth::OAuth(provider)) => Some(provider),
        _ => None,
    }
}

/// What the fields change about the account (only what differs from `details`), or
/// what's wrong with them.
pub(crate) fn changes(details: &AccountDetails, fields: &Fields) -> Result<AccountChanges, String> {
    let name = fields.name.trim();
    if name.is_empty() {
        return Err("Enter a name for the account.".to_owned());
    }
    let mut changes = AccountChanges {
        name: (name != details.info.name).then(|| name.to_owned()),
        password: (!fields.password.is_empty()).then(|| Sensitive::new(fields.password.clone())),
        ..AccountChanges::default()
    };
    // Browser sign-ins have fixed servers; accounts without a server have none to edit.
    let Some(current) = details
        .info
        .server
        .as_ref()
        .filter(|_| signs_in_with(details).is_none())
    else {
        return Ok(changes);
    };
    let host = fields.host.trim();
    if host.is_empty() {
        return Err("Enter the IMAP server.".to_owned());
    }
    let server = ImapServer {
        host: host.to_owned(),
        port: port(&fields.port, fields.security.default_port(), "IMAP")?,
        security: fields.security,
    };
    changes.server = (server != *current).then_some(server);
    let username = fields.username.trim();
    changes.username = (username != details.username.trim()).then(|| username.to_owned());
    let smtp_host = fields.smtp_host.trim();
    if !smtp_host.is_empty() || details.smtp.is_some() {
        if smtp_host.is_empty() {
            return Err("Enter the SMTP server.".to_owned());
        }
        let smtp = SmtpServerSettings {
            host: smtp_host.to_owned(),
            port: port(
                &fields.smtp_port,
                smtp_default_port(fields.smtp_security),
                "SMTP",
            )?,
            security: fields.smtp_security,
        };
        changes.smtp = (Some(&smtp) != details.smtp.as_ref()).then_some(smtp);
    }
    Ok(changes)
}

/// Whether `changes` need the password again (and don't have it).
pub(crate) fn needs_password(changes: &AccountChanges) -> bool {
    changes.password.is_none()
        && (changes.server.is_some() || changes.username.is_some() || changes.smtp.is_some())
}

/// The dialog's state, shared by its buttons.
struct Form {
    core: CoreHandle,
    details: AccountDetails,
    name: Entity<InputState>,
    password: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    username: Entity<InputState>,
    smtp_host: Entity<InputState>,
    smtp_port: Entity<InputState>,
    tenant: Entity<InputState>,
    security: ImapSecurity,
    smtp_security: ImapSecurity,
    advanced: bool,
    busy: Option<SharedString>,
    error: Option<SharedString>,
    success: Option<SharedString>,
}

impl Form {
    fn fields(&self, cx: &App) -> Fields {
        Fields {
            name: value(&self.name, cx),
            password: self.password.read(cx).value().to_string(),
            host: value(&self.host, cx),
            port: value(&self.port, cx),
            security: self.security,
            username: value(&self.username, cx),
            smtp_host: value(&self.smtp_host, cx),
            smtp_port: value(&self.smtp_port, cx),
            smtp_security: self.smtp_security,
        }
    }
}

type Shared = Rc<RefCell<Form>>;

/// Loads the account's settings and opens the dialog in the main window.
pub(crate) fn show(core: CoreHandle, account: AccountId, cx: &mut App) {
    let load = {
        let core = core.clone();
        cx.background_spawn(async move { core.account_details(account).await })
    };
    cx.spawn(async move |cx| {
        let details = load.await;
        cx.update(|cx| match details {
            Ok(details) => open_window(core, details, cx),
            Err(error) => notify(format!("The account can't be edited: {error}"), cx),
        });
    })
    .detach();
}

fn notify(message: String, cx: &mut App) {
    let Some(window) = super::main_window(cx) else {
        tracing::info!("{message}");
        return;
    };
    let shown = window.update(cx, |_, window, cx| {
        window.push_notification(Notification::info(message).title(PRODUCT_NAME), cx);
    });
    if let Err(error) = shown {
        tracing::debug!(%error, "no main window for an account notification");
    }
}

fn open_window(core: CoreHandle, details: AccountDetails, cx: &mut App) {
    let Some(window) = super::main_window(cx) else {
        return;
    };
    let opened = window.update(cx, |_, window, cx| {
        let fields = fields_of(&details);
        let input = |text: &str, placeholder: &str, window: &mut Window, cx: &mut App| {
            let (text, placeholder) = (text.to_owned(), placeholder.to_owned());
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(text)
            })
        };
        let password_hint = if signs_in_with(&details).is_some() {
            "No password: signs in in the browser"
        } else if details.has_password {
            "New password (leave empty to keep the saved one)"
        } else {
            "Password (none saved yet)"
        };
        let tenant = details
            .tenant
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        let form =
            Form {
                core,
                name: input(&fields.name, "Name in the folder pane", window, cx),
                password: cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(password_hint)
                        .masked(true)
                }),
                host: input(&fields.host, "IMAP server", window, cx),
                port: input(&fields.port, "Port", window, cx),
                username: input(
                    &fields.username,
                    "User name (default: the address)",
                    window,
                    cx,
                ),
                smtp_host: input(&fields.smtp_host, "SMTP server", window, cx),
                smtp_port: input(&fields.smtp_port, "Port", window, cx),
                tenant: input(&tenant, "common", window, cx),
                security: fields.security,
                smtp_security: fields.smtp_security,
                // A server that doesn't work: show its settings right away.
                advanced: details.info.sync.as_ref().is_some_and(|s| {
                    matches!(s.state, SyncState::Error { .. } | SyncState::Offline)
                }),
                busy: None,
                error: None,
                success: None,
                details,
            };
        open(Rc::new(RefCell::new(form)), window, cx);
    });
    if let Err(error) = opened {
        tracing::error!(%error, "cannot show the Edit Account dialog");
    }
}

/// Switches a security button; a port that was the old security's usual one follows.
fn switch_security(
    state: &Shared,
    smtp: bool,
    security: ImapSecurity,
    window: &mut Window,
    cx: &mut App,
) {
    let mut form = state.borrow_mut();
    let (old, port, usual) = if smtp {
        let old = std::mem::replace(&mut form.smtp_security, security);
        (
            old,
            form.smtp_port.clone(),
            smtp_default_port as fn(ImapSecurity) -> u16,
        )
    } else {
        let old = std::mem::replace(&mut form.security, security);
        (
            old,
            form.port.clone(),
            ImapSecurity::default_port as fn(ImapSecurity) -> u16,
        )
    };
    drop(form);
    let typed = value(&port, cx);
    if typed.is_empty() || typed == usual(old).to_string() {
        let text = usual(security).to_string();
        port.update(cx, |input, cx| input.set_value(text, window, cx));
    }
    window.refresh();
}

fn open(state: Shared, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme();
        let form = state.borrow();
        let details = &form.details;
        let provider = signs_in_with(details);
        let has_server = details.info.server.is_some() && provider.is_none();
        let planned = changes(details, &form.fields(cx));
        let password_needed = planned.as_ref().is_ok_and(needs_password);
        let busy = form.busy.clone();
        let error = form.error.clone();
        let success = form.success.clone();
        let advanced = form.advanced;
        let (security, smtp_security) = (form.security, form.smtp_security);
        let address: SharedString = details
            .info
            .email
            .clone()
            .unwrap_or_else(|| details.info.name.clone())
            .into();
        let how: SharedString = match (provider, &details.info.server) {
            (Some(provider), Some(server)) => format!(
                "Signs in with {} · {}:{}",
                provider.display_name(),
                server.host,
                server.port
            ),
            (None, Some(server)) => format!(
                "Password · {}:{} · {}",
                server.host,
                server.port,
                security_label(server.security)
            ),
            _ => "Local Folders: no server".to_owned(),
        }
        .into();
        let problem: Option<SharedString> = details.info.sync.as_ref().and_then(|sync| {
            matches!(
                sync.state,
                SyncState::AuthRequired | SyncState::Error { .. } | SyncState::Offline
            )
            .then(|| sync.text().into())
        });
        let sign_in_needed = details
            .info
            .sync
            .as_ref()
            .is_some_and(|s| s.state == SyncState::AuthRequired);

        let security_button = |smtp: bool, choice: ImapSecurity| {
            let state = state.clone();
            let selected = if smtp { smtp_security } else { security };
            Button::new(SharedString::from(format!(
                "{}security-{}",
                if smtp { "smtp-" } else { "" },
                choice.as_str()
            )))
            .small()
            .label(security_label(choice))
            .selected(selected == choice)
            .on_click(move |_, window, cx| switch_security(&state, smtp, choice, window, cx))
        };
        let toggle_advanced = {
            let state = state.clone();
            move |_: &gpui::ClickEvent, window: &mut Window, _: &mut App| {
                let mut form = state.borrow_mut();
                form.advanced = !form.advanced;
                window.refresh();
            }
        };
        let save = {
            let state = state.clone();
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| save(&state, window, cx)
        };
        let test = {
            let state = state.clone();
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| test(&state, window, cx)
        };
        let sign_in = provider.map(|provider| {
            let state = state.clone();
            Button::new("sign-in-again")
                .small()
                .label(format!("Sign in with {} again", provider.display_name()))
                .when(sign_in_needed, |b| b.primary())
                .disabled(busy.is_some())
                .on_click(move |_, window, cx| sign_in_again(&state, provider, window, cx))
        });
        let muted = |text: SharedString| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };

        dialog
            .title("Edit Account")
            .width(ui::theme::scaled(520., cx))
            .child(
                v_flex()
                    .gap_2()
                    .text_sm()
                    .child(
                        v_flex()
                            .child(div().font_medium().child(address))
                            .child(muted(how)),
                    )
                    .when_some(problem, |this, problem| {
                        this.child(div().text_xs().text_color(theme.danger).child(problem))
                    })
                    .child(Input::new(&form.name))
                    .when(
                        provider.is_none() && details.info.server.is_some(),
                        |this| this.child(Input::new(&form.password)),
                    )
                    .when(password_needed, |this| {
                        this.child(div().text_xs().text_color(theme.danger).child(
                            "Enter the password: changed servers or a new user name need \
                                 it again (Maze only sends a saved password to the servers \
                                 it was saved for).",
                        ))
                    })
                    .when(provider == Some(ProviderKind::Microsoft), |this| {
                        this.child(
                            v_flex()
                                .gap_1()
                                .child(muted(TENANT_HINT.into()))
                                .child(Input::new(&form.tenant)),
                        )
                    })
                    .when(has_server, |this| {
                        this.child(
                            h_flex().child(
                                Button::new("advanced")
                                    .ghost()
                                    .xsmall()
                                    .label(if advanced {
                                        "Hide server settings"
                                    } else {
                                        "Server settings…"
                                    })
                                    .on_click(toggle_advanced),
                            ),
                        )
                    })
                    .when(has_server && advanced, |this| {
                        this.child(
                            h_flex()
                                .gap_2()
                                .child(div().flex_1().child(Input::new(&form.host)))
                                .child(div().w_20().child(Input::new(&form.port))),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .child(security_button(false, ImapSecurity::Tls))
                                .child(security_button(false, ImapSecurity::Starttls))
                                .child(security_button(false, ImapSecurity::Insecure)),
                        )
                        .child(Input::new(&form.username))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().flex_1().child(Input::new(&form.smtp_host)))
                                .child(div().w_20().child(Input::new(&form.smtp_port))),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .child(security_button(true, ImapSecurity::Tls))
                                .child(security_button(true, ImapSecurity::Starttls))
                                .child(security_button(true, ImapSecurity::Insecure)),
                        )
                    })
                    .when_some(busy.clone(), |this, busy| {
                        this.child(div().text_color(theme.muted_foreground).child(busy))
                    })
                    .when_some(success, |this, success| {
                        this.child(div().text_color(theme.success).child(success))
                    })
                    .when_some(error, |this, error| {
                        this.child(div().text_color(theme.danger).child(error))
                    }),
            )
            .footer(
                h_flex()
                    .w_full()
                    .justify_between()
                    .gap_2()
                    .child(h_flex().gap_2().children(sign_in))
                    .child(
                        h_flex()
                            .gap_2()
                            .when(details.info.server.is_some(), |this| {
                                this.child(
                                    Button::new("test-connection")
                                        .small()
                                        .label("Test Connection")
                                        .disabled(busy.is_some())
                                        .on_click(test),
                                )
                            })
                            .child(
                                Button::new("save-account")
                                    .primary()
                                    .small()
                                    .label(if busy.is_some() {
                                        "Checking…"
                                    } else {
                                        "Save"
                                    })
                                    .disabled(busy.is_some())
                                    .on_click(save),
                            ),
                    ),
            )
    });
}

fn set_status(
    state: &Shared,
    busy: Option<SharedString>,
    error: Option<SharedString>,
    window: &mut Window,
) {
    {
        let mut form = state.borrow_mut();
        form.busy = busy;
        form.error = error;
        form.success = None;
    }
    window.refresh();
}

/// The changes the fields make, or `None` after showing what's wrong.
fn planned(
    state: &Shared,
    window: &mut Window,
    cx: &mut App,
) -> Option<(CoreHandle, AccountId, AccountChanges)> {
    let result = {
        let form = state.borrow();
        changes(&form.details, &form.fields(cx))
            .map(|changes| (form.core.clone(), form.details.info.id, changes))
    };
    match result {
        Ok(planned) if needs_password(&planned.2) => {
            set_status(
                state,
                None,
                Some(
                    "Enter the password: changed servers or a new user name need it again.".into(),
                ),
                window,
            );
            None
        }
        Ok(planned) => Some(planned),
        Err(error) => {
            set_status(state, None, Some(error.into()), window);
            None
        }
    }
}

/// Runs `work` in the core; closes the dialog and reloads the window on success, shows
/// the error otherwise.
fn run(
    state: &Shared,
    busy: &str,
    work: impl Future<Output = mazemail_core::Result<mazemail_core::AccountInfo>> + Send + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    set_status(state, Some(busy.to_owned().into()), None, window);
    let attempt = cx.background_spawn(work);
    let handle = window.window_handle();
    let state = state.clone();
    cx.spawn(async move |cx| {
        let result = attempt.await;
        cx.update(|cx| match result {
            Ok(info) => {
                tracing::info!(account = %info.id, "account changed from the dialog");
                let _ = handle.update(cx, |_, window, cx| window.close_dialog(cx));
                super::apply_changes(&DataChanges::everything(), cx);
            }
            Err(error) => {
                let _ = handle.update(cx, |_, window, _| {
                    set_status(&state, None, Some(error.to_string().into()), window);
                });
            }
        });
    })
    .detach();
}

fn save(state: &Shared, window: &mut Window, cx: &mut App) {
    let Some((core, account, changes)) = planned(state, window, cx) else {
        return;
    };
    let message = match &changes.server {
        Some(server) => format!("Connecting to {}…", server.host),
        None if changes.password.is_some() => "Checking the password…".to_owned(),
        None => "Saving…".to_owned(),
    };
    run(
        state,
        &message,
        async move { core.update_account(account, changes).await },
        window,
        cx,
    );
}

/// Logs in with the fields' settings without saving; shows the outcome.
fn test(state: &Shared, window: &mut Window, cx: &mut App) {
    let Some((core, account, changes)) = planned(state, window, cx) else {
        return;
    };
    let host = {
        let form = state.borrow();
        changes
            .server
            .as_ref()
            .map(|s| s.host.clone())
            .or_else(|| form.details.info.server.as_ref().map(|s| s.host.clone()))
            .unwrap_or_default()
    };
    set_status(state, Some(format!("Testing {host}…").into()), None, window);
    let attempt =
        cx.background_spawn(async move { core.test_account_changes(account, changes).await });
    let handle = window.window_handle();
    let state = state.clone();
    cx.spawn(async move |cx| {
        let result = attempt.await;
        cx.update(|cx| {
            let _ = handle.update(cx, |_, window, _| match result {
                Ok(test) => {
                    let imap = connection_ok(&host, test.imap);
                    match test.smtp {
                        Ok(server) => {
                            set_status(&state, None, None, window);
                            state.borrow_mut().success = Some(
                                format!("{imap} Sending through {} works too.", server.host).into(),
                            );
                        }
                        Err(error) => set_status(
                            &state,
                            None,
                            Some(format!("{imap} But {error}").into()),
                            window,
                        ),
                    }
                    window.refresh();
                }
                Err(error) => set_status(&state, None, Some(error.to_string().into()), window),
            });
        });
    })
    .detach();
}

fn sign_in_again(state: &Shared, provider: ProviderKind, window: &mut Window, cx: &mut App) {
    let (core, account, tenant) = {
        let form = state.borrow();
        let tenant = match provider {
            ProviderKind::Microsoft => match parse_tenant(&value(&form.tenant, cx)) {
                Ok(tenant) => Some(tenant),
                Err(error) => {
                    drop(form);
                    set_status(state, None, Some(error.into()), window);
                    return;
                }
            },
            ProviderKind::Google => None,
        };
        (form.core.clone(), form.details.info.id, tenant)
    };
    let message = format!(
        "Continue in your browser: sign in with {} and allow {PRODUCT_NAME} access…",
        provider.display_name()
    );
    run(
        state,
        &message,
        async move { core.sign_in_again(account, tenant).await },
        window,
        cx,
    );
}

#[cfg(test)]
mod tests {
    use mazemail_core::AccountInfo;
    use store::AccountKind;

    use super::*;

    fn details(auth: AccountAuth) -> AccountDetails {
        AccountDetails {
            info: AccountInfo {
                id: AccountId(1),
                uuid: auth::Uuid::nil(),
                name: "Work".into(),
                kind: AccountKind::Imap,
                email: Some("alice@example.org".into()),
                server: Some(ImapServer::new("imap.example.org", ImapSecurity::Tls)),
                auth: Some(auth),
                sync: None,
            },
            username: String::new(),
            smtp: Some(SmtpServerSettings::new(
                "smtp.example.org",
                ImapSecurity::Starttls,
            )),
            tenant: None,
            has_password: true,
        }
    }

    #[test]
    fn unchanged_fields_change_nothing() {
        let details = details(AccountAuth::Password);
        let fields = fields_of(&details);
        assert_eq!(fields.port, "993");
        assert_eq!(fields.smtp_port, "587");
        let changes = changes(&details, &fields).unwrap();
        assert!(changes.name.is_none() && changes.password.is_none());
        assert!(changes.server.is_none() && changes.username.is_none() && changes.smtp.is_none());
        assert!(!needs_password(&changes));
    }

    #[test]
    fn only_what_differs_is_sent() {
        let details = details(AccountAuth::Password);
        let mut fields = fields_of(&details);
        fields.name = " Job ".into();
        fields.port = String::new(); // TLS's usual port, as before
        let renamed = changes(&details, &fields).unwrap();
        assert_eq!(renamed.name.as_deref(), Some("Job"));
        assert!(renamed.server.is_none());
        assert!(!needs_password(&renamed));

        fields.security = ImapSecurity::Starttls;
        fields.port = "143".into();
        fields.smtp_host = "mail.example.org".into();
        let moved = changes(&details, &fields).unwrap();
        assert_eq!(
            moved.server,
            Some(ImapServer::new("imap.example.org", ImapSecurity::Starttls))
        );
        assert_eq!(moved.smtp.as_ref().map(|s| s.port), Some(587));
        assert!(needs_password(&moved), "a new server needs the password");
        fields.password = "secret".into();
        assert!(!needs_password(&changes(&details, &fields).unwrap()));
    }

    #[test]
    fn bad_fields_are_reported() {
        let details = details(AccountAuth::Password);
        let check = |edit: fn(&mut Fields)| {
            let mut fields = fields_of(&details);
            edit(&mut fields);
            changes(&details, &fields).unwrap_err()
        };
        assert!(check(|f| f.name = " ".into()).contains("name"));
        assert!(check(|f| f.host = String::new()).contains("IMAP server"));
        assert!(check(|f| f.port = "99999".into()).contains("IMAP port"));
        assert!(check(|f| f.smtp_port = "0".into()).contains("SMTP port"));
        assert!(check(|f| f.smtp_host = String::new()).contains("SMTP server"));
    }

    #[test]
    fn browser_sign_ins_only_change_the_name() {
        let details = details(AccountAuth::OAuth(ProviderKind::Google));
        let mut fields = fields_of(&details);
        fields.name = "Personal".into();
        fields.host = "elsewhere.example".into();
        let changes = changes(&details, &fields).unwrap();
        assert_eq!(changes.name.as_deref(), Some("Personal"));
        assert!(changes.server.is_none() && changes.smtp.is_none());
    }
}
