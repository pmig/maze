//! Maze's importers and exporters (PLAN §4.14, CONCEPT §17).
//!
//! - [`mbox`] and [`maildir`]: streaming readers and writers.
//! - [`reconstruct`]: Thunderbird's stored copy → the server's bytes, with flags and
//!   keywords from `X-Mozilla-Status`/`-Status2`/`-Keys` ([`mozilla`]).
//! - [`local`]: `mazemail import mbox|maildir` into a local account, idempotent.
//! - [`export`]: `mazemail export mbox|maildir`, plaintext, the reverse of the readers.
//! - [`thunderbird`]: profile discovery, `prefs.js` and the other profile files, a
//!   dry-run [`ImportReport`](thunderbird::ImportReport) and the import itself.
//! - [`reuse`]: the encrypted index of Thunderbird's offline IMAP stores that lets the
//!   first sync take bodies from disk ([`BodyReuse`], an [`ingest::BodyLookup`]).
//!
//! Layer L2: uses `store` and `ingest` (L1) directly; `mazemail_core` and the CLI call
//! it. All functions are blocking and use rayon for parallel work; call them from a
//! blocking task.
//!
//! # Privacy
//!
//! Nothing here logs message content, subjects or addresses. The only plaintext Maze
//! writes is the export the user asked for ([`export::PLAINTEXT_WARNING`]) and a
//! short-lived copy of address books in a private temp directory while reading them.

mod error;
pub mod export;
pub mod local;
pub mod maildir;
pub mod mbox;
pub mod mozilla;
pub mod progress;
pub mod reuse;
pub mod thunderbird;
pub mod tree;

pub use error::{Error, Result};
pub use export::{ExportFormat, ExportReport, ExportScope, PLAINTEXT_WARNING, export};
pub use local::{ImportStats, ImportTarget, import_maildir, import_mbox};
pub use mozilla::{MozillaState, Reconstructed, reconstruct};
pub use progress::{NoProgress, Phase, Progress, ProgressUpdate, Unit};
pub use reuse::{BodyReuse, ScanStats};
