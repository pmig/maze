//! The "Add Account" dialog (CONCEPT §6): an address and a password for any IMAP
//! server, or "Sign in with Google / Microsoft" in the browser.
//!
//! The server is detected from the address for well-known providers
//! ([`suggest_imap_server`]: Gmail, Outlook.com, iCloud, Yahoo, Fastmail, GMX, …, else
//! `imap.<domain>` with TLS on 993) and shown under the fields, with the SMTP server
//! for sending ([`suggest_smtp_server`], else `smtp.<domain>` with STARTTLS on 587);
//! "Server settings…" opens host, port, security and user name of both to override
//! them (SMTP logs in like IMAP). When the address belongs
//! to Google or Microsoft (by domain, or by the host for Microsoft 365), the matching
//! sign-in button is highlighted. For Microsoft, the organization (tenant) field shows
//! (also under "Server settings…"), filled in from `auth.microsoft.tenant`; the account
//! keeps the tenant it signs in with.
//!
//! Adding runs in the core ([`CoreHandle::add_imap_account`],
//! [`CoreHandle::add_oauth_account`]), which connects and lists the folders before it
//! saves anything; errors (wrong password, unreachable server, no OAuth client ID)
//! keep the dialog open with the message. "Test Connection" does the same check for a
//! password account ([`CoreHandle::test_imap_account`]), then logs in to the SMTP
//! server ([`CoreHandle::test_smtp_account`]), and saves nothing. The dialog opens from the empty state's
//! button, the folder pane's "Add Account…" and the command palette
//! ("Maze: Add Account").

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    App, AppContext as _, Entity, ParentElement as _, SharedString, Styled as _, Window, div,
    prelude::FluentBuilder as _,
};
use mail_ui::DataChanges;
use mazemail_core::{
    AddImapAccount, AddOAuthAccount, ConnectionTest, CoreHandle, ImapSecurity, ImapServer,
    ProviderKind, SmtpServerSettings, smtp_default_port, suggest_imap_server, suggest_smtp_server,
};
use mazemail_types::{MicrosoftTenant, PRODUCT_NAME, Sensitive};
use ui::kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    v_flex,
};

/// The dialog's state, shared by its buttons.
struct Form {
    core: CoreHandle,
    email: Entity<InputState>,
    name: Entity<InputState>,
    password: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    username: Entity<InputState>,
    smtp_host: Entity<InputState>,
    smtp_port: Entity<InputState>,
    /// The Microsoft tenant, prefilled from `auth.microsoft.tenant`.
    tenant: Entity<InputState>,
    /// `None`: as detected.
    security: Option<ImapSecurity>,
    /// `None`: as detected.
    smtp_security: Option<ImapSecurity>,
    advanced: bool,
    busy: Option<SharedString>,
    error: Option<SharedString>,
    /// The last successful "Test Connection".
    success: Option<SharedString>,
}

type Shared = Rc<RefCell<Form>>;

/// What the dialog will connect to, from the fields and the detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) server: Option<ImapServer>,
    pub(crate) detected: bool,
    pub(crate) oauth: Option<ProviderKind>,
}

/// Combines the typed server settings with what the address suggests. Empty fields
/// fall back to the suggestion; a typed host without a port gets the security's port.
pub(crate) fn plan(email: &str, host: &str, port: &str, security: Option<ImapSecurity>) -> Plan {
    let suggestion = suggest_imap_server(email);
    let host = host.trim();
    let oauth = ProviderKind::detect(Some(email.trim()), (!host.is_empty()).then_some(host))
        .or_else(|| suggestion.as_ref().and_then(|s| s.oauth));
    let detected = host.is_empty() && suggestion.as_ref().is_some_and(|s| s.known);
    let server = if host.is_empty() {
        suggestion.map(|s| {
            let security = security.unwrap_or(s.server.security);
            ImapServer {
                port: port
                    .trim()
                    .parse()
                    .unwrap_or_else(|_| security.default_port()),
                security,
                host: s.server.host,
            }
        })
    } else {
        let security = security.unwrap_or_default();
        Some(ImapServer {
            host: host.to_owned(),
            port: port
                .trim()
                .parse()
                .unwrap_or_else(|_| security.default_port()),
            security,
        })
    };
    Plan {
        server,
        detected,
        oauth,
    }
}

