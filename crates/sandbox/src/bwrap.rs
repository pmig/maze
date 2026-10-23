//! The Linux bubblewrap arguments (PLAN §4.18).
//!
//! Namespace flags follow Zed's `crates/sandbox/src/linux_bubblewrap.rs`
//! (GPL-3.0-or-later). Mount order, later mounts covering earlier ones:
//!
//! ```text
//! --ro-bind / /  --dev /dev  --proc /proc  --tmpfs /tmp
//! --tmpfs $HOME                     ; and the other hidden roots
//! --ro-bind <read> <read>           ; agent home's siblings stay hidden
//! --bind <write> <write>            ; scratch, TMPDIR, agent home
//! --tmpfs <deny_read dir> | --ro-bind /dev/null <deny_read file>
//! --bind <socket> <socket>          ; mazemail.sock
//! ```
//!
//! bubblewrap can only bind paths that exist, so missing read and write paths are
//! skipped (the caller creates the session's directories first) and `deny_read` entries
//! are masked only when they exist. Masked paths sit under read-only mounts, so the
//! sandboxed process can't swap them for symlinks.
//!
//! **Not yet implemented** (PLAN §4.18): the seccomp filter that blocks `AF_UNIX`
//! sockets and the loopback bridge (`mazemail --sandbox-launcher`) that would then carry
//! the API. Until then the socket is bind-mounted and reachable directly; other Unix
//! sockets are hidden only as far as their paths are (`/tmp` is private, `$HOME` and
//! `$XDG_RUNTIME_DIR` are masked by the `agent` crate's policy), and abstract-namespace
//! sockets stay reachable while the network namespace is shared.

use std::path::Path;

use crate::{NetworkAccess, SandboxError, SandboxPolicy, utf8};

/// bubblewrap arguments for `policy` (without the trailing `-- program args…`).
pub fn bwrap_args(policy: &SandboxPolicy, cwd: Option<&Path>) -> Result<Vec<String>, SandboxError> {
    let mut args: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| args.extend(items.iter().map(|s| (*s).to_owned()));
    push(&["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]);
    push(&["--tmpfs", "/tmp"]);

    for root in &policy.hide {
        let root = utf8(root)?;
        push(&["--tmpfs", root]);
    }
    for path in &policy.read {
        if path.exists() {
            let path = utf8(path)?;
            push(&["--ro-bind", path, path]);
        }
    }
    for path in &policy.write {
        if path.exists() {
            let path = utf8(path)?;
            push(&["--bind", path, path]);
        }
    }
    for path in &policy.deny_read {
        if path.is_dir() {
            push(&["--tmpfs", utf8(path)?]);
        } else if path.exists() {
            let path = utf8(path)?;
            push(&["--ro-bind", "/dev/null", path]);
        }
    }
    for socket in &policy.unix_sockets {
        if socket.exists() {
            let socket = utf8(socket)?;
            push(&["--bind", socket, socket]);
        }
    }

    push(&[
        "--unshare-user",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-pid",
        "--unshare-cgroup-try",
        "--die-with-parent",
        "--new-session",
    ]);
    if policy.network == NetworkAccess::None {
        push(&["--unshare-net"]);
    }
    if let Some(cwd) = cwd {
        push(&["--chdir", utf8(cwd)?]);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pretty_assertions::assert_eq;

    use super::*;

    fn windows(args: &[String], needle: &[&str]) -> Option<usize> {
        args.windows(needle.len())
            .position(|w| w.iter().zip(needle).all(|(a, b)| a == b))
    }

    #[test]
    fn mounts_follow_the_documented_order() {
        let home = tempfile::tempdir().unwrap();
        let home_path = home.path().to_str().unwrap().to_owned();
        let data = home.path().join(".local/share/mazemail");
        let scratch = data.join("sessions/s1");
        let agents = data.join("agents");
        let mail = data.join("mail");
        let socket = data.join("mazemail.sock");
        for dir in [&scratch, &agents, &mail] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(&socket, b"").unwrap();
        let missing = data.join("runtimes");
        let policy = SandboxPolicy {
            hide: vec![home.path().to_owned()],
            read: vec![agents.clone(), missing.clone()],
            write: vec![scratch.clone()],
            deny_read: vec![mail.clone(), PathBuf::from("/nonexistent/secret")],
            unix_sockets: vec![socket.clone()],
            network: NetworkAccess::All,
        };
        let args = bwrap_args(&policy, Some(&scratch)).unwrap();
        let s = |p: &Path| p.to_str().unwrap().to_owned();

        assert_eq!(
            &args[..7],
            ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]
        );
        let tmp = windows(&args, &["--tmpfs", "/tmp"]).unwrap();
        let hide = windows(&args, &["--tmpfs", &home_path]).unwrap();
        let read = windows(&args, &["--ro-bind", &s(&agents), &s(&agents)]).unwrap();
        let write = windows(&args, &["--bind", &s(&scratch), &s(&scratch)]).unwrap();
        let deny = windows(&args, &["--tmpfs", &s(&mail)]).unwrap();
        let sock = windows(&args, &["--bind", &s(&socket), &s(&socket)]).unwrap();
        assert!(tmp < hide && hide < read && read < write && write < deny && deny < sock);

        // Missing paths can't be bound, and missing denials need no mask.
        assert!(!args.contains(&s(&missing)));
        assert!(!args.iter().any(|a| a == "/nonexistent/secret"));
        assert!(args.iter().any(|a| a == "--die-with-parent"));
        assert!(!args.iter().any(|a| a == "--unshare-net"));
        assert!(windows(&args, &["--chdir", &s(&scratch)]).is_some());
    }

    #[test]
    fn denied_files_are_masked_with_dev_null() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("logins.json");
        std::fs::write(&file, b"{}").unwrap();
        let policy = SandboxPolicy {
            deny_read: vec![file.clone()],
            network: NetworkAccess::None,
            ..SandboxPolicy::default()
        };
        let args = bwrap_args(&policy, None).unwrap();
        let file = file.to_str().unwrap();
        assert!(windows(&args, &["--ro-bind", "/dev/null", file]).is_some());
        assert!(args.iter().any(|a| a == "--unshare-net"));
    }
}
