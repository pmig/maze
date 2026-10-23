//! A throwaway Stalwart IMAP server for integration tests.
//!
//! A copy of `crates/sync/tests/support/mod.rs` (test support can't be shared
//! between crates without a test-support crate); it uses the imap crate's pinned,
//! checksum-verified download script.
//!
//! [`Stalwart::start`] downloads a pinned release into `target/stalwart/` (via
//! `fetch-stalwart.sh`, checksum-verified), writes a config with an in-memory
//! directory (user `alice`, password `secret`) and a RocksDB store in a temp dir,
//! starts it on a random localhost port and stops it on drop.
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
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data)?;
        let config = format!(
            r#"
[server]
hostname = "localhost"

[server.listener.imap]
bind = ["127.0.0.1:{port}"]
protocol = "imap"

[imap.auth]
allow-plain-text = true

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
            _dir: dir,
        };
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let deadline = Instant::now() + Duration::from_secs(30);
        while TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
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

impl Drop for Stalwart {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
