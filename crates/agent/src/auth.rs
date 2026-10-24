//! Signing agents in (PLAN §4.12): API keys, terminal logins and browser links.
//!
//! Agents offer sign-in methods in `initialize` (ACP `authMethods`):
//!
//! - **Terminal** methods (Claude Code's "Claude Subscription" and "Anthropic Console",
//!   pi's "Launch pi in the terminal") are run by the client: the agent's own command
//!   plus the method's arguments, interactively. Maze writes a login script
//!   ([`login_script`]) and opens it in Terminal. The login runs in the same agent
//!   sandbox with the same `HOME` as the agent, so the credentials land where the agent
//!   finds them and nowhere else (Claude Code can't write the macOS keychain from the
//!   sandbox and keeps its login in `.claude/.credentials.json` in the agent's home).
//! - **Agent** methods (Cursor's "Cursor Login", Codex's "ChatGPT") are handled by the
//!   agent in `authenticate`, which usually opens a browser.
//! - **API keys and tokens**: Maze keeps one per agent in its encrypted store and passes
//!   it in the agent's environment ([`key_env`] picks the variable).
//!
//! Sandboxed agents can't start a browser (Seatbelt denies LaunchServices). The
//! [`OPEN_SHIM`] stands in for `open` / `xdg-open` on the agent's `PATH`: it appends
//! `https://` links to the file named by [`OPEN_URLS_ENV`], and Maze opens them
//! ([`UrlInbox`]). Only `https://` links are passed on.

use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use sandbox::{SandboxError, SandboxPolicy};

/// The file the [`OPEN_SHIM`] appends links to.
pub const OPEN_URLS_ENV: &str = "MAZEMAIL_OPEN_URLS";

/// `open` / `xdg-open` for sandboxed agents: hands `https://` links to Maze.
pub const OPEN_SHIM: &str = r#"#!/bin/sh
# Maze: agents run in a sandbox that can't start your browser. This stands in for
# `open`: it hands https links to Maze, which opens them for the agent.
for arg in "$@"; do
  case "$arg" in
    https://*)
      if [ -n "$MAZEMAIL_OPEN_URLS" ]; then
        printf '%s\n' "$arg" >> "$MAZEMAIL_OPEN_URLS"
      fi
      printf 'Open this link in your browser: %s\n' "$arg" >&2
      ;;
  esac
done
exit 0
"#;

/// Writes the [`OPEN_SHIM`] as `open` and `xdg-open` into `bin_dir`; returns the
/// `open` path (for `BROWSER`).
pub fn install_open_shim(bin_dir: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(bin_dir)?;
    for name in ["open", "xdg-open"] {
        let path = bin_dir.join(name);
        std::fs::write(&path, OPEN_SHIM)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(bin_dir.join("open"))
}

/// Reads the links the [`OPEN_SHIM`] wrote, each once.
#[derive(Debug)]
pub struct UrlInbox {
    path: PathBuf,
    offset: u64,
    partial: String,
}

impl UrlInbox {
    /// Reads `path` (it needn't exist yet) from its current end.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let offset = std::fs::metadata(&path).map_or(0, |m| m.len());
        Self {
            path,
            offset,
            partial: String::new(),
        }
    }

    /// The file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// New `https://` links since the last call.
    pub fn take(&mut self) -> Vec<String> {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        let len = file.metadata().map_or(0, |m| m.len());
        if len < self.offset {
            // Truncated: start over.
            self.offset = 0;
            self.partial.clear();
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut bytes = Vec::new();
        // At most 64 KiB per call; links are short.
        if file.take(64 * 1024).read_to_end(&mut bytes).is_err() {
            return Vec::new();
        }
        self.offset += bytes.len() as u64;
        self.partial.push_str(&String::from_utf8_lossy(&bytes));
        let mut out = Vec::new();
        while let Some(end) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=end).collect();
            if let Some(url) = https_link(line.trim()) {
                out.push(url);
            }
        }
        out
    }
}

/// `text` if it is one `https://` link without spaces or control characters.
pub fn https_link(text: &str) -> Option<String> {
    let ok = text.starts_with("https://")
        && text.len() > "https://".len()
        && text.len() <= 8192
        && !text.chars().any(|c| c.is_whitespace() || c.is_control());
    ok.then(|| text.to_owned())
}

