//! The ACP terminal host (PLAN §4.12, CONCEPT §10.2).
//!
//! Maze advertises the `terminal` capability, so agents run shell commands through Maze
//! and each one shows up in the agent column. Every command runs in the session's
//! sandbox.
//!
//! - A **single, plain `mazemail …` invocation** runs at once, in the scratch dir, with
//!   the bundled CLI and no shell in between ([`classify`]).
//! - **Anything else** (other programs, pipes, `;`, `$(…)`, redirections, even after a
//!   `mazemail`) needs the user's approval: allow once, for this session (that program
//!   from then on), or deny. Approved commands stay sandboxed.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use sandbox::SandboxPolicy;
use tokio::io::AsyncReadExt as _;
use tokio::sync::watch;

use crate::process::{LaunchSpec, ProcessGroupGuard, SpawnError, command};

/// Output kept per terminal when the agent sets no limit.
pub const DEFAULT_OUTPUT_LIMIT: usize = 1024 * 1024;

/// How a requested command is treated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandClass {
    /// A plain `mazemail` call: runs automatically with these arguments.
    Mazemail(Vec<String>),
    /// Needs approval; the program name decides "allow for this session".
    Other {
        /// The program's name (`git`, `sh`, …).
        program: String,
    },
}

/// Classifies an ACP `terminal/create` request (`command` plus `args`).
pub fn classify(command: &str, args: &[String]) -> CommandClass {
    let other = |program: &str| CommandClass::Other {
        program: program_name(program),
    };
    if args.is_empty() {
        return match split_plain(command) {
            Some(words) if words.first().is_some_and(|w| is_mazemail(w)) => {
                CommandClass::Mazemail(words[1..].to_vec())
            }
            Some(words) => other(words.first().map_or(command, String::as_str)),
            None => other(first_word(command)),
        };
    }
    if is_mazemail(command) {
        return CommandClass::Mazemail(args.to_vec());
    }
    // `sh -c "mazemail search …"` is what many agents send.
    let shell = matches!(
        program_name(command).as_str(),
        "sh" | "bash" | "zsh" | "dash"
    );
    if shell
        && let [flag, line] = args
        && matches!(flag.as_str(), "-c" | "-lc")
    {
        return match classify(line, &[]) {
            CommandClass::Mazemail(words) => CommandClass::Mazemail(words),
            CommandClass::Other { program } => CommandClass::Other { program },
        };
    }
    other(command)
}

fn is_mazemail(word: &str) -> bool {
    word == "mazemail"
}

fn program_name(program: &str) -> String {
    program.rsplit('/').next().unwrap_or(program).to_owned()
}

fn first_word(line: &str) -> &str {
    line.split_whitespace().next().unwrap_or(line)
}

/// Splits a command line into words if it is a plain command: words separated by
/// spaces, with '…' and "…" quoting and backslash escapes, and none of the shell's
/// operators or expansions. `None` for anything else.
pub fn split_plain(line: &str) -> Option<Vec<String>> {
    const SPECIAL: &[char] = &[
        ';', '&', '|', '<', '>', '(', ')', '$', '`', '!', '{', '}', '*', '?', '[', ']', '~', '#',
        '\n', '\r',
    ];
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '$' | '`' => return None,
                        '\\' => match chars.next()? {
                            c @ ('"' | '\\') => word.push(c),
                            c => {
                                word.push('\\');
                                word.push(c);
                            }
                        },
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                let next = chars.next()?;
                if next == '\n' {
                    return None;
                }
                word.push(next);
            }
            c if SPECIAL.contains(&c) => return None,
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    (!words.is_empty()).then_some(words)
}

/// How a command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitInfo {
    /// Exit code.
    pub code: Option<u32>,
    /// Signal name.
    pub signal: Option<String>,
}

impl ExitInfo {
    fn from_status(status: std::process::ExitStatus) -> Self {
        use std::os::unix::process::ExitStatusExt as _;
        Self {
            code: status.code().and_then(|c| u32::try_from(c).ok()),
            signal: status.signal().map(signal_name),
        }
    }
}

fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP".to_owned(),
        2 => "SIGINT".to_owned(),
        9 => "SIGKILL".to_owned(),
        15 => "SIGTERM".to_owned(),
        other => format!("signal {other}"),
    }
}

#[derive(Debug)]
struct OutputBuffer {
    text: String,
    limit: usize,
    truncated: bool,
}

impl OutputBuffer {
    fn push(&mut self, bytes: &[u8]) {
        self.text.push_str(&String::from_utf8_lossy(bytes));
        if self.text.len() > self.limit {
            let mut cut = self.text.len() - self.limit;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
            self.truncated = true;
        }
    }
}

#[derive(Debug)]
struct Terminal {
    output: Mutex<OutputBuffer>,
    exit: watch::Receiver<Option<ExitInfo>>,
    guard: Mutex<ProcessGroupGuard>,
}

/// The terminals of one session.
#[derive(Debug, Default)]
pub struct TerminalHost {
    terminals: Mutex<HashMap<String, Arc<Terminal>>>,
    next: AtomicU64,
}

/// A snapshot of a terminal's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalOutput {
    /// Output so far (the end, if truncated).
    pub output: String,
    /// Whether the start was dropped.
    pub truncated: bool,
    /// Exit, if it has ended.
    pub exit: Option<ExitInfo>,
}

impl TerminalHost {
    /// An empty host.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Terminal>>> {
        self.terminals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn get(&self, id: &str) -> Option<Arc<Terminal>> {
        self.lock().get(id).cloned()
    }

    /// Starts `spec` under `policy` and returns the terminal id.
    pub fn spawn(
        &self,
        spec: &LaunchSpec,
        policy: Option<&SandboxPolicy>,
        output_limit: Option<u64>,
    ) -> Result<String, SpawnError> {
        let mut command = command(spec, policy)?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|source| SpawnError::Spawn {
            program: spec.program.display().to_string(),
            source,
        })?;
        let id = format!("term-{}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
        let limit = output_limit
            .and_then(|l| usize::try_from(l).ok())
            .unwrap_or(DEFAULT_OUTPUT_LIMIT);
        let (exit_tx, exit_rx) = watch::channel(None);
        let terminal = Arc::new(Terminal {
            output: Mutex::new(OutputBuffer {
                text: String::new(),
                limit,
                truncated: false,
            }),
            exit: exit_rx,
            guard: Mutex::new(ProcessGroupGuard::new(child.id())),
        });
        let readers: Vec<_> = [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        ]
        .into_iter()
        .flatten()
        .map(|mut stream| {
            let terminal = Arc::clone(&terminal);
            tokio::spawn(async move {
                let mut buffer = [0u8; 8192];
                while let Ok(read) = stream.read(&mut buffer).await {
                    if read == 0 {
                        break;
                    }
                    terminal
                        .output
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(&buffer[..read]);
                }
            })
        })
        .collect();
        tokio::spawn(async move {
            let status = child.wait().await;
            for reader in readers {
                let _ = reader.await;
            }
            let info = match status {
                Ok(status) => ExitInfo::from_status(status),
                Err(_) => ExitInfo {
                    code: None,
                    signal: None,
                },
            };
            let _ = exit_tx.send(Some(info));
        });
        self.lock().insert(id.clone(), terminal);
        Ok(id)
    }

    /// The output so far.
    pub fn output(&self, id: &str) -> Option<TerminalOutput> {
        let terminal = self.get(id)?;
        let output = terminal
            .output
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let exit = terminal.exit.borrow().clone();
        Some(TerminalOutput {
            output: output.text.clone(),
            truncated: output.truncated,
            exit,
        })
    }

    /// Waits until the command ends.
    pub async fn wait(&self, id: &str) -> Option<ExitInfo> {
        let mut exit = self.get(id)?.exit.clone();
        let result = exit.wait_for(Option::is_some).await;
        result.ok().and_then(|value| value.clone())
    }

