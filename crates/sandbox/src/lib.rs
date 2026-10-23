//! The agent sandbox (PLAN §4.18, ADR-013): every agent process Maze starts, everything
//! it spawns, and every terminal command Maze runs for an agent runs inside an OS
//! sandbox that can't read the mail store, the rest of the home directory or other
//! agent sessions.
//!
//! Adapted from Zed's `sandbox` crate (`zed-industries/zed`, `crates/sandbox`,
//! Copyright 2022 - 2026 Zed Industries, Inc., GPL-3.0-or-later): the Seatbelt base
//! profile with its `mach-lookup` allowlist and the bubblewrap namespace flags come from
//! there. Zed's policy only restricts writes; Maze adds read denial ([`SandboxPolicy::hide`],
//! [`SandboxPolicy::deny_read`]) and the Unix socket allowlist.
//!
//! - **macOS:** `/usr/bin/sandbox-exec -p <profile> -- program args…`
//!   ([`seatbelt_profile`]). The profile is passed inline, so there is no profile file
//!   an agent could rewrite between two commands.
//! - **Linux:** `bwrap <args> -- program args…` ([`bwrap_args`]).
//!
//! [`wrap`] turns a program and its arguments into the wrapped command for the current
//! platform. The crate only builds commands; spawning (with a cleared environment and
//! its own process group) is up to the caller (`agent`).
//!
//! This crate is layer L1 and has no Maze dependencies.

mod bwrap;
mod seatbelt;

use std::path::{Path, PathBuf};

pub use bwrap::bwrap_args;
pub use seatbelt::seatbelt_profile;

/// `sandbox-exec`, present on every macOS install.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Outbound network for the sandboxed process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NetworkAccess {
    /// Any host (agents have to reach their model provider). Unix sockets stay limited
    /// to [`SandboxPolicy::unix_sockets`].
    #[default]
    All,
    /// No network at all, except the allowed Unix sockets.
    None,
}

/// What a sandboxed process may touch. See PLAN §4.18 for the default policy that the
/// `agent` crate builds from this.
///
/// Rules apply in this order, and later rules win (Seatbelt semantics; the bubblewrap
/// mounts are ordered the same way):
///
/// 1. Everything outside [`hide`](Self::hide) is readable (system locations).
/// 2. [`hide`](Self::hide) roots are unreadable: the home directory, and the parts of
///    the data directory that hold other sessions and other agents' homes.
/// 3. [`read`](Self::read) paths are readable again (read-only), [`write`](Self::write)
///    paths are readable and writable. Nothing else is writable.
/// 4. [`deny_read`](Self::deny_read) paths are unreadable and unwritable no matter what
///    came before (the mail store, the user's extra denials).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// Directories hidden from the process, typically `$HOME` first.
    pub hide: Vec<PathBuf>,
    /// Paths readable (and executable) inside hidden directories.
    pub read: Vec<PathBuf>,
    /// Directories the process may write (and read).
    pub write: Vec<PathBuf>,
    /// Paths that are never readable or writable.
    pub deny_read: Vec<PathBuf>,
    /// The only Unix sockets the process may connect to (`mazemail.sock`).
    pub unix_sockets: Vec<PathBuf>,
    /// Outbound network.
    pub network: NetworkAccess,
}

impl SandboxPolicy {
    /// A short, human description of the policy for the padlock tooltip.
    pub fn describe(&self) -> String {
        let list = |paths: &[PathBuf]| {
            if paths.is_empty() {
                "nothing".to_owned()
            } else {
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        };
        format!(
            "Hidden: {}\nReadable: {}\nWritable: {}\nNever readable: {}\nSockets: {}\nNetwork: {}",
            list(&self.hide),
            list(&self.read),
            list(&self.write),
            list(&self.deny_read),
            list(&self.unix_sockets),
            match self.network {
                NetworkAccess::All => "any host",
                NetworkAccess::None => "off",
            }
        )
    }
}

/// A command ready to spawn: the sandbox launcher and its arguments, ending with the
/// original program and arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedCommand {
    /// The program to execute (`/usr/bin/sandbox-exec`, `bwrap`, or the original program
    /// when sandboxing is off).
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// Whether the command really runs in a sandbox.
    pub sandboxed: bool,
}

/// Errors building a sandboxed command.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// A path can't be written into a Seatbelt profile or bwrap argument.
    #[error("the path {0} is not valid UTF-8 and can't be used in the sandbox policy")]
    NonUtf8Path(PathBuf),
    /// A policy path is relative.
    #[error("sandbox policy paths must be absolute, got {0}")]
    RelativePath(PathBuf),
    /// The platform's sandbox tool is missing.
    #[error("{0}")]
    Unavailable(String),
    /// This platform has no sandbox implementation.
    #[error("agents can't be sandboxed on this platform")]
    UnsupportedPlatform,
}

/// Whether the current platform can sandbox agents; the error explains what to install.
pub fn check_available() -> Result<(), SandboxError> {
    if cfg!(target_os = "macos") {
        if Path::new(SANDBOX_EXEC).exists() {
            Ok(())
        } else {
            Err(SandboxError::Unavailable(format!(
                "{SANDBOX_EXEC} is missing, so agents can't be sandboxed"
            )))
        }
    } else if cfg!(target_os = "linux") {
        find_bwrap().map(|_| ())
    } else {
        Err(SandboxError::UnsupportedPlatform)
    }
}

