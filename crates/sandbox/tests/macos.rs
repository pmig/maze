//! The Seatbelt profile against the real `sandbox-exec`: a fake home directory in a
//! temp dir stands in for `$HOME`.

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used)] // Tests may unwrap (AGENTS.md).

use std::io::ErrorKind;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use sandbox::{NetworkAccess, SandboxPolicy, wrap};

struct FakeHome {
    _dir: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
    scratch: PathBuf,
}

impl FakeHome {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // Seatbelt matches resolved paths; `/var` is a symlink to `/private/var`.
        let home = std::fs::canonicalize(dir.path()).unwrap();
        let data = home.join("Library/Application Support/mazemail");
        let scratch = data.join("sessions/s1");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::create_dir_all(data.join("sessions/s2")).unwrap();
        std::fs::create_dir_all(data.join("mail")).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join("secret.txt"), "top secret").unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), "private key").unwrap();
        std::fs::write(data.join("mail/mazemail.db"), "encrypted").unwrap();
        std::fs::write(data.join("sessions/s2/thread.md"), "other session").unwrap();
        std::fs::write(scratch.join("thread.md"), "# Thread").unwrap();
        Self {
            _dir: dir,
            home,
            data,
            scratch,
        }
    }

    fn policy(&self) -> SandboxPolicy {
        SandboxPolicy {
            hide: vec![self.home.clone()],
            read: vec![self.data.join("agents")],
            write: vec![self.scratch.clone()],
            deny_read: vec![self.data.join("mail")],
            unix_sockets: Vec::new(),
            network: NetworkAccess::None,
        }
    }

    fn run(&self, policy: &SandboxPolicy, program: &str, args: &[&str]) -> Output {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        let wrapped = wrap(Path::new(program), &args, Some(&self.scratch), policy).unwrap();
        Command::new(&wrapped.program)
            .args(&wrapped.args)
            .current_dir(&self.scratch)
            .env_clear()
            .env("HOME", &self.scratch)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn home_is_hidden_but_scratch_is_readable() {
    let fake = FakeHome::new();
    let policy = fake.policy();

    let out = fake.run(&policy, "/bin/cat", &[path(&fake.home.join("secret.txt"))]);
    assert!(!out.status.success(), "reading the home dir must fail");
    assert!(out.stdout.is_empty());

    let out = fake.run(
        &policy,
        "/bin/cat",
        &[path(&fake.scratch.join("thread.md"))],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"# Thread");

    for denied in [
        fake.home.join(".ssh/id_ed25519"),
        fake.data.join("mail/mazemail.db"),
        fake.data.join("sessions/s2/thread.md"),
    ] {
        let out = fake.run(&policy, "/bin/cat", &[path(&denied)]);
        assert!(
            !out.status.success(),
            "{} must be unreadable",
            denied.display()
        );
    }

    // Listing the home directory fails too.
    let out = fake.run(&policy, "/bin/ls", &[path(&fake.home)]);
    assert!(!out.status.success());
    // System locations stay readable.
    let out = fake.run(&policy, "/bin/cat", &["/etc/hosts"]);
    assert!(out.status.success());
}

#[test]
fn writes_only_land_in_the_scratch_dir() {
    let fake = FakeHome::new();
    let policy = fake.policy();
    let inside = fake.scratch.join("out.txt");
    let outside = fake.home.join("planted.txt");
    let script = format!(
        "echo ok > '{}'; echo bad > '{}'",
        path(&inside),
        path(&outside)
    );
    fake.run(&policy, "/bin/sh", &["-c", &script]);
    assert_eq!(std::fs::read_to_string(&inside).unwrap(), "ok\n");
    assert!(
        !outside.exists(),
        "writing outside the scratch dir must fail"
    );
}

#[test]
fn deny_read_wins_over_allowed_paths() {
    let fake = FakeHome::new();
    let mut policy = fake.policy();
    // Even if the whole data dir were allowed, the mail store stays unreadable.
    policy.read.push(fake.data.clone());
    let out = fake.run(
        &policy,
        "/bin/cat",
        &[path(&fake.data.join("mail/mazemail.db"))],
    );
    assert!(!out.status.success());
}

/// Accepts one connection within `wait`.
fn accept_one(listener: UnixListener, wait: Duration) -> std::thread::JoinHandle<bool> {
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let deadline = Instant::now() + wait;
        loop {
            match listener.accept() {
                Ok(_) => return true,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => return false,
            }
        }
    })
}

#[test]
fn only_the_maze_socket_is_reachable() {
    let fake = FakeHome::new();
    // Short paths: `sun_path` holds 104 bytes on macOS.
    let sockets = tempfile::Builder::new()
        .prefix("mz-")
        .tempdir_in("/tmp")
        .unwrap();
    let maze = sockets.path().join("mazemail.sock");
    let other = sockets.path().join("other.sock");
    let maze_accepted = accept_one(UnixListener::bind(&maze).unwrap(), Duration::from_secs(5));
    // `nc` has given up long before this listener stops waiting.
    let other_accepted = accept_one(UnixListener::bind(&other).unwrap(), Duration::from_secs(3));

    let mut policy = fake.policy();
    policy.network = NetworkAccess::All;
    policy.unix_sockets = vec![maze.clone()];

    fake.run(&policy, "/usr/bin/nc", &["-U", "-w", "2", path(&maze)]);
    fake.run(&policy, "/usr/bin/nc", &["-U", "-w", "2", path(&other)]);
    assert!(
        maze_accepted.join().unwrap(),
        "mazemail.sock must be reachable"
    );
    assert!(
        !other_accepted.join().unwrap(),
        "other Unix sockets must stay unreachable with network on"
    );
}
