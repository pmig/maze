//! Opening links and terminal logins outside Maze, behind a trait so tests never start
//! a browser or Terminal.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use gpui::App;

/// Opens links in the browser and login scripts in a terminal.
pub trait Launcher: Send + Sync + 'static {
    /// Opens an `https://` link in the default browser.
    fn open_url(&self, url: &str, cx: &mut App);

    /// Opens a login script (from [`agent::AgentManager::terminal_login`]) in a
    /// terminal window.
    fn open_terminal(&self, script: &Path) -> std::io::Result<()>;
}

/// The real thing: the system browser, and Terminal on macOS (elsewhere the first
/// terminal [`terminal_command`] finds).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemLauncher;

impl Launcher for SystemLauncher {
    fn open_url(&self, url: &str, cx: &mut App) {
        cx.open_url(url);
    }

    fn open_terminal(&self, script: &Path) -> std::io::Result<()> {
        let mut command = if cfg!(target_os = "macos") {
            let mut command = std::process::Command::new("/usr/bin/open");
            command.arg("-a").arg("Terminal").arg(script);
            command
        } else {
            let terminal = std::env::var("TERMINAL").ok();
            let (program, args) = terminal_command(script, terminal.as_deref(), &on_path)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "no terminal found: set TERMINAL to yours, or install \
                         xdg-terminal-exec",
                    )
                })?;
            let mut command = std::process::Command::new(program);
            command.args(args);
            command
        };
        let mut child = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        // `open` returns at once; reap it so it doesn't linger as a zombie.
        std::thread::spawn(move || child.wait());
        Ok(())
    }
}

/// How common terminals run a program: the flag before it, if any.
const TERMINALS: &[(&str, &[&str])] = &[
    // The freedesktop way to start the user's default terminal.
    ("xdg-terminal-exec", &[]),
    // Debian's and Ubuntu's alternative.
    ("x-terminal-emulator", &["-e"]),
    ("ghostty", &["-e"]),
    ("kitty", &[]),
    ("foot", &[]),
    ("alacritty", &["-e"]),
    ("wezterm", &["start", "--"]),
    ("gnome-terminal", &["--"]),
    ("ptyxis", &["--"]),
    ("konsole", &["-e"]),
    ("xfce4-terminal", &["-x"]),
    ("tilix", &["-e"]),
    ("xterm", &["-e"]),
];

/// The program and arguments that open `script` in a terminal: `terminal` (the
/// `TERMINAL` variable, a program name or path) if set and found, else the first of
/// [`TERMINALS`] that `found` says is installed. `None` if there is none.
pub(crate) fn terminal_command(
    script: &Path,
    terminal: Option<&str>,
    found: &dyn Fn(&str) -> Option<PathBuf>,
) -> Option<(PathBuf, Vec<OsString>)> {
    let with = |program: PathBuf, flags: &[&str]| {
        let mut args: Vec<OsString> = flags.iter().map(OsString::from).collect();
        args.push(script.as_os_str().to_owned());
        (program, args)
    };
    if let Some(terminal) = terminal.map(str::trim).filter(|t| !t.is_empty())
        && let Some(program) = found(terminal)
    {
        let name = Path::new(terminal)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(terminal);
        let flags = TERMINALS
            .iter()
            .find(|(known, _)| *known == name)
            .map_or(&["-e"][..], |(_, flags)| *flags);
        return Some(with(program, flags));
    }
    TERMINALS
        .iter()
        .find_map(|(name, flags)| found(name).map(|program| with(program, flags)))
}

/// `program` as an absolute path: itself if it is a path to a file, else the first
/// match on `PATH`.
fn on_path(program: &str) -> Option<PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return path.is_file().then(|| path.to_owned());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(names: &'static [&'static str]) -> impl Fn(&str) -> Option<PathBuf> {
        move |name| {
            names
                .contains(&name)
                .then(|| PathBuf::from("/usr/bin").join(name))
        }
    }

    fn args(command: Option<(PathBuf, Vec<OsString>)>) -> Option<(String, Vec<String>)> {
        command.map(|(program, args)| {
            (
                program.display().to_string(),
                args.iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect(),
            )
        })
    }

    #[test]
    fn finds_a_terminal_without_x_terminal_emulator() {
        let script = Path::new("/tmp/login.command");
        // Manjaro with sway: foot, kitty and ghostty, no x-terminal-emulator.
        let found = installed(&["foot", "kitty", "ghostty"]);
        assert_eq!(
            args(terminal_command(script, None, &found)),
            Some((
                "/usr/bin/ghostty".to_owned(),
                vec!["-e".to_owned(), "/tmp/login.command".to_owned()]
            ))
        );
        // TERMINAL wins, with its own way of taking a command.
        assert_eq!(
            args(terminal_command(script, Some("foot"), &found)),
            Some((
                "/usr/bin/foot".to_owned(),
                vec!["/tmp/login.command".to_owned()]
            ))
        );
        // An unknown TERMINAL gets `-e`; a missing one is skipped.
        let found = installed(&["myterm", "kitty"]);
        assert_eq!(
            args(terminal_command(script, Some("myterm"), &found))
                .unwrap()
                .1,
            ["-e", "/tmp/login.command"]
        );
        assert_eq!(
            args(terminal_command(script, Some("nope"), &found))
                .unwrap()
                .0,
            "/usr/bin/kitty"
        );
        // The freedesktop launcher and Debian's alternative come first.
        let found = installed(&["kitty", "xdg-terminal-exec", "x-terminal-emulator"]);
        assert_eq!(
            args(terminal_command(script, None, &found)).unwrap().0,
            "/usr/bin/xdg-terminal-exec"
        );
        assert_eq!(terminal_command(script, None, &installed(&[])), None);
    }
}
