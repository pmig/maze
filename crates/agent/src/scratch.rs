//! Session scratch dirs (PLAN §4.12, §4.17): `<data_dir>/sessions/<id>/` is the
//! agent's working directory. It holds `AGENTS.md` (generated from the `mazemail`
//! command registry), pointers for agents that read other file names, the selected mail
//! thread as Markdown when the user attached it, and the session's `TMPDIR`.
//!
//! It is one of the documented plaintext exceptions: the agent has to read it, so it
//! only ever holds what the user gave this session. It is deleted when the session ends
//! and stale ones are removed at startup.

use std::path::{Path, PathBuf};

use crate::paths::AgentPaths;

/// The thread export's file name.
pub const THREAD_FILE: &str = "thread.md";

/// A session's scratch dir; deleted on drop.
#[derive(Debug)]
pub struct Scratch {
    dir: PathBuf,
    tmp: PathBuf,
    keep: bool,
}

impl Scratch {
    /// Creates `<sessions>/<session>/` (mode 0700) with `AGENTS.md`, `CLAUDE.md` and
    /// `GEMINI.md` (both importing AGENTS.md) and the `TMPDIR`.
    pub fn create(paths: &AgentPaths, session: &str, agents_md: &str) -> std::io::Result<Self> {
        let dir = paths.session_dir(session);
        std::fs::create_dir_all(paths.sessions_dir())?;
        create_private_dir(&dir)?;
        let tmp = dir.join(".tmp");
        create_private_dir(&tmp)?;
        let scratch = Self {
            dir,
            tmp,
            keep: false,
        };
        std::fs::write(scratch.dir.join("AGENTS.md"), agents_md)?;
        // Claude Code reads CLAUDE.md and Gemini CLI reads GEMINI.md; both import.
        std::fs::write(scratch.dir.join("CLAUDE.md"), "@AGENTS.md\n")?;
        std::fs::write(scratch.dir.join("GEMINI.md"), "@AGENTS.md\n")?;
        Ok(scratch)
    }

    /// The scratch dir.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The session's `TMPDIR`.
    pub fn tmp(&self) -> &Path {
        &self.tmp
    }

    /// Writes the exported mail thread (replacing an earlier one).
    pub fn write_thread(&self, markdown: &str) -> std::io::Result<PathBuf> {
        let path = self.dir.join(THREAD_FILE);
        std::fs::write(&path, markdown)?;
        Ok(path)
    }

    /// Removes the exported thread.
    pub fn remove_thread(&self) {
        let _ = std::fs::remove_file(self.dir.join(THREAD_FILE));
    }

    /// Keeps the directory on drop (tests).
    pub fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.keep
            && let Err(error) = std::fs::remove_dir_all(&self.dir)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, "could not delete an agent session's scratch dir");
        }
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

/// Deletes every scratch dir. Called once at startup, before any session exists (the
/// data directory lock makes sure no other Maze is running).
pub fn remove_stale(paths: &AgentPaths) -> usize {
    let Ok(entries) = std::fs::read_dir(paths.sessions_dir()) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::info!(removed, "removed stale agent session scratch dirs");
    }
    removed
}

/// A fresh random session id (`s_` and 16 hex digits).
pub fn new_session_id() -> String {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        // Uniqueness matters, secrecy doesn't: fall back to the clock.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        bytes = (nanos as u64).to_le_bytes();
    }
    format!("s_{}", hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AgentPaths::new(dir.path());
        let session = new_session_id();
        assert!(session.starts_with("s_") && session.len() == 18);
        let scratch = Scratch::create(&paths, &session, "# Mail access").unwrap();
        let root = scratch.dir().to_owned();
        assert_eq!(
            std::fs::read_to_string(root.join("AGENTS.md")).unwrap(),
            "# Mail access"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("CLAUDE.md")).unwrap(),
            "@AGENTS.md\n"
        );
        assert!(scratch.tmp().is_dir());
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&root).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        scratch.write_thread("# Thread").unwrap();
        assert!(root.join(THREAD_FILE).is_file());
        drop(scratch);
        assert!(
            !root.exists(),
            "the scratch dir is deleted when the session ends"
        );
    }

    #[test]
    fn stale_dirs_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AgentPaths::new(dir.path());
        let mut a = Scratch::create(&paths, "s_a", "").unwrap();
        let mut b = Scratch::create(&paths, "s_b", "").unwrap();
        a.keep();
        b.keep();
        assert_eq!(remove_stale(&paths), 2);
        assert!(!paths.session_dir("s_a").exists());
        assert_eq!(remove_stale(&paths), 0);
    }
}
