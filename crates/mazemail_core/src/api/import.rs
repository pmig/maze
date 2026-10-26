//! `mazemail import`: mbox files and Maildir folders into a folder of an IMAP account,
//! and Thunderbird profiles, through the `import` crate (PLAN §4.14, ADR-019).
//!
//! Maze keeps all mail on the servers, so mbox and Maildir messages are not stored
//! locally: the target folder (and any missing parent) is created like the user would
//! create it ([`MailCommand::CreateFolder`]), then every message becomes a queued
//! `append` op ([`import::upload`]) that the account's sync engine replays; the
//! folder's next sync brings the messages in. Re-running an import skips what the
//! folder already has or what is still queued. The import runs on a blocking thread;
//! progress goes to the client as `working` frames.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use import::thunderbird::{ImportOptions, ThunderbirdProfile};
use import::{ImportStats, ProgressUpdate, UploadTarget};
use local_api::registry::args::{ImportArgs, ImportFormat};
use local_api::{CommandError, Progress, RequestContext};
use mazemail_types::{AccountId, FolderId, FolderRole};
use store::{AccountKind, Queries as _, Store};
use tokio::sync::mpsc;

use crate::api::handler::core_error;
use crate::commands::{MailCommand, Origin};
use crate::handle::CoreHandle;

/// Minimum time between two progress frames.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// What an import did, for the response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Imported {
    pub(crate) stats: ImportStats,
    /// Folders created on the server (mbox, Maildir).
    pub(crate) folders: u64,
    /// Accounts created (Thunderbird profiles).
    pub(crate) accounts: u64,
    /// What a Thunderbird import skipped, one sentence each.
    pub(crate) skipped: Vec<String>,
}

/// What the path points at.
fn detect(path: &Path, format: Option<ImportFormat>) -> Result<ImportFormat, CommandError> {
    if let Some(format) = format {
        return Ok(format);
    }
    let metadata = std::fs::metadata(path).map_err(|error| {
        CommandError::invalid_args(format!("cannot read {}: {error}", path.display()))
    })?;
    if metadata.is_file() {
        return Ok(ImportFormat::Mbox);
    }
    if import::maildir::is_maildir(path) {
        return Ok(ImportFormat::Maildir);
    }
    if path.join("prefs.js").is_file() || path.join("profiles.ini").is_file() {
        return Ok(ImportFormat::Thunderbird);
    }
    Err(CommandError::invalid_args(format!(
        "{} is neither an mbox file, a Maildir folder nor a Thunderbird profile; pass --format",
        path.display()
    )))
}

fn import_error(error: import::Error) -> CommandError {
    match error {
        import::Error::InvalidTarget(reason) => CommandError::invalid_args(reason),
        import::Error::NoProfile(path) => CommandError::invalid_args(format!(
            "no Thunderbird profile found in {}",
            path.display()
        )),
        import::Error::Io {
            context,
            path,
            source,
        } => CommandError::invalid_args(format!("{context} {}: {source}", path.display())),
        other => {
            tracing::error!(error = %other, "import failed");
            CommandError::internal("the import failed inside Maze")
        }
    }
}

fn store_error(error: store::Error) -> CommandError {
    core_error(error.into())
}