    /// Kills the command (its whole process group).
    pub fn kill(&self, id: &str) -> bool {
        match self.get(id) {
            Some(terminal) => {
                terminal
                    .guard
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .kill();
                true
            }
            None => false,
        }
    }

    /// Kills the command if it still runs and forgets the terminal.
    pub fn release(&self, id: &str) -> bool {
        let removed = self.lock().remove(id);
        match removed {
            Some(terminal) => {
                terminal
                    .guard
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .kill();
                true
            }
            None => false,
        }
    }

    /// Kills every command (session end).
    pub fn kill_all(&self) {
        for (_, terminal) in self.lock().drain() {
            terminal
                .guard
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;

    fn words(line: &str) -> Option<Vec<String>> {
        split_plain(line)
    }

    #[test]
    fn plain_mazemail_calls_run_automatically() {
        assert_eq!(
            classify("mazemail search 'from:anna has:attachment' --json", &[]),
            CommandClass::Mazemail(vec![
                "search".into(),
                "from:anna has:attachment".into(),
                "--json".into()
            ])
        );
        assert_eq!(
            classify("mazemail", &["show".into(), "t_19bd".into()]),
            CommandClass::Mazemail(vec!["show".into(), "t_19bd".into()])
        );
        assert_eq!(
            classify("bash", &["-c".into(), "mazemail folders".into()]),
            CommandClass::Mazemail(vec!["folders".into()])
        );
    }

    #[test]
    fn everything_else_needs_approval() {
        for line in [
            "mazemail search x; cat ~/.ssh/id_rsa",
            "mazemail search x | jq .",
            "mazemail search $(whoami)",
            "mazemail show t_1 > out.md",
            "mazemail search \"$HOME\"",
            "mazemail search `id`",
            "mazemail search x && curl evil.example",
            "git status",
            "/usr/local/bin/mazemail-fake search",
        ] {
            assert!(
                matches!(classify(line, &[]), CommandClass::Other { .. }),
                "{line}"
            );
        }
        assert_eq!(
            classify("git status", &[]),
            CommandClass::Other {
                program: "git".into()
            }
        );
        assert_eq!(
            classify("/usr/bin/git", &["log".into()]),
            CommandClass::Other {
                program: "git".into()
            }
        );
        assert_eq!(
            classify("sh", &["-c".into(), "ls | wc -l".into()]),
            CommandClass::Other {
                program: "ls".into()
            }
        );
    }

    #[test]
    fn quoting() {
        assert_eq!(
            words(r#"a "b c" 'd e' f\ g"#),
            Some(vec!["a".into(), "b c".into(), "d e".into(), "f g".into()])
        );
        assert_eq!(words(r#"a "b\"c""#), Some(vec!["a".into(), "b\"c".into()]));
        assert_eq!(words("a 'unterminated"), None);
        assert_eq!(words("   "), None);
    }

    fn spec(script: &str, dir: &std::path::Path) -> LaunchSpec {
        LaunchSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            env: BTreeMap::new(),
            cwd: dir.to_owned(),
        }
    }

    #[tokio::test]
    async fn output_wait_and_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let host = TerminalHost::new();
        let id = host
            .spawn(
                &spec("printf 0123456789; exit 3", dir.path()),
                None,
                Some(4),
            )
            .unwrap();
        let exit = host.wait(&id).await.unwrap();
        assert_eq!(exit.code, Some(3));
        let output = host.output(&id).unwrap();
        assert_eq!(output.output, "6789");
        assert!(output.truncated);
        assert!(host.release(&id));
        assert!(host.output(&id).is_none());
    }

    #[tokio::test]
    async fn kill_ends_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let host = TerminalHost::new();
        let id = host
            .spawn(&spec("sleep 30", dir.path()), None, None)
            .unwrap();
        assert!(host.kill(&id));
        let exit = tokio::time::timeout(std::time::Duration::from_secs(5), host.wait(&id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exit.signal.as_deref(), Some("SIGKILL"));
    }
}