/// The SMTP server the dialog will use: the typed one, else the one derived from the
/// address and the IMAP host. A typed host without a port gets the security's port.
pub(crate) fn smtp_plan(
    email: &str,
    imap_host: Option<&str>,
    host: &str,
    port: &str,
    security: Option<ImapSecurity>,
) -> Option<SmtpServerSettings> {
    let host = host.trim();
    let base = if host.is_empty() {
        suggest_smtp_server(email, imap_host)?.server
    } else {
        SmtpServerSettings::new(host, security.unwrap_or(ImapSecurity::Starttls))
    };
    let security = security.unwrap_or(base.security);
    let port = port.trim().parse().unwrap_or(if security == base.security {
        base.port
    } else {
        smtp_default_port(security)
    });
    Some(SmtpServerSettings {
        host: base.host,
        port,
        security,
    })
}

/// The SMTP server the fields describe.
fn form_smtp(
    form: &Form,
    email: &str,
    imap: Option<&ImapServer>,
    cx: &App,
) -> Option<SmtpServerSettings> {
    smtp_plan(
        email,
        imap.map(|s| s.host.as_str()),
        &value(&form.smtp_host, cx),
        &value(&form.smtp_port, cx),
        form.smtp_security,
    )
}

/// Opens the dialog in the main window (no-op while the core isn't open).
pub(crate) fn show(core: CoreHandle, cx: &mut App) {
    let Some(window) = super::main_window(cx) else {
        return;
    };
    let opened = window.update(cx, |_, window, cx| {
        let input = |placeholder: &str, window: &mut Window, cx: &mut App| {
            let placeholder = placeholder.to_owned();
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let tenant = core.settings().settings.auth.microsoft.tenant.to_string();
        let form = Form {
            core,
            email: input("name@example.com", window, cx),
            name: input("Name in the folder pane (optional)", window, cx),
            password: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Password or app password")
                    .masked(true)
            }),
            host: input("IMAP server (detected)", window, cx),
            port: input("Port", window, cx),
            username: input("User name (default: the address)", window, cx),
            smtp_host: input("SMTP server (detected)", window, cx),
            smtp_port: input("Port", window, cx),
            tenant: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("common")
                    .default_value(tenant)
            }),
            security: None,
            smtp_security: None,
            advanced: false,
            busy: None,
            error: None,
            success: None,
        };
        open(Rc::new(RefCell::new(form)), window, cx);
    });
    match opened {
        Ok(()) => tracing::debug!("Add Account dialog opened"),
        Err(error) => tracing::error!(%error, "cannot show the Add Account dialog"),
    }
}

fn value(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().trim().to_owned()
}

fn security_label(security: ImapSecurity) -> &'static str {
    match security {
        ImapSecurity::Tls => "TLS",
        ImapSecurity::Starttls => "STARTTLS",
        ImapSecurity::Insecure => "None (insecure)",
    }
}

