//! Approvals: `mazemail login` and commands with [`Permission::Ask`](crate::Permission)
//! wait for the user (PLAN §4.11, CONCEPT §10.5).
//!
//! The GUI implements [`Approver`] with a dialog (optionally behind Touch ID).
//! [`TerminalApprover`] asks on the server's controlling terminal for
//! `mazemail --headless`, and [`DenyAllApprover`] is the safe default when nobody can
//! answer. The server applies its own timeout around [`Approver::approve`].

use std::fmt;
use std::io::{self, BufRead, Write};
use std::time::Duration;

use async_trait::async_trait;
use mazemail_types::{PRODUCT_NAME, Sensitive};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};

use crate::peer::ProcessInfo;
use crate::token::{Grant, Profile, TokenId, TokenScope, Ttl};

/// What the user is asked to approve.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    /// The requesting process: path, pid and parent, from the socket's peer
    /// credentials.
    pub process: ProcessInfo,
    /// The token the requester already holds, if any. Holding a token never skips the
    /// dialog: tokens can't mint tokens.
    pub requester_token: Option<TokenId>,
    /// For a login: the requested profile. For a command: the token's profile.
    pub profile: Profile,
    /// For a login: the requested scope. For a command: the token's scope.
    pub scope: TokenScope,
    /// For a login: the requested lifetime. For a command: the token's lifetime.
    pub lifetime: Option<Duration>,
    /// The command's wire name (`login`, `send`, `rule.add`, …).
    pub command: String,
    /// What kind of approval this is.
    pub kind: ApprovalKind,
}

/// The two kinds of approval.
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalKind {
    /// A new token (`mazemail login`, `mazemail token create`).
    Login {
        /// The requested label.
        label: Option<String>,
        /// Whether `send` should be enabled for the token.
        allow_send: bool,
    },
    /// One command run with an existing token.
    Command {
        /// The command's arguments, for the dialog (may contain mail data).
        args: Sensitive<Value>,
        /// Whether the dialog may offer "allow for this session". Always `false` for
        /// `send`, which is never auto-approved.
        rememberable: bool,
    },
}

impl ApprovalRequest {
    /// The rights requested, as a grant ("approve as asked").
    pub fn requested_grant(&self) -> Grant {
        Grant {
            profile: self.profile,
            scope: self.scope.clone(),
            lifetime: self.lifetime,
            allow_send: matches!(
                self.kind,
                ApprovalKind::Login {
                    allow_send: true,
                    ..
                }
            ),
        }
    }

    /// A one-paragraph description for prompts, without mail content.
    pub fn describe(&self) -> String {
        let who = self.process.describe();
        let parent = self
            .process
            .parent
            .as_ref()
            .map(|parent| {
                let exe = parent
                    .exe
                    .as_ref()
                    .map_or_else(|| "unknown program".to_owned(), |e| e.display().to_string());
                format!(", started by {exe} (pid {})", parent.pid)
            })
            .unwrap_or_default();
        let holder = self
            .requester_token
            .as_ref()
            .map(|id| format!(" It already holds token {id}."))
            .unwrap_or_default();
        match &self.kind {
            ApprovalKind::Login { label, allow_send } => {
                let lifetime = self.lifetime.map_or_else(
                    || "without expiry".to_owned(),
                    |d| format!("for {}", Ttl::new(d)),
                );
                let label = label
                    .as_ref()
                    .map(|label| format!(" labeled {label:?}"))
                    .unwrap_or_default();
                let send = if *allow_send {
                    ", with send enabled"
                } else {
                    ""
                };
                format!(
                    "{who}{parent} asks to sign in to {PRODUCT_NAME} with profile {}, {}, {lifetime}{label}{send}.{holder}",
                    self.profile,
                    self.scope.describe()
                )
            }
            ApprovalKind::Command { .. } => format!(
                "{who}{parent} asks to run `mazemail {}` with profile {}.{holder}",
                self.command.replace('.', " "),
                self.profile
            ),
        }
    }
}

/// The user's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Approved. For a login, the grant is what the user approved (possibly narrower
    /// than requested) and exactly what the new token gets. For a command it is
    /// ignored.
    Approve(Grant),
    /// Denied.
    Deny,
}

/// Asks the user.
#[async_trait]
pub trait Approver: Send + Sync {
    /// Shows `request` and waits for the answer. The server cancels the future when its
    /// approval timeout expires.
    async fn approve(&self, request: ApprovalRequest) -> Decision;
}

/// Denies everything. The default when no one can answer.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyAllApprover;

#[async_trait]
impl Approver for DenyAllApprover {
    async fn approve(&self, _request: ApprovalRequest) -> Decision {
        Decision::Deny
    }
}

/// Asks on a terminal, for `mazemail --headless`: prints the request and reads `y` or
/// `n` (anything else, EOF or a timeout denies).
///
/// Input is read by a dedicated thread, so a prompt that times out doesn't leave a
/// blocked read behind; answers typed after a timeout are discarded before the next
/// prompt. Prompts are shown one at a time.
pub struct TerminalApprover {
    io: Mutex<TerminalIo>,
}

struct TerminalIo {
    lines: mpsc::Receiver<String>,
    output: Box<dyn Write + Send>,
}

