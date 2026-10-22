//! Progress reporting for long imports and exports.

use std::fmt;

/// What an import or export is busy with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Phase {
    /// Reading the Thunderbird profile (prefs, address books, filters).
    Profile,
    /// Creating accounts, identities, SMTP servers and tags.
    Accounts,
    /// Importing messages into local folders.
    Messages,
    /// Scanning offline stores for the body reuse index.
    BodyIndex,
    /// Importing address books.
    Contacts,
    /// Importing filters and saved searches.
    Rules,
    /// Writing an export.
    Export,
}

/// One progress update. `total` is `0` when unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressUpdate {
    pub phase: Phase,
    /// Units done so far in this phase.
    pub done: u64,
    /// Units in this phase, `0` if unknown.
    pub total: u64,
    /// What `done` and `total` count.
    pub unit: Unit,
}

/// The unit of a [`ProgressUpdate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Unit {
    Messages,
    Bytes,
    Items,
}

/// Receives progress updates; called from worker threads, so keep it cheap.
pub trait Progress: Send + Sync {
    fn update(&self, update: ProgressUpdate);
}

impl<F: Fn(ProgressUpdate) + Send + Sync> Progress for F {
    fn update(&self, update: ProgressUpdate) {
        self(update);
    }
}

/// Ignores all updates.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProgress;

impl Progress for NoProgress {
    fn update(&self, _update: ProgressUpdate) {}
}

pub(crate) fn report(progress: &dyn Progress, phase: Phase, done: u64, total: u64, unit: Unit) {
    progress.update(ProgressUpdate {
        phase,
        done,
        total,
        unit,
    });
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Profile => "reading the profile",
            Self::Accounts => "accounts",
            Self::Messages => "messages",
            Self::BodyIndex => "offline body index",
            Self::Contacts => "contacts",
            Self::Rules => "filters and saved searches",
            Self::Export => "export",
        })
    }
}
