//! Opening links and terminal logins outside Maze, behind a trait so tests never start
//! a browser or Terminal.

use std::path::Path;

use gpui::App;

/// Opens links in the browser and login scripts in a terminal.
pub trait Launcher: Send + Sync + 'static {
    /// Opens an `https://` link in the default browser.
    fn open_url(&self, url: &str, cx: &mut App);

    /// Opens a login script (from [`agent::AgentManager::terminal_login`]) in a
    /// terminal window.
    fn open_terminal(&self, script: &Path) -> std::io::Result<()>;
}

/// The real thing: the system browser, and Terminal on macOS
/// (`x-terminal-emulator` elsewhere).
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
            let mut command = std::process::Command::new("x-terminal-emulator");
            command.arg("-e").arg(script);
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
