//! `mazemail import`: mbox files, Maildir folders and Thunderbird profiles through the
//! `import` crate (PLAN §4.14).
//!
//! Messages go through `ingest` with core's [`CoreIndexSink`](crate::CoreIndexSink),
//! so they are searchable as soon as the import returns (the index is committed
//! before the response). Re-importing the same source is idempotent (see
//! [`import::local`]). The import runs on a blocking thread; progress goes to the
//! client as `working` frames.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use import::thunderbird::{ImportOptions, ThunderbirdProfile};
use import::{ImportStats, ImportTarget, ProgressUpdate};
use ingest::Ingestor;
use local_api::registry::args::{ImportArgs, ImportFormat};
use local_api::{CommandError, Progress, RequestContext};
use mazemail_types::AccountId;
use tokio::sync::mpsc;

use crate::handle::CoreHandle;

/// Minimum time between two progress frames.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// What an import did, for the response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Imported {
    pub(crate) stats: ImportStats,
    /// Accounts created (Thunderbird profiles).
    pub(crate) accounts: u64,
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

/// Runs the import described by `args`. The dispatcher has already had the user
/// approve it (the command's permission is `ask`).
pub(crate) async fn run(
    core: &CoreHandle,
    ctx: &RequestContext,
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
    if format == ImportFormat::Thunderbird && args.account.is_some() {
        return Err(CommandError::invalid_args(
            "a Thunderbird profile brings its own accounts; drop --account",
        ));
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<ProgressUpdate>();
    let ingestor = Ingestor::with_sink(core.store().clone(), Arc::new(core.index_sink()));
    let path = args.path.clone();
    let account = args.account;
    let mut job = core
        .runtime()
        .spawn_blocking(move || blocking_import(&ingestor, &path, format, account, &tx));

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
    let imported = result
        .map_err(|_| CommandError::internal("the import stopped unexpectedly"))?
        .map_err(import_error)?;
    // Searchable before the client hears about it.
    core.commit_index().await.map_err(|error| {
        tracing::error!(%error, "committing the index after an import");
        CommandError::internal("the import finished but the search index failed")
    })?;
    tracing::info!(
        imported = imported.stats.imported,
        duplicates = imported.stats.duplicates,
        failed = imported.stats.failed,
        "local api import finished"
    );
    Ok(imported)
}

fn blocking_import(
    ingestor: &Ingestor,
    path: &Path,
    format: ImportFormat,
    account: Option<AccountId>,
    tx: &mpsc::UnboundedSender<ProgressUpdate>,
) -> import::Result<Imported> {
    let progress = |update: ProgressUpdate| {
        let _ = tx.send(update);
    };
    let target = ImportTarget {
        account,
        folder: None,
    };
    match format {
        ImportFormat::Mbox => Ok(Imported {
            stats: import::import_mbox(ingestor, path, &target, &progress)?,
            accounts: 0,
        }),
        ImportFormat::Maildir => Ok(Imported {
            stats: import::import_maildir(ingestor, path, &target, &progress)?,
            accounts: 0,
        }),
        ImportFormat::Thunderbird => {
            let profile = ThunderbirdProfile::load(path)?;
            let outcome = import::thunderbird::import(
                &profile,
                ingestor,
                None,
                &ImportOptions::default(),
                &progress,
            )?;
            Ok(Imported {
                stats: outcome.messages,
                accounts: outcome.accounts_created as u64,
            })
        }
    }
}
