//! A throwaway Stalwart IMAP + SMTP submission server for integration tests.
//!
//! Like `crates/imap/tests/support` (whose `fetch-stalwart.sh` downloads the pinned,
//! checksum-verified release into `target/stalwart/`), plus a plain-text submission
//! listener that accepts `AUTH PLAIN`/`LOGIN` without TLS (local tests only) and
//! delivers to the in-memory directory's mailboxes. Nothing is relayed outside.
//!
//! Set `MAZEMAIL_STALWART_BIN` to use an existing binary instead.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

pub(crate) const USER: &str = "alice";
pub(crate) const PASSWORD: &str = "secret";

/// Whether the integration tests were asked for (`MAZEMAIL_TEST_IMAP=1`).
pub(crate) fn enabled() -> bool {
    std::env::var("MAZEMAIL_TEST_IMAP").is_ok_and(|v| v == "1")
}

pub(crate) struct Stalwart {
    child: Child,
    pub(crate) port: u16,
    pub(crate) smtp_port: u16,
    _dir: tempfile::TempDir,
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"),
        PathBuf::from,
    )
}

fn binary() -> anyhow::Result<PathBuf> {
    if let Some(bin) = std::env::var_os("MAZEMAIL_STALWART_BIN") {
        return Ok(PathBuf::from(bin));
    }
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../imap/tests/support/fetch-stalwart.sh");
    let out = Command::new("sh")
        .arg(&script)
        .arg(target_dir().join("stalwart"))
        .stderr(Stdio::inherit())
        .output()
        .context("running fetch-stalwart.sh")?;
    if !out.status.success() {
        bail!("fetch-stalwart.sh failed");
    }
    Ok(PathBuf::from(String::from_utf8(out.stdout)?.trim()))
}

fn free_port() -> anyhow::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

impl Stalwart {
    pub(crate) fn start() -> anyhow::Result<Self> {
        let bin = binary()?;
        let dir = tempfile::tempdir()?;
        let port = free_port()?;
        let smtp_port = free_port()?;
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data)?;
        let config = format!(
            r#"
[server]
hostname = "localhost"

[server.listener.imap]
bind = ["127.0.0.1:{port}"]
protocol = "imap"

[server.listener.submission]
bind = ["127.0.0.1:{smtp_port}"]
protocol = "smtp"

[imap.auth]
allow-plain-text = true

[session.auth]
mechanisms = "[plain, login]"
directory = "'memory'"
require = true
allow-plain-text = true

[session.rcpt]
directory = "'memory'"
relay = false

[storage]
data = "rocksdb"
fts = "rocksdb"
blob = "rocksdb"
lookup = "rocksdb"
directory = "memory"

[store.rocksdb]
type = "rocksdb"
path = "{data}"
compression = "lz4"

[directory.memory]
type = "memory"
domains = ["example.org"]

[[directory.memory.principals]]
name = "{USER}"
class = "individual"
secret = "{PASSWORD}"
email = ["{USER}@example.org"]

[tracer.stdout]
type = "stdout"
level = "error"
ansi = false
enable = true
"#,
            data = data.display(),
        );
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, config)?;
        let log = std::fs::File::create(dir.path().join("stalwart.log"))?;
        let child = Command::new(&bin)
            .arg("--config")
            .arg(&config_path)
            .current_dir(dir.path())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .with_context(|| format!("starting {}", bin.display()))?;
        let mut server = Self {
            child,
            port,
            smtp_port,
            _dir: dir,
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        let up = |port: u16| {
            let addr = SocketAddr::from(([127, 0, 0, 1], port));
            TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
        };
        while !(up(port) && up(smtp_port)) {
            if let Some(status) = server.child.try_wait()? {
                bail!("stalwart exited early with {status}");
            }
            if Instant::now() > deadline {
                bail!("stalwart did not open port {port} in time");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(server)
    }
}

impl Stalwart {
    /// The server's log so far, for failure messages.
    pub(crate) fn log(&self) -> String {
        std::fs::read_to_string(self._dir.path().join("stalwart.log")).unwrap_or_default()
    }
}

impl Drop for Stalwart {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