/// Runs the import described by `args`. The dispatcher has already had the user
/// approve it (the command's permission is `ask`); `origin` is recorded with the
/// folders it creates and the uploads it queues.
pub(crate) async fn run(
    core: &CoreHandle,
    ctx: &RequestContext,
    origin: Origin,
    args: ImportArgs,
) -> Result<Imported, CommandError> {
    if !args.path.is_absolute() {
        return Err(CommandError::invalid_args(
            "the import path must be absolute (the CLI resolves it)",
        ));
    }
    if let Some(account) = args.account
        && !ctx.scope.allows_account(account)
    {
        return Err(CommandError::not_found(format!("no account {account}")));
    }
    let format = detect(&args.path, args.format)?;
    let jobs = match format {
        ImportFormat::Thunderbird => {
            if args.account.is_some() || args.folder.is_some() {
                return Err(CommandError::invalid_args(
                    "a Thunderbird profile brings its own accounts; drop --account and --folder",
                ));
            }
            Jobs::Thunderbird(args.path.clone())
        }
        ImportFormat::Mbox | ImportFormat::Maildir => {
            let account = target_account(core, ctx, args.account)?;
            let base = match args.folder.as_deref() {
                Some(path) => folder_path(path)?,
                None => vec![file_label(&args.path)],
            };
            let mut sources = vec![(args.path.clone(), base.clone())];
            if format == ImportFormat::Maildir {
                if !import::maildir::is_maildir(&args.path) {
                    return Err(CommandError::invalid_args(format!(
                        "{} is not a Maildir folder (no cur/ directory)",
                        args.path.display()
                    )));
                }
                for (segments, dir) in
                    import::maildir::maildirpp_subfolders(&args.path).map_err(import_error)?
                {
                    let mut path = base.clone();
                    path.extend(segments);
                    sources.push((dir, path));
                }
            }
            let mut uploads = Vec::with_capacity(sources.len());
            let mut folders = 0;
            for (source, path) in sources {
                let (folder, created) = ensure_folder(core, origin, account, &path).await?;
                folders += created;
                uploads.push((source, UploadTarget { account, folder }));
            }
            Jobs::Upload {
                format,
                uploads,
                folders,
            }
        }
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<ProgressUpdate>();
    let store = core.store().clone();
    let origin_text = origin.as_text();
    let mut job = core
        .runtime()
        .spawn_blocking(move || blocking_import(&store, jobs, &origin_text, &tx));

    let mut last = Instant::now().checked_sub(PROGRESS_INTERVAL);
    let result = loop {
        tokio::select! {
            joined = &mut job => break joined,
            Some(update) = rx.recv() => {
                if last.is_some_and(|at| at.elapsed() < PROGRESS_INTERVAL) {
                    continue;
                }
                last = Some(Instant::now());
                let total = (update.total > 0).then_some(update.total);
                // A client that went away doesn't stop the import: half an import is
                // worse than a finished one nobody watched.
                let _ = ctx
                    .progress(Progress::working(
                        format!("importing {}", update.phase),
                        Some(update.done),
                        total,
                    ))
                    .await;
            }
        }
    };
    let (imported, account) = result
        .map_err(|_| CommandError::internal("the import stopped unexpectedly"))?
        .map_err(import_error)?;
    if let Some(account) = account
        && imported.stats.queued > 0
    {
        core.ops_enqueued(account);
    }
    tracing::info!(
        queued = imported.stats.queued,
        duplicates = imported.stats.duplicates,
        failed = imported.stats.failed,
        accounts = imported.accounts,
        "local api import finished"
    );
    Ok(imported)
}

/// What the blocking part does.
enum Jobs {
    Thunderbird(PathBuf),
    Upload {
        format: ImportFormat,
        /// Each source (the file, the Maildir and its Maildir++ subfolders) and where
        /// it goes.
        uploads: Vec<(PathBuf, UploadTarget)>,
        folders: u64,
    },
}

/// The account mbox and Maildir mail goes to: the one given, or the only one there is.
fn target_account(
    core: &CoreHandle,
    ctx: &RequestContext,
    account: Option<AccountId>,
) -> Result<AccountId, CommandError> {
    let syncs = |kind: AccountKind| {
        matches!(
            kind,
            AccountKind::Imap | AccountKind::Gmail | AccountKind::M365
        )
    };
    let accounts = core.accounts().map_err(core_error)?;
    match account {
        Some(id) => {
            let found = accounts
                .iter()
                .find(|a| a.id == id)
                .ok_or_else(|| CommandError::not_found(format!("no account {id}")))?;
            if syncs(found.kind) {
                Ok(id)
            } else {
                Err(CommandError::invalid_args(format!(
                    "account {id} isn't an IMAP account; Maze imports into IMAP, Gmail and \
                     Microsoft 365 accounts"
                )))
            }
        }
        None => {
            let candidates: Vec<AccountId> = accounts
                .iter()
                .filter(|a| syncs(a.kind) && ctx.scope.allows_account(a.id))
                .map(|a| a.id)
                .collect();
            match candidates.as_slice() {
                [only] => Ok(*only),
                [] => Err(CommandError::invalid_args(
                    "there is no IMAP account to import into; add one first",
                )),
                _ => Err(CommandError::invalid_args(
                    "choose the account to import into with --account (`mazemail account list` \
                     shows them)",
                )),
            }
        }
    }
}

/// `Archive/2019` → `["Archive", "2019"]`.
fn folder_path(path: &str) -> Result<Vec<String>, CommandError> {
    let segments: Vec<String> = path.split('/').map(|s| s.trim().to_owned()).collect();
    if segments.iter().any(String::is_empty) {
        return Err(CommandError::invalid_args(format!(
            "{path:?} isn't a folder path; separate folders with `/` (`Archive/2019`)"
        )));
    }
    Ok(segments)
}

/// A folder name from a file or directory name.
fn file_label(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .map(|s| s.to_string_lossy().trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Imported".to_owned())
}

/// The folder at `path` below the account's root, created (with missing parents) on
/// the server if it isn't there. Returns it and how many folders were created.
async fn ensure_folder(
    core: &CoreHandle,
    origin: Origin,
    account: AccountId,
    path: &[String],
) -> Result<(FolderId, u64), CommandError> {
    let mut parent: Option<FolderId> = None;
    let mut created = 0;
    for segment in path {
        let folders = core
            .store()
            .reader()
            .and_then(|r| r.folders(Some(account)))
            .map_err(store_error)?;
        let found = folders.iter().find(|f| {
            f.parent == parent
                && f.remote_name.is_some()
                && (f.display_name == *segment
                    || (parent.is_none()
                        && f.role == Some(FolderRole::Inbox)
                        && segment.eq_ignore_ascii_case("inbox")))
        });
        parent = Some(match found {
            Some(folder) => folder.id,
            None => {
                let receipt = core
                    .apply_approved(
                        MailCommand::CreateFolder {
                            account,
                            parent,
                            name: segment.clone(),
                        },
                        origin,
                    )
                    .await
                    .map_err(core_error)?;
                created += 1;
                receipt
                    .created_folder
                    .ok_or_else(|| CommandError::internal("the folder wasn't created"))?
            }
        });
    }
    parent
        .map(|folder| (folder, created))
        .ok_or_else(|| CommandError::invalid_args("the folder path is empty"))
}

fn blocking_import(
    store: &Store,
    jobs: Jobs,
    origin: &str,
    tx: &mpsc::UnboundedSender<ProgressUpdate>,
) -> import::Result<(Imported, Option<AccountId>)> {
    let progress = |update: ProgressUpdate| {
        let _ = tx.send(update);
    };
    match jobs {
        Jobs::Upload {
            format,
            uploads,
            folders,
        } => {
            let mut out = Imported {
                folders,
                ..Imported::default()
            };
            let account = uploads.first().map(|(_, target)| target.account);
            for (source, target) in uploads {
                let stats = match format {
                    ImportFormat::Maildir => {
                        import::upload_maildir(store, &source, target, origin, &progress)?
                    }
                    _ => import::upload_mbox(store, &source, target, origin, &progress)?,
                };
                out.stats.add(&stats);
            }
            Ok((out, account))
        }
        Jobs::Thunderbird(path) => {
            let profile = ThunderbirdProfile::load(&path)?;
            let outcome = import::thunderbird::import(
                &profile,
                store,
                None,
                &ImportOptions::default(),
                &progress,
            )?;
            Ok((
                Imported {
                    accounts: outcome.accounts_created as u64,
                    skipped: outcome.skipped,
                    ..Imported::default()
                },
                None,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_paths_and_default_names() {
        assert_eq!(folder_path("Archive/2019").unwrap(), ["Archive", "2019"]);
        assert_eq!(folder_path(" Old mail ").unwrap(), ["Old mail"]);
        assert!(folder_path("Archive//2019").is_err());
        assert!(folder_path("").is_err());
        assert_eq!(
            file_label(Path::new("/x/Archive 2023.mbox")),
            "Archive 2023"
        );
        assert_eq!(file_label(Path::new("/x/Mail")), "Mail");
    }
}