/// `https://…` links in a line of agent output.
pub fn links_in(line: &str) -> Vec<String> {
    line.split_whitespace()
        .map(|word| word.trim_end_matches([')', ']', '>', '"', '\'', ',', '.']))
        .map(|word| word.trim_start_matches(['(', '[', '<', '"', '\'']))
        .filter_map(https_link)
        .collect()
}

/// How an agent can be given an API key or token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyAuth {
    /// What to paste, for the card.
    pub help: &'static str,
    /// Placeholder of the input.
    pub placeholder: &'static str,
}

const ANTHROPIC: KeyAuth = KeyAuth {
    help: "Paste an Anthropic API key (sk-ant-api…) or a token from `claude setup-token` \
           (sk-ant-oat…, uses your Claude subscription).",
    placeholder: "sk-ant-…",
};
const OPENAI: KeyAuth = KeyAuth {
    help: "Paste an OpenAI API key.",
    placeholder: "sk-…",
};
const GEMINI: KeyAuth = KeyAuth {
    help: "Paste a Gemini API key from Google AI Studio.",
    placeholder: "Gemini API key",
};
const CURSOR: KeyAuth = KeyAuth {
    help: "Paste a Cursor API key (Cursor dashboard, Integrations).",
    placeholder: "Cursor API key",
};

/// Which key family an agent takes: by id for the built-in agents, else by the
/// provider an `api-key` sign-in method names (`_meta["api-key"].provider`).
fn family(agent_id: &str, provider: Option<&str>) -> Option<&'static str> {
    match crate::catalog::resolve_alias(agent_id) {
        "claude-acp" => Some("anthropic"),
        "codex-acp" => Some("openai"),
        "gemini" => Some("google"),
        "cursor" => Some("cursor"),
        _ => match provider? {
            "anthropic" => Some("anthropic"),
            "openai" => Some("openai"),
            "google" | "gemini" => Some("google"),
            _ => None,
        },
    }
}

/// Whether (and how) the agent takes an API key; `provider` from its sign-in methods.
pub fn key_auth(agent_id: &str, provider: Option<&str>) -> Option<KeyAuth> {
    Some(match family(agent_id, provider)? {
        "anthropic" => ANTHROPIC,
        "openai" => OPENAI,
        "google" => GEMINI,
        _ => CURSOR,
    })
}

/// The environment variable a key is passed in. Claude Code takes OAuth tokens from
/// `claude setup-token` (`sk-ant-oat…`) as `CLAUDE_CODE_OAUTH_TOKEN` and everything else
/// as `ANTHROPIC_API_KEY`.
pub fn key_env(agent_id: &str, provider: Option<&str>, key: &str) -> Option<&'static str> {
    Some(match family(agent_id, provider)? {
        "anthropic" if key.starts_with("sk-ant-oat") => "CLAUDE_CODE_OAUTH_TOKEN",
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "google" => "GEMINI_API_KEY",
        _ => "CURSOR_API_KEY",
    })
}

/// A pasted key that can't be right.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// Nothing pasted.
    #[error("paste a key first")]
    Empty,
    /// Spaces, line breaks or control characters inside.
    #[error("the key contains spaces or line breaks; paste only the key")]
    Malformed,
    /// Longer than any key.
    #[error("that's too long for a key")]
    TooLong,
    /// The agent takes no keys.
    #[error("this agent doesn't take an API key")]
    Unsupported,
}

/// The key without surrounding whitespace, if it can be one.
pub fn clean_key(key: &str) -> Result<&str, KeyError> {
    let key = key.trim();
    if key.is_empty() {
        Err(KeyError::Empty)
    } else if key.len() > 4096 {
        Err(KeyError::TooLong)
    } else if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        Err(KeyError::Malformed)
    } else {
        Ok(key)
    }
}

/// Variables set from the agent's home for some agents. Claude Code gets its config
/// directory explicitly, which also gives its keychain item a name of its own, so it
/// never reads or replaces the login of the Claude Code you use in a terminal.
pub fn home_env(agent_id: &str, agent_home: &Path) -> Vec<(String, String)> {
    match crate::catalog::resolve_alias(agent_id) {
        "claude-acp" => vec![(
            "CLAUDE_CONFIG_DIR".to_owned(),
            agent_home.join(".claude").display().to_string(),
        )],
        _ => Vec::new(),
    }
}

