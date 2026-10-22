//! Who is on the other end of the socket (PLAN §4.11 "Peer check").
//!
//! The kernel reports the peer's uid and pid for a Unix socket (`getpeereid` plus
//! `LOCAL_PEEREPID` on macOS, `SO_PEERCRED` on Linux, both through
//! [`tokio::net::UnixStream::peer_cred`]). Connections from other uids are rejected
//! before a single frame is read. The pid, its executable and its parent go into the
//! approval dialog and the audit log.
//!
//! The executable path is looked up by pid after the connection is accepted
//! (`proc_pidpath` on macOS, `/proc/<pid>/exe` on Linux). A process that exits right
//! after connecting could in theory have its pid reused in between; the path is
//! information for the user, not an authorization input (tokens and approvals are).

#[allow(unsafe_code)]
mod sys;

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::net::UnixStream;

/// The uid this process runs as (effective uid).
pub fn current_uid() -> u32 {
    sys::effective_uid()
}

/// The peer runs as a different user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("peer uid {peer} is not the server's uid {expected}")]
pub struct PeerRejected {
    /// The peer's uid.
    pub peer: u32,
    /// The uid the server accepts.
    pub expected: u32,
}

/// Accepts only peers running as `expected`.
pub fn check_peer_uid(peer: u32, expected: u32) -> Result<(), PeerRejected> {
    if peer == expected {
        Ok(())
    } else {
        Err(PeerRejected { peer, expected })
    }
}

/// The connecting process.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    /// Its uid (always the server's, after the check).
    pub uid: u32,
    /// Its gid.
    pub gid: u32,
    /// Its process details.
    pub process: ProcessInfo,
}

impl PeerInfo {
    /// Reads the credentials of a connected stream and looks up the process.
    pub fn from_stream(stream: &UnixStream) -> io::Result<Self> {
        let cred = stream.peer_cred()?;
        Ok(Self {
            uid: cred.uid(),
            gid: cred.gid(),
            process: cred.pid().map(ProcessInfo::lookup).unwrap_or_default(),
        })
    }
}

/// A process as shown in approval dialogs and the audit log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    /// Process id.
    pub pid: Option<i32>,
    /// Executable path.
    pub exe: Option<PathBuf>,
    /// Parent process (usually the shell or agent that ran `mazemail`).
    pub parent: Option<ParentProcess>,
}

/// The parent of the connecting process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentProcess {
    /// Process id.
    pub pid: i32,
    /// Executable path.
    pub exe: Option<PathBuf>,
}

impl ProcessInfo {
    /// Looks up `pid`'s executable and parent (best effort: fields the OS doesn't
    /// report stay `None`).
    pub fn lookup(pid: i32) -> Self {
        let parent = sys::parent_pid(pid)
            .filter(|&ppid| ppid > 0)
            .map(|ppid| ParentProcess {
                pid: ppid,
                exe: sys::executable(ppid),
            });
        Self {
            pid: Some(pid),
            exe: sys::executable(pid),
            parent,
        }
    }

    /// "`/usr/bin/zsh` (pid 42)", for dialogs.
    pub fn describe(&self) -> String {
        let exe = self.exe.as_ref().map_or_else(
            || "unknown program".to_owned(),
            |exe| exe.display().to_string(),
        );
        match self.pid {
            Some(pid) => format!("{exe} (pid {pid})"),
            None => exe,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_check() {
        let me = current_uid();
        assert_eq!(check_peer_uid(me, me), Ok(()));
        let other = me.wrapping_add(1);
        assert_eq!(
            check_peer_uid(other, me),
            Err(PeerRejected {
                peer: other,
                expected: me
            })
        );
    }

    #[test]
    fn looks_up_own_process() {
        let pid = i32::try_from(std::process::id()).unwrap();
        let info = ProcessInfo::lookup(pid);
        assert_eq!(info.pid, Some(pid));
        let exe = info.exe.clone().expect("own executable path");
        assert_eq!(
            exe.canonicalize().unwrap(),
            std::env::current_exe().unwrap().canonicalize().unwrap()
        );
        let parent = info.parent.clone().expect("parent process");
        assert!(parent.pid > 0);
        assert!(info.describe().contains(&format!("(pid {pid})")));
    }

    #[test]
    fn unknown_pid_is_best_effort() {
        // Pids are below 2^22 on Linux and 99999 on macOS.
        let info = ProcessInfo::lookup(i32::MAX);
        assert_eq!(info.exe, None);
        assert_eq!(info.parent, None);
        assert!(info.describe().starts_with("unknown program"));
    }

    #[tokio::test]
    async fn reads_credentials_of_a_socket_pair() {
        let (a, _b) = UnixStream::pair().unwrap();
        let peer = PeerInfo::from_stream(&a).unwrap();
        assert_eq!(peer.uid, current_uid());
        assert_eq!(
            peer.process.pid,
            Some(i32::try_from(std::process::id()).unwrap())
        );
    }
}
