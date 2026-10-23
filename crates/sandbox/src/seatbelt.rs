//! The macOS Seatbelt profile (PLAN §4.18).
//!
//! The base profile (deny by default, the `mach-lookup` allowlist, pseudo-terminals,
//! the `/dev` write targets, the network Mach services) is Zed's
//! (`crates/sandbox/src/macos_seatbelt.rs`, GPL-3.0-or-later), which derives from
//! Codex's and Chromium's policies. Maze's order on top of it:
//!
//! ```text
//! (deny default)
//! (allow file-read*)
//! (deny file-read* (subpath "$HOME"))            ; and the other hidden roots
//! (allow file-read* (subpath "<allowed>"))       ; agent home, scratch, agents, runtimes, CLI
//! (allow file-read-metadata (literal "<ancestor>"))  ; so realpath() works on the way there
//! (allow file-write* (subpath "<writable>"))     ; scratch, per-session TMPDIR, agent home
//! (deny file-read* file-write* (subpath "<deny_read>"))
//! (allow network*)                               ; when network is on
//! (deny network-outbound (remote unix-socket))
//! (allow network-outbound (remote unix-socket (literal "<data_dir>/mazemail.sock")))
//! (allow network-outbound (literal "/private/var/run/mDNSResponder"))  ; DNS, network on
//! ```
//!
//! In Seatbelt the last matching rule wins. Every path is emitted as given and, when it
//! differs, with symlinks resolved.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::{NetworkAccess, SandboxError, SandboxPolicy, utf8, with_resolved};

/// Zed's base profile: everything denied, reads allowed, process and Mach basics.
const BASE: &str = r#"(version 1)

; Start by denying everything
(deny default)

; Allow reading from the entire filesystem (narrowed below)
(allow file-read*)

; Allow process execution
(allow process-exec*)
(allow process-fork)

; Allow signal handling
(allow signal)

; Allow sysctl reads (needed for many system calls)
(allow sysctl-read)

; Mach service lookups: an ALLOWLIST (from Zed, Codex and Chromium). An unrestricted
; mach-lookup would let a sandboxed process reach LaunchServices / launchd and have a
; process spawned outside the sandbox, so those endpoints, the pasteboard and audio are
; deliberately missing.
(allow mach-lookup
    (global-name "com.apple.system.opendirectoryd.libinfo")
    (global-name "com.apple.system.opendirectoryd.membership")
    (global-name "com.apple.system.DirectoryService.libinfo_v1")
    (global-name "com.apple.bsd.dirhelper")
    (global-name "com.apple.cfprefsd.daemon")
    (global-name "com.apple.cfprefsd.agent")
    (local-name "com.apple.cfprefsd.agent")
    (global-name "com.apple.logd")
    (global-name "com.apple.logd.events")
    (global-name "com.apple.system.logger")
    (global-name "com.apple.diagnosticd")
    (global-name "com.apple.system.notification_center")
    (global-name "com.apple.analyticsd")
    (global-name "com.apple.analyticsd.messagetracer")
    (global-name "com.apple.PowerManagement.control")
    (global-name "com.apple.dt.automationmode.reader"))

; Pseudo-terminals
(allow pseudo-tty)
(allow file-read* file-write* file-ioctl
    (literal "/dev/ptmx"))