/// A login to run in a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginCommand {
    /// Shown at the top ("Claude Code: Claude Subscription").
    pub title: String,
    /// Program.
    pub program: PathBuf,
    /// Arguments.
    pub args: Vec<String>,
    /// The complete environment (nothing else is inherited).
    pub env: BTreeMap<String, String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// The sandbox; `None` when the agent sandbox is off.
    pub policy: Option<SandboxPolicy>,
    /// The file the open shim writes links to.
    pub urls: PathBuf,
}

/// Single-quotes `text` for `sh`.
pub fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The login as a shell script for a terminal (`.command` on macOS).
///
/// The login runs with exactly `command.env` (`env -i`) and, unless the sandbox is
/// off, in the agent sandbox. Next to it a watcher opens the `https://` links the
/// login's `open` hands over, since the sandbox can't start a browser. The script
/// holds no secrets: no session token, no API key.
pub fn login_script(command: &LoginCommand) -> Result<String, SandboxError> {
    let (program, args) = match &command.policy {
        Some(policy) => {
            let wrapped =
                sandbox::wrap(&command.program, &command.args, Some(&command.cwd), policy)?;
            (wrapped.program, wrapped.args)
        }
        None => (command.program.clone(), command.args.clone()),
    };
    let open = if cfg!(target_os = "macos") {
        "/usr/bin/open"
    } else {
        "xdg-open"
    };
    let mut line = String::from("/usr/bin/env -i");
    for (name, value) in &command.env {
        if name == "TERM" {
            continue;
        }
        line.push(' ');
        line.push_str(&sh_quote(&format!("{name}={value}")));
    }
    line.push_str(" \"TERM=${TERM:-xterm-256color}\" ");
    line.push_str(&sh_quote(&program.display().to_string()));
    for arg in &args {
        line.push(' ');
        line.push_str(&sh_quote(arg));
    }
    let title = command.title.replace(['\n', '\r'], " ");
    Ok(format!(
        r#"#!/bin/sh
# {title}
# Written by Maze. It runs the agent's own login with the agent's home in Maze, in
# Maze's agent sandbox, and opens the sign-in links it shows in your browser.
urls={urls}
: > "$urls"
(
  seen=0
  while :; do
    count=$(wc -l < "$urls" 2>/dev/null | tr -d ' ')
    if [ -n "$count" ] && [ "$count" -gt "$seen" ]; then
      tail -n "+$((seen + 1))" "$urls" | head -n "$((count - seen))" | while IFS= read -r url; do
        case "$url" in
          https://*) {open} "$url" ;;
        esac
      done
      seen=$count
    fi
    sleep 1
  done
) &
watcher=$!
trap 'kill "$watcher" 2>/dev/null' EXIT INT TERM
printf '%s\n\n' {banner}
cd {cwd} || exit 1
{line}
status=$?
kill "$watcher" 2>/dev/null
rm -f "$urls"
echo
if [ "$status" -eq 0 ]; then
  echo 'Done. Go back to Maze and click "Done, retry". You can close this window.'
else
  echo "The login ended with status $status. Go back to Maze to try again or paste a key."
fi
"#,
        urls = sh_quote(&command.urls.display().to_string()),
        banner = sh_quote(&format!(
            "{title}: signing in for Maze. Follow the steps below; links open in your \
             browser."
        )),
        cwd = sh_quote(&command.cwd.display().to_string()),
    ))
}

