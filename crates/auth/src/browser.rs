//! Opening the authorization URL in the system browser.

use std::io;
use std::process::{Command, Stdio};

use url::Url;

/// Opens a URL in a browser. [`SystemBrowser`] is the real one; tests inject a fake
/// that follows the redirect itself. Closures `Fn(&Url) -> io::Result<()>` implement
/// it too.
///
/// The URL carries the `state`, the PKCE challenge and possibly the user's address
/// as `login_hint`: implementations must not log it.
pub trait BrowserOpener: Send + Sync {
    /// Opens `url`. Returns once the browser was asked to open it, not when the user
    /// is done.
    fn open(&self, url: &Url) -> io::Result<()>;
}

impl<F> BrowserOpener for F
where
    F: Fn(&Url) -> io::Result<()> + Send + Sync,
{
    fn open(&self, url: &Url) -> io::Result<()> {
        self(url)
    }
}

/// The user's default browser: `/usr/bin/open` on macOS, `xdg-open` on Linux and
/// other Unix systems.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemBrowser;

impl BrowserOpener for SystemBrowser {
    fn open(&self, url: &Url) -> io::Result<()> {
        let mut command = system_command()?;
        // Passed as a single argument, never through a shell.
        command
            .arg(url.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn()?;
        // `xdg-open` can stay around for as long as the browser runs; reap it in the
        // background instead of waiting.
        std::thread::Builder::new()
            .name("mazemail-browser".to_owned())
            .spawn(move || match child.wait() {
                Ok(status) if !status.success() => {
                    tracing::warn!(%status, "the browser launcher reported a failure");
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(%err, "waiting for the browser launcher failed"),
            })?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn system_command() -> io::Result<Command> {
    Ok(Command::new("/usr/bin/open"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn system_command() -> io::Result<Command> {
    Ok(Command::new("xdg-open"))
}

#[cfg(not(unix))]
fn system_command() -> io::Result<Command> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "opening a browser is not supported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn closures_are_openers() {
        let opened = Mutex::new(Vec::new());
        let opener = |url: &Url| {
            opened.lock().unwrap().push(url.to_string());
            Ok(())
        };
        let url = Url::parse("https://example.org/auth").unwrap();
        opener.open(&url).unwrap();
        assert_eq!(*opened.lock().unwrap(), ["https://example.org/auth"]);
    }
}
