//! `mazemail export`: the messages matching a search as plaintext mbox or Maildir
//! (CONCEPT §1 "no lock-in", PLAN §4.14).
//!
//! `import::export` exports whole accounts and folder trees; the local API exports
//! the result of a query instead, which can span folders and accounts. The bytes are
//! written with `import`'s own [`MboxWriter`] and [`MaildirWriter`], so an export
//! carries the same flags and tags (`X-Mozilla-Status`, Maildir info and Dovecot
//! keywords) and imports back byte for byte.
//!
//! The export is one of the documented plaintext exceptions (PLAN §4.17): the
//! command always asks for approval, files are created with mode 0600 (directories
//! 0700) and an existing target is never overwritten.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};

use import::maildir::MaildirWriter;
use import::mbox::{Envelope, MboxWriter};
use import::mozilla::MozillaState;
use local_api::registry::args::ExportFormat;
use mazemail_types::{Flags, FolderId};
use store::{MessageDetail, Store};

/// What an export wrote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Exported {
    /// Messages written.
    pub(crate) messages: u64,
    /// Messages skipped because only their headers are stored.
    pub(crate) without_body: u64,
}

/// Why an export failed. Paths are the user's own, so they may be shown.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ExportError {
    #[error("{} already exists; choose a new file or directory", .0.display())]
    Exists(PathBuf),
    #[error("cannot write {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Import(#[from] import::Error),
    #[error(transparent)]
    Store(#[from] store::Error),
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> ExportError + '_ {
    move |source| ExportError::Io {
        path: path.to_owned(),
        source,
    }
}

/// The flags and keywords of a message: from its copy in `folder` if given, else its
/// first copy.
fn state(detail: &MessageDetail, folder: Option<FolderId>) -> (Flags, Vec<String>) {
    let location = folder
        .and_then(|folder| detail.locations.iter().find(|l| l.folder == folder))
        .or_else(|| detail.locations.first());
    location
        .map(|l| (l.flags, l.keywords.clone()))
        .unwrap_or_default()
}

/// The stored raw message, `None` if only headers are stored.
fn raw(store: &Store, detail: &MessageDetail) -> Result<Option<Vec<u8>>, ExportError> {
    let Some(blob) = detail.blob else {
        return Ok(None);
    };
    match store.blobs().read(&blob) {
        Ok(raw) => Ok(Some(raw)),
        Err(store::Error::BlobMissing(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// A new file only we can read; fails if anything exists at `path`.
fn create_private_file(path: &Path) -> Result<File, ExportError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            ExportError::Exists(path.to_owned())
        } else {
            io_error(path)(error)
        }
    })
}

/// Writes `messages` (oldest first) to `target`. Blocking: call it from a blocking
/// task. `folder` picks which copy's flags are written when a message is in several
/// folders.
pub(crate) fn write(
    store: &Store,
    messages: &[MessageDetail],
    format: ExportFormat,
    target: &Path,
    folder: Option<FolderId>,
) -> Result<Exported, ExportError> {
    if std::fs::symlink_metadata(target).is_ok() {
        return Err(ExportError::Exists(target.to_owned()));
    }
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty())
        && !parent.is_dir()
    {
        return Err(io_error(parent)(io::ErrorKind::NotFound.into()));
    }
    let mut out = Exported::default();
    match format {
        ExportFormat::Mbox => {
            let file = create_private_file(target)?;
            let mut writer = MboxWriter::new(BufWriter::with_capacity(1 << 20, file));
            for detail in messages {
                let Some(raw) = raw(store, detail)? else {
                    out.without_body += 1;
                    continue;
                };
                let (flags, keywords) = state(detail, folder);
                let mozilla = MozillaState::from_flags(flags, &keywords);
                let envelope = Envelope {
                    sender: detail.from.as_ref().map(|a| a.email.clone()),
                    date: Some(detail.received.unwrap_or(detail.date)),
                };
                writer
                    .write_message(&envelope, Some(&mozilla), &raw)
                    .map_err(io_error(target))?;
                out.messages += 1;
            }
            let buffered = writer.finish().map_err(io_error(target))?;
            let file = buffered
                .into_inner()
                .map_err(|error| io_error(target)(error.into_error()))?;
            file.sync_all().map_err(io_error(target))?;
        }
        ExportFormat::Maildir => {
            let mut writer = MaildirWriter::create(target)?;
            for detail in messages {
                let Some(raw) = raw(store, detail)? else {
                    out.without_body += 1;
                    continue;
                };
                let (flags, keywords) = state(detail, folder);
                writer.write(&raw, flags, &keywords)?;
                out.messages += 1;
            }
            writer.finish()?;
        }
    }
    Ok(out)
}
