//! Maze end to end against a real IMAP server: `mazemail --headless` with a fresh data
//! directory, the `mazemail` CLI adding a Stalwart account (password on stdin), the
//! initial sync, search, new mail through IDLE, and an archive replayed on the server.
//!
//! Ignored by default because it downloads and runs a server binary:
//!
//! ```sh
//! MAZEMAIL_TEST_IMAP=1 cargo nextest run -p mazemail --run-ignored only --test imap_e2e
//! ```

// Tests may unwrap and print (AGENTS.md).
#![allow(clippy::unwrap_used, clippy::print_stderr)]

mod support;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use imap::{ImapAuth, ImapClient, ImapConfig, ImapSession, Security};
use mazemail_types::Flags;
use serde_json::Value;
use support::Stalwart;

/// `mazemail --headless`, stopped with SIGTERM on drop.
struct Headless {
    child: Child,
}

impl Drop for Headless {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Env {
    data: PathBuf,
    config: PathBuf,
    token: Option<String>,
}

impl Env {
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mazemail"));
        command
            .env("MAZEMAIL_DATA_DIR", &self.data)
            .env("MAZEMAIL_CONFIG_DIR", &self.config)
            .env("MAZEMAIL_INSECURE_DEV_KEYSTORE", "1")
            .env("MAZEMAIL_INSECURE_DEV_SECRETS", "1")
            .env("MAZEMAIL_TEST_AUTO_APPROVE", "1")
            .env_remove("MAZEMAIL_SESSION")
            .env_remove("MAZEMAIL_API_ADDR");
        if let Some(token) = &self.token {
            command.env("MAZEMAIL_SESSION", token);
        }
        command
    }

    /// Runs a CLI command with `--json`, feeding `stdin`, and returns its output.
    fn json(&self, args: &[&str], stdin: Option<&str>) -> Value {
        let output = self.run(args, stdin);
        assert!(
            output.status.success(),
            "mazemail {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut child = self
            .command()
            .args(args)
            .arg("--json")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        if let Some(text) = stdin {
            input.write_all(text.as_bytes()).unwrap();
        }
        drop(input);
        child.wait_with_output().unwrap()
    }
}

fn wait_for(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) -> Duration {
    let start = Instant::now();
    while !check() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
    start.elapsed()
}

fn message(n: u32, subject: &str) -> Vec<u8> {
    let date = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc2822)
        .unwrap();
    format!(
        "From: Bob <bob@example.net>\r\nTo: alice@example.org\r\nSubject: {subject}\r\n\
         Message-ID: <e2e-{n}@example.net>\r\nDate: {date}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\nThe zanzibar budget, part {n}.\r\n"
    )
    .into_bytes()
}

async fn imap(port: u16) -> ImapClient {
    let config = ImapConfig::new(
        "127.0.0.1",
        port,
        ImapAuth::password(support::USER, support::PASSWORD),
    )
    .with_security(Security::PlainInsecure)
    .with_command_timeout(Duration::from_secs(10));
    ImapClient::connect(&config).await.unwrap()
}

fn hits(value: &Value) -> usize {
    value.as_array().map_or(0, Vec::len)
}

fn socket(data: &Path) -> PathBuf {
    data.join("mazemail.sock")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Stalwart; set MAZEMAIL_TEST_IMAP=1 and run ignored tests"]
async fn headless_maze_syncs_a_real_imap_server() {
    if !support::enabled() {
        eprintln!("skipping: MAZEMAIL_TEST_IMAP is not 1");
        return;
    }
    let server = Stalwart::start().unwrap();
    let mut other = imap(server.port).await;
    other.create("Archive").await.unwrap();
    other.subscribe("Archive", true).await.unwrap();
    for n in 1..=3 {
        other
            .append(
                "INBOX",
                &message(n, &format!("Budget report {n}")),
                Flags::empty(),
            )
            .await
            .unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let mut env = Env {
        data: dir.path().join("data"),
        config: dir.path().join("config"),
        token: None,
    };
    std::fs::create_dir_all(&env.config).unwrap();
    std::fs::write(
        env.config.join("settings.json"),
        r#"{ "agent": { "accounts": { "Test": "any" } } }"#,
    )
    .unwrap();
    let log = std::fs::File::create(dir.path().join("headless.log")).unwrap();
    let _headless = Headless {
        child: env
            .command()
            .arg("--headless")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    };
    wait_for("the socket", Duration::from_secs(60), || {
        socket(&env.data).exists()
    });

    let login = env.json(&["login", "--profile", "writer", "--ttl", "1h"], None);
    env.token = Some(login["token"].as_str().unwrap().to_owned());

    let port = server.port.to_string();
    let added = env.json(
        &[
            "account",
            "add",
            "alice@example.org",
            "--name",
            "Test",
            "--host",
            "127.0.0.1",
            "--port",
            &port,
            "--security",
            "none",
            "--username",
            support::USER,
            "--password-stdin",
        ],
        Some(&format!("{}\n", support::PASSWORD)),
    );
    assert_eq!(added["kind"], "imap");
    let account = added["id"].as_str().unwrap().to_owned();

    // A wrong password is rejected before anything is saved.
    let wrong = env.run(
        &[
            "account",
            "add",
            "bob@example.org",
            "--host",
            "127.0.0.1",
            "--port",
            &port,
            "--security",
            "none",
            "--username",
            "alice",
            "--password-stdin",
        ],
        Some("wrong\n"),
    );
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("rejected the login"));

    wait_for("the initial sync", Duration::from_secs(60), || {
        env.json(&["account", "list"], None)[0]["state"] == "idle"
    });
    let folders = env.json(&["folders"], None);
    let inbox = folders
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["role"] == "inbox")
        .unwrap();
    assert_eq!(inbox["total"], 3);
    wait_for("search", Duration::from_secs(30), || {
        hits(&env.json(&["search", "zanzibar"], None)) == 3
    });

    // New mail arrives through IDLE.
    other
        .append(
            "INBOX",
            &message(4, "Budget report 4 (live)"),
            Flags::empty(),
        )
        .await
        .unwrap();
    let took = wait_for("IDLE delivery", Duration::from_secs(15), || {
        hits(&env.json(&["search", "live"], None)) == 1
    });
    eprintln!("new mail was searchable after {took:?}");

    // Archive with the writer token: replayed on the server into Archive/<year>.
    let first = env.json(&["search", "subject:\"Budget report 1\""], None)[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let archived = env.json(&["archive", &first], None);
    assert_eq!(archived["outcome"], "applied");
    let mut archive_folder = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    while archive_folder.is_none() {
        assert!(
            Instant::now() < deadline,
            "the archive never reached the server"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        for folder in other.list().await.unwrap() {
            if folder.name.starts_with("Archive/")
                && other.select(&folder.name, None).await.unwrap().exists == 1
            {
                archive_folder = Some(folder.name);
            }
        }
    }
    assert_eq!(other.select("INBOX", None).await.unwrap().exists, 3);

    // Sync on demand works; removing the account deletes its password.
    let synced = env.json(&["sync", "--account", &account], None);
    assert_eq!(synced["accounts"][0], account.as_str());
    env.json(&["account", "remove", &account], None);
    assert_eq!(hits(&env.json(&["account", "list"], None)), 0);
    let secrets = std::fs::read_to_string(env.data.join("insecure-dev-secrets.json")).unwrap();
    assert!(!secrets.contains(support::PASSWORD));
    let _ = other.logout().await;
}