impl fmt::Debug for TerminalApprover {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TerminalApprover").finish_non_exhaustive()
    }
}

impl TerminalApprover {
    /// Prompts on the process's controlling terminal (`/dev/tty`).
    pub fn controlling_tty() -> io::Result<Self> {
        let input = std::fs::File::open("/dev/tty")?;
        let output = std::fs::OpenOptions::new().write(true).open("/dev/tty")?;
        Ok(Self::new(io::BufReader::new(input), output))
    }

    /// Prompts on `output` and reads answers from `input`.
    pub fn new(input: impl BufRead + Send + 'static, output: impl Write + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel(8);
        let spawned = std::thread::Builder::new()
            .name("mazemail-approval-tty".into())
            .spawn(move || {
                for line in input.lines() {
                    let Ok(line) = line else { break };
                    if tx.blocking_send(line).is_err() {
                        break;
                    }
                }
            });
        if let Err(error) = spawned {
            // Without the reader every prompt sees EOF and denies, which is safe.
            tracing::warn!(%error, "cannot start the approval reader thread");
        }
        Self {
            io: Mutex::new(TerminalIo {
                lines: rx,
                output: Box::new(output),
            }),
        }
    }
}

#[async_trait]
impl Approver for TerminalApprover {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        let mut io = self.io.lock().await;
        // Drop answers meant for an earlier prompt that timed out.
        while io.lines.try_recv().is_ok() {}
        let prompt = format!("\n{PRODUCT_NAME}: {}\nAllow? [y/N] ", request.describe());
        if io
            .output
            .write_all(prompt.as_bytes())
            .and_then(|()| io.output.flush())
            .is_err()
        {
            return Decision::Deny;
        }
        let answer = io.lines.recv().await;
        let approved = answer
            .is_some_and(|line| matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"));
        let verdict = if approved { "approved\n" } else { "denied\n" };
        // The answer stands even if the confirmation can't be written.
        let _ = io.output.write_all(verdict.as_bytes());
        let _ = io.output.flush();
        if approved {
            Decision::Approve(request.requested_grant())
        } else {
            Decision::Deny
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use super::*;
    use crate::peer::ParentProcess;

    #[derive(Clone, Default)]
    struct SharedOutput(Arc<StdMutex<Vec<u8>>>);

    impl Write for SharedOutput {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SharedOutput {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn login_request() -> ApprovalRequest {
        ApprovalRequest {
            process: ProcessInfo {
                pid: Some(42),
                exe: Some("/usr/local/bin/mazemail".into()),
                parent: Some(ParentProcess {
                    pid: 7,
                    exe: Some("/bin/zsh".into()),
                }),
            },
            requester_token: None,
            profile: Profile::ReadOnly,
            scope: TokenScope::all(),
            lifetime: Some(Duration::from_secs(8 * 3600)),
            command: "login".into(),
            kind: ApprovalKind::Login {
                label: Some("claude".into()),
                allow_send: false,
            },
        }
    }

    #[test]
    fn description() {
        assert_eq!(
            login_request().describe(),
            "/usr/local/bin/mazemail (pid 42), started by /bin/zsh (pid 7) asks to sign in to \
             Maze with profile read-only, all accounts, for 8h labeled \"claude\"."
        );
        let mut command = login_request();
        command.command = "rule.add".into();
        command.requester_token = Some("tk_abc".parse().unwrap());
        command.kind = ApprovalKind::Command {
            args: Sensitive::new(serde_json::json!({ "when": "from:secret@example.com" })),
            rememberable: true,
        };
        let text = command.describe();
        assert!(text.contains("`mazemail rule add`"), "{text}");
        assert!(text.contains("already holds token tk_abc"), "{text}");
        assert!(!text.contains("secret@example.com"), "{text}");
    }

    #[tokio::test]
    async fn deny_all() {
        assert_eq!(
            DenyAllApprover.approve(login_request()).await,
            Decision::Deny
        );
    }

    #[tokio::test]
    async fn terminal_yes_no_and_eof() {
        let output = SharedOutput::default();
        let approver = TerminalApprover::new(io::Cursor::new("y\nno\n"), output.clone());
        let request = login_request();
        assert_eq!(
            approver.approve(request.clone()).await,
            Decision::Approve(request.requested_grant())
        );
        assert_eq!(approver.approve(request.clone()).await, Decision::Deny);
        // EOF denies.
        assert_eq!(approver.approve(request).await, Decision::Deny);
        let text = output.text();
        assert_eq!(text.matches("Allow? [y/N]").count(), 3);
        assert!(text.contains("approved"));
        assert!(text.contains("denied"));
    }

    #[tokio::test]
    async fn stale_answers_are_discarded() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let approver = TerminalApprover::new(io::BufReader::new(reader), SharedOutput::default());
        // Times out without an answer.
        let timed_out =
            tokio::time::timeout(Duration::from_millis(50), approver.approve(login_request()))
                .await;
        assert!(timed_out.is_err());
        // The late answer arrives before the next prompt and must not approve it.
        writer.write_all(b"y\n").unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let next = tokio::spawn(async move { approver.approve(login_request()).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        writer.write_all(b"n\n").unwrap();
        assert_eq!(next.await.unwrap(), Decision::Deny);
    }
}