(allow file-read* file-write*
    (require-all
        (regex #"^/dev/ttys[0-9]+$")
        (extension "com.apple.sandbox.pty")))
(allow file-ioctl
    (regex #"^/dev/ttys[0-9]+$"))
"#;

/// Writes every process needs (redirections like `2>/dev/null`).
const DEV_WRITES: &str = r#"
; Common /dev write targets
(allow file-write*
    (literal "/dev/null")
    (literal "/dev/zero")
    (literal "/dev/tty")
    (literal "/dev/stdin")
    (literal "/dev/stdout")
    (literal "/dev/stderr")
    (subpath "/dev/fd"))
"#;

/// Mach services for DNS, TLS trust and the keychain, only with network on.
const NETWORK_SERVICES: &str = r#"
; Network on: DNS, network configuration, TLS trust and keychain services
(allow mach-lookup
    (global-name "com.apple.SystemConfiguration.configd")
    (global-name "com.apple.SystemConfiguration.DNSConfiguration")
    (global-name "com.apple.networkd")
    (global-name "com.apple.mDNSResponder")
    (global-name "com.apple.SecurityServer")
    (global-name "com.apple.trustd")
    (global-name "com.apple.trustd.agent")
    (global-name "com.apple.ocspd"))
"#;

/// The Seatbelt profile text for `policy`.
pub fn seatbelt_profile(policy: &SandboxPolicy) -> Result<String, SandboxError> {
    let mut out = String::from(BASE);

    if !policy.hide.is_empty() {
        out.push_str("\n; Hide the home directory and other sessions\n");
        for root in &policy.hide {
            for path in with_resolved(root) {
                rule(&mut out, "deny file-read*", "subpath", &path)?;
            }
        }
    }

    let readable: Vec<&PathBuf> = policy.read.iter().chain(&policy.write).collect();
    if !readable.is_empty() {
        out.push_str(
            "\n; Readable again: agent home, session scratch dir, agents, runtimes, CLI\n",
        );
        for allowed in &readable {
            for path in with_resolved(allowed) {
                rule(&mut out, "allow file-read*", "subpath", &path)?;
            }
        }
        // `realpath()` (Node's module loader, for one) stats every component on the
        // way to an allowed path. Metadata of those directories only, never listings.
        let mut ancestors: Vec<PathBuf> = Vec::new();
        for allowed in &readable {
            for path in with_resolved(allowed) {
                for ancestor in hidden_ancestors(&path, &policy.hide) {
                    if !ancestors.contains(&ancestor) {
                        ancestors.push(ancestor);
                    }
                }
            }
        }
        if !ancestors.is_empty() {
            out.push_str("\n; Metadata of the directories leading there\n");
            for ancestor in &ancestors {
                rule(&mut out, "allow file-read-metadata", "literal", ancestor)?;
            }
        }
    }

    if !policy.write.is_empty() {
        out.push_str("\n; Writable: session scratch dir, per-session TMPDIR, agent home\n");
        for writable in &policy.write {
            for path in with_resolved(writable) {
                rule(&mut out, "allow file-write*", "subpath", &path)?;
            }
        }
    }
    out.push_str(DEV_WRITES);

    if !policy.deny_read.is_empty() {
        out.push_str("\n; Never readable or writable\n");
        for denied in &policy.deny_read {
            for path in with_resolved(denied) {
                rule(&mut out, "deny file-read* file-write*", "subpath", &path)?;
            }
        }
    }

    match policy.network {
        NetworkAccess::All => {
            out.push_str("\n; Outbound network\n(allow network*)\n(allow system-socket)\n");
            out.push_str(NETWORK_SERVICES);
        }
        NetworkAccess::None => {
            out.push_str("\n; No network; Unix sockets only for the allowlist below\n");
            out.push_str("(allow system-socket (socket-domain AF_UNIX))\n");
        }
    }
    // `(allow network*)` also allows connecting to any Unix socket (Docker, ssh-agent,
    // 1Password). Only Maze's own socket stays reachable.
    out.push_str("\n; Unix sockets: only Maze's API socket\n");
    out.push_str("(deny network-outbound (remote unix-socket))\n");
    for socket in &policy.unix_sockets {
        for path in with_resolved(socket) {
            writeln!(
                out,
                "(allow network-outbound (remote unix-socket (literal \"{}\")))",
                escape(utf8(&path)?)
            )
            .ok();
        }
    }
    if policy.network == NetworkAccess::All {
        // Name resolution goes through mDNSResponder's socket (as in Codex's policy);
        // without it nothing resolves.
        out.push_str("(allow network-outbound (literal \"/private/var/run/mDNSResponder\"))\n");
    }
    Ok(out)
}

/// Directories between a hidden root (inclusive) and `path` (exclusive).
fn hidden_ancestors(path: &Path, hide: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in hide {
        for root in with_resolved(root) {
            if path == root || !path.starts_with(&root) {
                continue;
            }
            let mut current = path.parent();
            while let Some(dir) = current {
                out.push(dir.to_owned());
                if dir == root {
                    break;
                }
                current = dir.parent();
            }
        }
    }
    out
}

fn rule(out: &mut String, action: &str, filter: &str, path: &Path) -> Result<(), SandboxError> {
    writeln!(out, "({action} ({filter} \"{}\"))", escape(utf8(path)?)).ok();
    Ok(())
}

/// Escapes a path for a Seatbelt string literal.
fn escape(path: &str) -> String {
    path.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(profile: &str) -> Vec<&str> {
        profile
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with(';'))
            .collect()
    }

    fn position(profile: &str, needle: &str) -> usize {
        rules(profile)
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("{needle} not in profile:\n{profile}"))
    }

    fn policy() -> SandboxPolicy {
        let data = "/Users/anna/Library/Application Support/mazemail";
        SandboxPolicy {
            hide: vec![PathBuf::from("/Users/anna")],
            read: vec![
                PathBuf::from(format!("{data}/agents")),
                PathBuf::from(format!("{data}/runtimes")),
            ],
            write: vec![
                PathBuf::from(format!("{data}/sessions/s1")),
                PathBuf::from(format!("{data}/agent-home/claude-acp")),
            ],
            deny_read: vec![PathBuf::from(format!("{data}/mail"))],
            unix_sockets: vec![PathBuf::from(format!("{data}/mazemail.sock"))],
            network: NetworkAccess::All,
        }
    }

    #[test]
    fn rules_come_in_the_documented_order() {
        let profile = seatbelt_profile(&policy()).unwrap();
        let deny_default = position(&profile, "(deny default)");
        let read_all = position(&profile, "(allow file-read*)");
        let hide = position(&profile, "(deny file-read* (subpath \"/Users/anna\"))");
        let allow_scratch = position(
            &profile,
            "(allow file-read* (subpath \"/Users/anna/Library/Application Support/mazemail/sessions/s1\"))",
        );
        let write_scratch = position(
            &profile,
            "(allow file-write* (subpath \"/Users/anna/Library/Application Support/mazemail/sessions/s1\"))",
        );
        let deny_mail = position(
            &profile,
            "(deny file-read* file-write* (subpath \"/Users/anna/Library/Application Support/mazemail/mail\"))",
        );
        let network = position(&profile, "(allow network*)");
        let deny_sockets = position(&profile, "(deny network-outbound (remote unix-socket))");
        let allow_socket = position(&profile, "mazemail.sock");
        assert!(deny_default < read_all);
        assert!(read_all < hide);
        assert!(hide < allow_scratch);
        assert!(allow_scratch < write_scratch);
        assert!(write_scratch < deny_mail);
        assert!(deny_mail < network);
        assert!(network < deny_sockets);
        assert!(deny_sockets < allow_socket);
        let dns = position(
            &profile,
            "(allow network-outbound (literal \"/private/var/run/mDNSResponder\"))",
        );
        assert!(deny_sockets < dns);
    }

    #[test]
    fn only_listed_directories_are_writable() {
        let profile = seatbelt_profile(&policy()).unwrap();
        let writes: Vec<&str> = rules(&profile)
            .into_iter()
            .filter(|line| line.starts_with("(allow file-write* (subpath"))
            .collect();
        assert_eq!(writes.len(), 2, "{writes:?}");
        assert!(writes.iter().any(|w| w.contains("sessions/s1")));
        assert!(writes.iter().any(|w| w.contains("agent-home/claude-acp")));
        assert!(!profile.contains("(allow file-write*)"));
        // Read-only paths are not writable.
        assert!(!writes.iter().any(|w| w.contains("/agents\"")));
    }

    #[test]
    fn ancestors_get_metadata_only() {
        let profile = seatbelt_profile(&policy()).unwrap();
        for dir in [
            "/Users/anna",
            "/Users/anna/Library",
            "/Users/anna/Library/Application Support",
            "/Users/anna/Library/Application Support/mazemail",
            "/Users/anna/Library/Application Support/mazemail/sessions",
        ] {
            assert!(
                profile.contains(&format!("(allow file-read-metadata (literal \"{dir}\"))")),
                "{dir}"
            );
            assert!(!profile.contains(&format!("(allow file-read* (subpath \"{dir}\"))")));
        }
        assert!(!profile.contains("(allow file-read-metadata (literal \"/Users\"))"));
    }

    #[test]
    fn mach_lookup_stays_an_allowlist() {
        let profile = seatbelt_profile(&policy()).unwrap();
        let rules = rules(&profile).join("\n");
        assert!(rules.contains("(allow mach-lookup"));
        assert!(!rules.contains("(allow mach-lookup)"));
        assert!(!rules.contains("launchservicesd"));
        assert!(!rules.contains("com.apple.pasteboard"));
    }

    #[test]
    fn no_network_keeps_only_the_socket() {
        let mut policy = policy();
        policy.network = NetworkAccess::None;
        let profile = seatbelt_profile(&policy).unwrap();
        assert!(!profile.contains("(allow network*)"));
        assert!(!profile.contains("com.apple.SecurityServer"));
        assert!(profile.contains("(allow system-socket (socket-domain AF_UNIX))"));
        assert!(!profile.contains("mDNSResponder\"))"));
        assert!(profile.contains(
            "(allow network-outbound (remote unix-socket (literal \"/Users/anna/Library/Application Support/mazemail/mazemail.sock\")))"
        ));
    }

    #[test]
    fn quotes_in_paths_are_escaped() {
        let policy = SandboxPolicy {
            read: vec![PathBuf::from("/Users/a/we\"ird\\dir")],
            ..SandboxPolicy::default()
        };
        let profile = seatbelt_profile(&policy).unwrap();
        assert!(profile.contains(r#"(allow file-read* (subpath "/Users/a/we\"ird\\dir"))"#));
    }

    #[test]
    fn symlinked_paths_are_emitted_resolved_too() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let policy = SandboxPolicy {
            write: vec![link.clone()],
            ..SandboxPolicy::default()
        };
        let profile = seatbelt_profile(&policy).unwrap();
        let resolved = std::fs::canonicalize(&link).unwrap();
        assert!(profile.contains(&format!(
            "(allow file-write* (subpath \"{}\"))",
            link.display()
        )));
        assert!(profile.contains(&format!(
            "(allow file-write* (subpath \"{}\"))",
            resolved.display()
        )));
    }
}