/// The non-setuid `bwrap` on `PATH`.
fn find_bwrap() -> Result<PathBuf, SandboxError> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("bwrap"))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            SandboxError::Unavailable(
                "Agents need bubblewrap (`bwrap`) on Linux. Install it with your package \
                 manager (for example `sudo apt install bubblewrap`; on Ubuntu 24.04 also \
                 allow unprivileged user namespaces for it in AppArmor), or turn the agent \
                 sandbox off in settings (`\"agent_sandbox\": { \"enabled\": false }`)."
                    .to_owned(),
            )
        })
}

/// Wraps `program args…` for the current platform's sandbox under `policy`, starting in
/// `cwd` (bwrap needs it inside the sandbox; on macOS the caller sets the working
/// directory when spawning).
pub fn wrap(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    policy: &SandboxPolicy,
) -> Result<WrappedCommand, SandboxError> {
    validate(policy)?;
    let program_str = utf8(program)?.to_owned();
    if cfg!(target_os = "macos") {
        check_available()?;
        let mut wrapped = vec![
            "-p".to_owned(),
            seatbelt_profile(policy)?,
            "--".to_owned(),
            program_str,
        ];
        wrapped.extend(args.iter().cloned());
        Ok(WrappedCommand {
            program: PathBuf::from(SANDBOX_EXEC),
            args: wrapped,
            sandboxed: true,
        })
    } else if cfg!(target_os = "linux") {
        let bwrap = find_bwrap()?;
        let mut wrapped = bwrap_args(policy, cwd)?;
        wrapped.push("--".to_owned());
        wrapped.push(program_str);
        wrapped.extend(args.iter().cloned());
        Ok(WrappedCommand {
            program: bwrap,
            args: wrapped,
            sandboxed: true,
        })
    } else {
        Err(SandboxError::UnsupportedPlatform)
    }
}

/// `program args…` unchanged, for the settings opt-out (`agent_sandbox.enabled: false`).
pub fn unsandboxed(program: &Path, args: &[String]) -> WrappedCommand {
    WrappedCommand {
        program: program.to_owned(),
        args: args.to_vec(),
        sandboxed: false,
    }
}

fn validate(policy: &SandboxPolicy) -> Result<(), SandboxError> {
    let all = policy
        .hide
        .iter()
        .chain(&policy.read)
        .chain(&policy.write)
        .chain(&policy.deny_read)
        .chain(&policy.unix_sockets);
    for path in all {
        if !path.is_absolute() {
            return Err(SandboxError::RelativePath(path.clone()));
        }
        utf8(path)?;
    }
    Ok(())
}

pub(crate) fn utf8(path: &Path) -> Result<&str, SandboxError> {
    path.to_str()
        .ok_or_else(|| SandboxError::NonUtf8Path(path.to_owned()))
}

/// `path` as given plus its canonical form when that differs (`/var` and `/tmp` are
/// symlinks into `/private` on macOS, and the user may have symlinked directories).
/// Rules are emitted for both, like Zed does for socket paths: Seatbelt matches the
/// resolved path for file operations, the literal one for some others.
pub(crate) fn with_resolved(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![path.to_owned()];
    if let Ok(resolved) = std::fs::canonicalize(path)
        && resolved != path
    {
        out.push(resolved);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_are_rejected() {
        let policy = SandboxPolicy {
            read: vec![PathBuf::from("relative/dir")],
            ..SandboxPolicy::default()
        };
        assert!(matches!(
            wrap(Path::new("/bin/true"), &[], None, &policy),
            Err(SandboxError::RelativePath(_))
        ));
    }

    #[test]
    fn unsandboxed_passes_the_command_through() {
        let cmd = unsandboxed(Path::new("/bin/echo"), &["hi".to_owned()]);
        assert_eq!(cmd.program, PathBuf::from("/bin/echo"));
        assert_eq!(cmd.args, vec!["hi".to_owned()]);
        assert!(!cmd.sandboxed);
    }

    #[test]
    fn describe_lists_every_part() {
        let policy = SandboxPolicy {
            hide: vec![PathBuf::from("/Users/a")],
            write: vec![PathBuf::from("/Users/a/scratch")],
            ..SandboxPolicy::default()
        };
        let text = policy.describe();
        assert!(text.contains("Hidden: /Users/a"));
        assert!(text.contains("Writable: /Users/a/scratch"));
        assert!(text.contains("Network: any host"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn wrap_uses_sandbox_exec_on_macos() {
        let policy = SandboxPolicy {
            hide: vec![PathBuf::from("/Users/nobody-here")],
            ..SandboxPolicy::default()
        };
        let cmd = wrap(Path::new("/bin/echo"), &["hi".to_owned()], None, &policy).unwrap();
        assert_eq!(cmd.program, PathBuf::from(SANDBOX_EXEC));
        assert_eq!(cmd.args[0], "-p");
        assert!(cmd.args[1].starts_with("(version 1)"));
        assert_eq!(&cmd.args[2..], ["--", "/bin/echo", "hi"]);
        assert!(cmd.sandboxed);
    }
}