/// Writes `script` to `path` (mode 0700, replacing an older one).
pub fn write_script(path: &Path, script: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let _ = std::fs::remove_file(path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(path)?;
    std::io::Write::write_all(&mut file, script.as_bytes())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn claude_keys_pick_their_variable_by_prefix() {
        assert_eq!(
            key_env("claude-acp", None, "sk-ant-oat01-abc"),
            Some("CLAUDE_CODE_OAUTH_TOKEN")
        );
        assert_eq!(
            key_env("claude", None, "sk-ant-api03-abc"),
            Some("ANTHROPIC_API_KEY")
        );
        // Anything else goes to the API key variable; Claude Code says if it's wrong.
        assert_eq!(
            key_env("claude-acp", None, "something"),
            Some("ANTHROPIC_API_KEY")
        );
        assert_eq!(
            key_env("codex-acp", None, "sk-proj-1"),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(key_env("gemini", None, "AIza"), Some("GEMINI_API_KEY"));
        assert_eq!(key_env("cursor", None, "key"), Some("CURSOR_API_KEY"));
        assert_eq!(key_env("pi-acp", None, "key"), None);
        // Other agents by the provider of their api-key method.
        assert_eq!(
            key_env("kilo", Some("openai"), "sk-1"),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(key_env("kilo", Some("acme"), "sk-1"), None);
        assert!(key_auth("claude-acp", None).is_some());
        assert!(key_auth("pi-acp", None).is_none());
    }

    #[test]
    fn pasted_keys_are_trimmed_and_checked() {
        assert_eq!(clean_key("  sk-ant-api03-x\n"), Ok("sk-ant-api03-x"));
        assert_eq!(clean_key("   "), Err(KeyError::Empty));
        assert_eq!(clean_key("sk-ant one"), Err(KeyError::Malformed));
        assert_eq!(clean_key(&"x".repeat(5000)), Err(KeyError::TooLong));
    }

    #[test]
    fn claude_gets_its_own_config_dir() {
        assert_eq!(
            home_env("claude-acp", Path::new("/d/agent-home/claude-acp")),
            vec![(
                "CLAUDE_CONFIG_DIR".to_owned(),
                "/d/agent-home/claude-acp/.claude".to_owned()
            )]
        );
        assert!(home_env("cursor", Path::new("/h")).is_empty());
    }

    #[test]
    fn links_are_found_and_limited_to_https() {
        assert_eq!(
            links_in("Open (https://cursor.com/loginDeepControl?x=1). Then wait"),
            vec!["https://cursor.com/loginDeepControl?x=1"]
        );
        assert!(links_in("http://example.com file:///etc/passwd").is_empty());
        assert_eq!(https_link("https://a b"), None);
        assert_eq!(https_link("https://"), None);
    }

    #[test]
    fn the_inbox_reads_each_link_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("urls");
        std::fs::write(&path, "https://old.example\n").unwrap();
        let mut inbox = UrlInbox::new(&path);
        assert!(inbox.take().is_empty());
        let append = |text: &str| {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(text.as_bytes()).unwrap();
        };
        append("https://one.example/a\njavascript:alert(1)\nhttps://two");
        assert_eq!(inbox.take(), vec!["https://one.example/a"]);
        append(".example\n");
        assert_eq!(inbox.take(), vec!["https://two.example"]);
        assert!(inbox.take().is_empty());
    }

    #[test]
    fn the_open_shim_hands_links_over() {
        let dir = tempfile::tempdir().unwrap();
        let open = install_open_shim(&dir.path().join("bin")).unwrap();
        let urls = dir.path().join("urls");
        let output = std::process::Command::new(&open)
            .args(["-a", "Safari", "https://login.example/x?y=1", "/etc/hosts"])
            .env_clear()
            .env(OPEN_URLS_ENV, &urls)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            std::fs::read_to_string(&urls).unwrap(),
            "https://login.example/x?y=1\n"
        );
        assert!(dir.path().join("bin/xdg-open").exists());
    }

    #[test]
    fn quoting_survives_quotes() {
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf %s {}", sh_quote("a 'b' $HOME `x`")))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "a 'b' $HOME `x`");
    }

    fn command(policy: Option<SandboxPolicy>) -> LoginCommand {
        LoginCommand {
            title: "Claude Code: Claude Subscription".to_owned(),
            program: PathBuf::from("/data/runtimes/node/bin/node"),
            args: vec![
                "/data/agents/claude-acp/1.0.0/index.js".to_owned(),
                "--cli".to_owned(),
                "auth".to_owned(),
                "login".to_owned(),
                "--claudeai".to_owned(),
            ],
            env: BTreeMap::from([
                ("HOME".to_owned(), "/data/agent-home/claude-acp".to_owned()),
                (
                    "PATH".to_owned(),
                    "/data/login/bin:/usr/bin:/bin".to_owned(),
                ),
                (
                    OPEN_URLS_ENV.to_owned(),
                    "/data/agent-home/claude-acp/.mazemail-open-urls".to_owned(),
                ),
            ]),
            cwd: PathBuf::from("/data/agent-home/claude-acp"),
            policy,
            urls: PathBuf::from("/data/agent-home/claude-acp/.mazemail-open-urls"),
        }
    }

    #[test]
    fn the_login_script_runs_the_command_with_only_its_environment() {
        let script = login_script(&command(None)).unwrap();
        assert!(script.starts_with("#!/bin/sh\n# Claude Code: Claude Subscription\n"));
        assert!(script.contains(
            "/usr/bin/env -i 'HOME=/data/agent-home/claude-acp' \
             'MAZEMAIL_OPEN_URLS=/data/agent-home/claude-acp/.mazemail-open-urls' \
             'PATH=/data/login/bin:/usr/bin:/bin' \"TERM=${TERM:-xterm-256color}\" \
             '/data/runtimes/node/bin/node' '/data/agents/claude-acp/1.0.0/index.js' \
             '--cli' 'auth' 'login' '--claudeai'\n"
        ));
        assert!(script.contains("cd '/data/agent-home/claude-acp' || exit 1"));
        assert!(script.contains("urls='/data/agent-home/claude-acp/.mazemail-open-urls'"));
        assert!(script.contains("https://*)"));
        // `sh -n`: the script parses.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("login/claude-acp.command");
        write_script(&path, &script).unwrap();
        let check = std::process::Command::new("/bin/sh")
            .arg("-n")
            .arg(&path)
            .status()
            .unwrap();
        assert!(check.success());
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_login_script_runs_in_the_sandbox() {
        let policy = SandboxPolicy {
            hide: vec![PathBuf::from("/Users/someone")],
            write: vec![PathBuf::from("/data/agent-home/claude-acp")],
            ..SandboxPolicy::default()
        };
        let script = login_script(&command(Some(policy))).unwrap();
        assert!(script.contains("\"TERM=${TERM:-xterm-256color}\" '/usr/bin/sandbox-exec' '-p' "));
        assert!(script.contains("'--' '/data/runtimes/node/bin/node'"));
        assert!(script.contains("(deny file-read* (subpath \"/Users/someone\"))"));
    }

    #[test]
    fn the_login_script_opens_links_from_the_login() {
        // Run the script with a fake program that "opens" a link through the shim, and
        // an `open` / `xdg-open` that records instead of starting a browser.
        let dir = tempfile::tempdir().unwrap();
        let shim_dir = dir.path().join("shim");
        install_open_shim(&shim_dir).unwrap();
        let urls = dir.path().join("urls");
        let opened = dir.path().join("opened");
        let login = dir.path().join("login.sh");
        std::fs::write(
            &login,
            "#!/bin/sh\nopen https://claude.example/oauth?code=1\nsleep 2\nexit 0\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&login, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut command = command(None);
        command.program = login;
        command.args = Vec::new();
        command.cwd = dir.path().to_owned();
        command.urls = urls.clone();
        command.env = BTreeMap::from([
            (
                "PATH".to_owned(),
                format!("{}:/usr/bin:/bin", shim_dir.display()),
            ),
            (OPEN_URLS_ENV.to_owned(), urls.display().to_string()),
        ]);
        let script = login_script(&command).unwrap();
        // Swap the real opener for a recorder.
        let recorder = format!(
            "printf '%s\\n' \"$url\" >> {}",
            sh_quote(&opened.display().to_string())
        );
        let script = script
            .replace("/usr/bin/open \"$url\"", &recorder)
            .replace("xdg-open \"$url\"", &recorder);
        let path = dir.path().join("run.sh");
        std::fs::write(&path, script).unwrap();
        let output = std::process::Command::new("/bin/sh")
            .arg(&path)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Done. Go back to Maze"), "{stdout}");
        assert_eq!(
            std::fs::read_to_string(&opened).unwrap().trim(),
            "https://claude.example/oauth?code=1"
        );
        assert!(!urls.exists());
    }
}