fn open(state: Shared, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme();
        let form = state.borrow();
        let email = value(&form.email, cx);
        let plan = plan(
            &email,
            &value(&form.host, cx),
            &value(&form.port, cx),
            form.security,
        );
        let smtp = form_smtp(&form, &email, plan.server.as_ref(), cx);
        let busy = form.busy.clone();
        let error = form.error.clone();
        let success = form.success.clone();
        let advanced = form.advanced;
        let smtp_line: Option<SharedString> = smtp.as_ref().map(|server| {
            format!(
                "Sending: {}:{} · {}",
                server.host,
                server.port,
                security_label(server.security)
            )
            .into()
        });
        let selected_smtp_security = smtp.as_ref().map(|s| s.security);
        let server_line: SharedString = match &plan.server {
            Some(server) => format!(
                "Server: {}:{} · {}{}",
                server.host,
                server.port,
                security_label(server.security),
                if plan.detected { " (detected)" } else { "" }
            )
            .into(),
            None => "Enter your address to detect the server.".into(),
        };
        let oauth_hint = plan.oauth.map(|provider| {
            SharedString::from(format!(
                "{} accounts usually need “Sign in with {}” (or an app password).",
                provider.display_name(),
                provider.display_name()
            ))
        });
        let selected_security = plan.server.as_ref().map(|s| s.security);

        let security_button = |security: ImapSecurity| {
            let state = state.clone();
            Button::new(SharedString::from(format!(
                "security-{}",
                security.as_str()
            )))
            .small()
            .label(security_label(security))
            .selected(selected_security == Some(security))
            .on_click(move |_, window, _| {
                state.borrow_mut().security = Some(security);
                window.refresh();
            })
        };
        let smtp_security_button = |security: ImapSecurity| {
            let state = state.clone();
            Button::new(SharedString::from(format!(
                "smtp-security-{}",
                security.as_str()
            )))
            .small()
            .label(security_label(security))
            .selected(selected_smtp_security == Some(security))
            .on_click(move |_, window, _| {
                state.borrow_mut().smtp_security = Some(security);
                window.refresh();
            })
        };
        let toggle_advanced = {
            let state = state.clone();
            move |_: &gpui::ClickEvent, window: &mut Window, _: &mut App| {
                let mut form = state.borrow_mut();
                form.advanced = !form.advanced;
                window.refresh();
            }
        };
        let add = {
            let state = state.clone();
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                add_with_password(&state, window, cx);
            }
        };
        let test = {
            let state = state.clone();
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                test_with_password(&state, window, cx);
            }
        };
        let sign_in = |provider: ProviderKind| {
            let state = state.clone();
            Button::new(SharedString::from(format!("sign-in-{}", provider.as_str())))
                .small()
                .label(format!("Sign in with {}", provider.display_name()))
                .when(plan.oauth == Some(provider), |b| b.primary())
                .disabled(busy.is_some())
                .on_click(move |_, window, cx| add_with_oauth(&state, provider, window, cx))
        };

        dialog
            .title("Add Account")
            .width(ui::theme::scaled(520., cx))
            .child(
                v_flex()
                    .gap_2()
                    .text_sm()
                    .child(Input::new(&form.email))
                    .child(Input::new(&form.name))
                    .child(Input::new(&form.password))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(server_line),
                    )
                    .when_some(smtp_line, |this, line| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(line),
                        )
                    })
                    .when_some(oauth_hint, |this, hint| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(hint),
                        )
                    })
                    .when(
                        advanced || plan.oauth == Some(ProviderKind::Microsoft),
                        |this| {
                            this.child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(TENANT_HINT),
                                    )
                                    .child(Input::new(&form.tenant)),
                            )
                        },
                    )
                    .child(
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
                    .when(advanced, |this| {
                        this.child(
                            h_flex()
                                .gap_2()
                                .child(div().flex_1().child(Input::new(&form.host)))
                                .child(div().w_20().child(Input::new(&form.port))),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .child(security_button(ImapSecurity::Tls))
                                .child(security_button(ImapSecurity::Starttls))
                                .child(security_button(ImapSecurity::Insecure)),
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
                                .child(smtp_security_button(ImapSecurity::Tls))
                                .child(smtp_security_button(ImapSecurity::Starttls))
                                .child(smtp_security_button(ImapSecurity::Insecure)),
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
                    .child(
                        h_flex()
                            .gap_2()
                            .child(sign_in(ProviderKind::Google))
                            .child(sign_in(ProviderKind::Microsoft)),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("test-connection")
                                    .small()
                                    .label("Test Connection")
                                    .disabled(busy.is_some())
                                    .on_click(test),
                            )
                            .child(
                                Button::new("add-account")
                                    .primary()
                                    .small()
                                    .label(if busy.is_some() {
                                        "Checking…"
                                    } else {
                                        "Add Account"
                                    })
                                    .disabled(busy.is_some())
                                    .on_click(add),
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

/// Runs `add` in the core; closes the dialog and reloads the window on success, shows
/// the error otherwise.
fn run(
    state: &Shared,
    busy: &str,
    add: impl Future<Output = mazemail_core::Result<mazemail_core::AccountInfo>> + Send + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    set_status(state, Some(busy.to_owned().into()), None, window);
    let attempt = cx.background_spawn(add);
    let handle = window.window_handle();
    let state = state.clone();
    cx.spawn(async move |cx| {
        let result = attempt.await;
        cx.update(|cx| match result {
            Ok(info) => {
                tracing::info!(account = %info.id, "account added from the dialog");
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

/// The password account the fields describe, or `None` after showing what's missing.
fn password_input(
    state: &Shared,
    window: &mut Window,
    cx: &mut App,
) -> Option<(CoreHandle, AddImapAccount)> {
    let input = {
        let form = state.borrow();
        let email = value(&form.email, cx);
        let plan = plan(
            &email,
            &value(&form.host, cx),
            &value(&form.port, cx),
            form.security,
        );
        let password = form.password.read(cx).value().to_string();
        let username = value(&form.username, cx);
        let name = value(&form.name, cx);
        let smtp = form_smtp(&form, &email, plan.server.as_ref(), cx);
        let Some(server) = plan.server else {
            drop(form);
            set_status(
                state,
                None,
                Some("Enter your email address (name@example.com).".into()),
                window,
            );
            return None;
        };
        if password.is_empty() {
            drop(form);
            set_status(
                state,
                None,
                Some("Enter the password, or sign in with Google or Microsoft.".into()),
                window,
            );
            return None;
        }
        (
            form.core.clone(),
            AddImapAccount {
                display_name: (!name.is_empty()).then_some(name),
                email,
                server,
                username: (!username.is_empty()).then_some(username),
                password: Sensitive::new(password),
                smtp,
            },
        )
    };
    Some(input)
}

fn add_with_password(state: &Shared, window: &mut Window, cx: &mut App) {
    let Some((core, input)) = password_input(state, window, cx) else {
        return;
    };
    let message = format!("Connecting to {}…", input.server.host);
    run(
        state,
        &message,
        async move { core.add_imap_account(input).await },
        window,
        cx,
    );
}

/// Connects and logs in with the fields' settings without saving; shows the outcome.
fn test_with_password(state: &Shared, window: &mut Window, cx: &mut App) {
    let Some((core, input)) = password_input(state, window, cx) else {
        return;
    };
    let host = input.server.host.clone();
    set_status(state, Some(format!("Testing {host}…").into()), None, window);
    // IMAP first; SMTP only if IMAP works (same login).
    let attempt = cx.background_spawn(async move {
        let imap = core.test_imap_account(input.clone()).await?;
        let smtp = core.test_smtp_account(input).await;
        Ok::<_, mazemail_core::Error>((imap, smtp))
    });
    let handle = window.window_handle();
    let state = state.clone();
    cx.spawn(async move |cx| {
        let result = attempt.await;
        cx.update(|cx| {
            let _ = handle.update(cx, |_, window, _| match result {
                Ok((test, smtp)) => {
                    let imap = connection_ok(&host, test);
                    match smtp {
                        Ok(server) => {
                            set_status(&state, None, None, window);
                            state.borrow_mut().success = Some(
                                format!("{imap} Sending through {} works too.", server.host).into(),
                            );
                        }
                        Err(error) => {
                            set_status(
                                &state,
                                None,
                                Some(format!("{imap} But {error}").into()),
                                window,
                            );
                        }
                    }
                    window.refresh();
                }
                Err(error) => set_status(&state, None, Some(error.to_string().into()), window),
            });
        });
    })
    .detach();
}

/// "Connected to imap.example.org: the login works, 12 folders."
pub(crate) fn connection_ok(host: &str, test: ConnectionTest) -> String {
    let folders = match test.folders {
        1 => "1 folder".to_owned(),
        n => format!("{n} folders"),
    };
    let gmail = if test.gmail { " (Gmail)" } else { "" };
    format!("Connected to {host}{gmail}: the login works, {folders}.")
}

/// What the tenant field says above it.
const TENANT_HINT: &str = "Sign in with Microsoft: organization (tenant). Your tenant ID or \
domain (contoso.com) if your organization's app registration is for its own accounts only; \
\"common\" for any work, school or personal account.";

/// The tenant field's value: blank means `common`.
pub(crate) fn parse_tenant(text: &str) -> Result<MicrosoftTenant, String> {
    if text.trim().is_empty() {
        return Ok(MicrosoftTenant::common());
    }
    MicrosoftTenant::parse(text).map_err(|err| format!("{}.", capitalize(&err.to_string())))
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn add_with_oauth(state: &Shared, provider: ProviderKind, window: &mut Window, cx: &mut App) {
    let (core, input) = {
        let form = state.borrow();
        let email = value(&form.email, cx);
        let name = value(&form.name, cx);
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
        (
            form.core.clone(),
            AddOAuthAccount {
                provider,
                email,
                display_name: (!name.is_empty()).then_some(name),
                tenant,
            },
        )
    };
    let message = format!(
        "Continue in your browser: sign in with {} and allow {PRODUCT_NAME} access…",
        provider.display_name()
    );
    run(
        state,
        &message,
        async move { core.add_oauth_account(input).await },
        window,
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tenant_field_is_checked() {
        assert!(parse_tenant("").unwrap().is_common());
        assert_eq!(
            parse_tenant(" Contoso.com ").unwrap().as_str(),
            "contoso.com"
        );
        let error = parse_tenant("contoso").unwrap_err();
        assert!(
            error.starts_with("\"contoso\" is not a Microsoft tenant"),
            "{error}"
        );
    }

    #[test]
    fn the_server_is_detected_and_can_be_overridden() {
        let gmail = plan("anna@gmail.com", "", "", None);
        assert_eq!(
            gmail.server,
            Some(ImapServer::new("imap.gmail.com", ImapSecurity::Tls))
        );
        assert!(gmail.detected);
        assert_eq!(gmail.oauth, Some(ProviderKind::Google));

        // Microsoft 365 is recognized by the host for custom domains.
        let m365 = plan("bob@contoso.example", "outlook.office365.com", "", None);
        assert_eq!(m365.oauth, Some(ProviderKind::Microsoft));
        assert!(!m365.detected);

        let custom = plan(
            "c@example.org",
            "mail.example.org",
            "1143",
            Some(ImapSecurity::Starttls),
        );
        assert_eq!(
            custom.server,
            Some(ImapServer {
                host: "mail.example.org".into(),
                port: 1143,
                security: ImapSecurity::Starttls,
            })
        );
        assert_eq!(custom.oauth, None);

        let guessed = plan("d@example.net", "", "", Some(ImapSecurity::Starttls));
        assert_eq!(
            guessed.server,
            Some(ImapServer {
                host: "imap.example.net".into(),
                port: 143,
                security: ImapSecurity::Starttls,
            })
        );
        assert!(!guessed.detected);
        assert_eq!(plan("nothing", "", "", None).server, None);
    }

    #[test]
    fn the_smtp_server_is_derived_and_can_be_overridden() {
        let gmail = smtp_plan("anna@gmail.com", Some("imap.gmail.com"), "", "", None).unwrap();
        assert_eq!(
            (gmail.host.as_str(), gmail.port, gmail.security),
            ("smtp.gmail.com", 465, ImapSecurity::Tls)
        );
        let m365 = smtp_plan(
            "bob@contoso.example",
            Some("outlook.office365.com"),
            "",
            "",
            None,
        )
        .unwrap();
        assert_eq!(
            (m365.host.as_str(), m365.port, m365.security),
            ("smtp.office365.com", 587, ImapSecurity::Starttls)
        );
        let own = smtp_plan("c@example.org", Some("imap.example.org"), "", "", None).unwrap();
        assert_eq!((own.host.as_str(), own.port), ("smtp.example.org", 587));
        // Switching the derived server to TLS takes TLS's port.
        let tls = smtp_plan(
            "c@example.org",
            Some("imap.example.org"),
            "",
            "",
            Some(ImapSecurity::Tls),
        )
        .unwrap();
        assert_eq!((tls.port, tls.security), (465, ImapSecurity::Tls));
        let typed = smtp_plan("c@example.org", None, "mail.example.org", "2525", None).unwrap();
        assert_eq!(
            (typed.host.as_str(), typed.port, typed.security),
            ("mail.example.org", 2525, ImapSecurity::Starttls)
        );
        assert_eq!(smtp_plan("nothing", None, "", "", None), None);
    }

    #[test]
    fn a_successful_test_says_what_it_found() {
        assert_eq!(
            connection_ok(
                "imap.example.org",
                ConnectionTest {
                    gmail: false,
                    folders: 12
                }
            ),
            "Connected to imap.example.org: the login works, 12 folders."
        );
        assert_eq!(
            connection_ok(
                "imap.gmail.com",
                ConnectionTest {
                    gmail: true,
                    folders: 1
                }
            ),
            "Connected to imap.gmail.com (Gmail): the login works, 1 folder."
        );
    }
}
