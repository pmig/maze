//! One folder pass (PLAN §4.5 "Folder sync algorithm").
//!
//! 1. `SELECT … (QRESYNC (uidvalidity modseq))` without known UIDs when the server
//!    supports it and the folder was synced before; apply `VANISHED` and the changed
//!    flags.
//! 2. Otherwise with CONDSTORE: `UID FETCH 1:* (FLAGS) (CHANGEDSINCE modseq)`, plus a
//!    `UID SEARCH ALL` diff for expunges when the message count doesn't add up or the
//!    periodic diff is due.
//! 3. Otherwise: chunked `UID FETCH … FLAGS` over the known UIDs, diffed against the
//!    store (missing UIDs were expunged).
//! 4. New UIDs (≥ the stored UIDNEXT; on a first sync every UID from `UID SEARCH ALL`)
//!    are fetched newest-first in batches and handed to the ingest pipeline.
//! 5. A changed UIDVALIDITY drops the folder's locations (`reset_folder`, bodies stay)
//!    and syncs it from scratch.
//!
//! UIDNEXT and HIGHESTMODSEQ are stored only after everything above committed, so a
//! pass that is interrupted (crash, connection loss, preemption) is simply repeated;
//! UIDs already stored are not fetched again.

use std::collections::{BTreeSet, HashMap, HashSet};

use imap::{FlagChange, GmailLabel, HeaderFetch, QresyncArgs, UidSet};
use mazemail_types::{Flags, FolderId, FolderRole, MessageId};
use store::{FolderSyncState, NewLocation, Queries};

use crate::connect::Conn;
use crate::error::{Result, SyncError};
use crate::events::{Delta, FolderCounts, SyncEvent};
use crate::gmail::{GmailFolders, apply_labels, drop_labels};
use crate::ingest::{HeaderInput, ReuseQuery};
use crate::shared::{Shared, YieldFlag};

/// What a pass should do beyond the minimum.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PassOptions {
    /// Run the `UID SEARCH ALL` expunge diff even if nothing hints at expunges.
    pub(crate) force_uid_diff: bool,
    /// Some locations in this folder have no UID yet (an op moved or appended a
    /// message without learning its UID): match new headers by Message-ID first.
    pub(crate) claim_unassigned: bool,
}

/// What a pass did.
#[derive(Debug, Clone, Default)]
pub(crate) struct PassReport {
    /// The pass finished and stored the new sync state (false: it yielded).
    pub(crate) complete: bool,
    /// New messages whose bodies the offline policy wants.
    pub(crate) body_candidates: Vec<(MessageId, i64)>,
    /// A `UID SEARCH ALL` diff ran.
    pub(crate) did_uid_diff: bool,
    /// Headers fetched.
    pub(crate) fetched: usize,
}

/// Syncs one mailbox folder.
pub(crate) async fn sync_folder(
    shared: &Shared,
    conn: &mut Conn,
    folder_id: FolderId,
    opts: PassOptions,
    yield_flag: &YieldFlag,
) -> Result<PassReport> {
    let folder = shared
        .read(move |r| r.folder(folder_id))
        .await?
        .ok_or(SyncError::FolderNotSynced(folder_id))?;
    let remote = match (&folder.remote_name, folder.is_label) {
        (Some(name), false) => name.clone(),
        _ => return Err(SyncError::FolderNotSynced(folder_id)),
    };
    let gmail = conn.caps.gmail();
    let mut labels = if gmail {
        Some(GmailFolders::load(shared).await?)
    } else {
        None
    };
    // Labels are taken from All Mail only: Trash and Spam aren't in any label.
    let apply_gmail_labels = gmail && folder.role == Some(FolderRole::All);
    let label_set: HashSet<FolderId> = labels
        .as_ref()
        .map(|g| g.labels.clone())
        .unwrap_or_default();

    let mut stored = folder.sync;
    let qresync = (conn.caps.qresync()
        && stored.uidnext.is_some()
        && stored.highestmodseq.is_some_and(|m| m > 0))
    .then(|| QresyncArgs {
        uidvalidity: stored.uidvalidity.unwrap_or_default(),
        highest_modseq: stored.highestmodseq.unwrap_or_default(),
        known_uids: None,
    })
    .filter(|q| q.uidvalidity != 0);
    let info = conn.select(&remote, qresync).await?;
    let mut report = PassReport::default();

    if let Some(old) = stored.uidvalidity
        && old != info.uidvalidity
    {
        tracing::info!(folder = %folder_id, "UIDVALIDITY changed; resetting the folder");
        let validity = info.uidvalidity;
        let reset_labels: Vec<FolderId> = if apply_gmail_labels {
            label_set.iter().copied().collect()
        } else {
            Vec::new()
        };
        let resets = reset_labels.clone();
        shared
            .write(1 + resets.len(), move |w| {
                w.reset_folder(folder_id, validity)?;
                for label in resets {
                    w.reset_folder(label, 0)?;
                }
                Ok(())
            })
            .await?;
        shared.events.emit(SyncEvent::FolderReset(folder_id));
        for label in reset_labels {
            shared.events.emit(SyncEvent::FolderReset(label));
        }
        stored = FolderSyncState {
            uidvalidity: Some(validity),
            uidnext: None,
            highestmodseq: None,
        };
    }
    let initial = stored.uidnext.is_none();

    // Nothing changed since the last pass.
    if !initial
        && !opts.force_uid_diff
        && info.uidnext.is_some()
        && info.uidnext == stored.uidnext
        && info.highest_modseq.is_some()
        && info.highest_modseq == stored.highestmodseq
        && u64::from(info.exists) == folder.total
    {
        report.complete = true;
        return Ok(report);
    }

    let local: HashMap<u32, Flags> = shared
        .read(move |r| r.folder_uids(folder_id))
        .await?
        .into_iter()
        .collect();
    let start = stored.uidnext.unwrap_or(1);
    let mut changes: Vec<FlagChange> = Vec::new();
    let mut vanished: BTreeSet<u32> = BTreeSet::new();
    let mut server_uids: Option<Vec<u32>> = None;
    let mut full_diff = false;

    if initial {
        let all = search_all(conn).await?;
        if !local.is_empty() {
            // Resuming an interrupted first sync: reconcile what is stored already.
            let present: HashSet<u32> = all.iter().copied().collect();
            vanished.extend(local.keys().filter(|u| !present.contains(u)));
            let kept: Vec<u32> = local
                .keys()
                .copied()
                .filter(|u| present.contains(u))
                .collect();
            changes = fetch_flags(conn, kept, shared.config.flag_chunk).await?;
        }
        server_uids = Some(all);
        report.did_uid_diff = true;
    } else if info.qresync_applied {
        vanished.extend(local.keys().filter(|u| info.vanished.contains(**u)));
        changes = info.changed.clone();
    } else if conn.caps.condstore()
        && let (Some(since), Some(now)) = (stored.highestmodseq, info.highest_modseq)
    {
        if since != now {
            let fetched = conn.session.fetch_changes(since).await;
            let set = conn.track(fetched)?;
            vanished.extend(local.keys().filter(|u| set.vanished.contains(**u)));
            changes = set.changed;
        }
        if opts.force_uid_diff {
            let all = search_all(conn).await?;
            let present: HashSet<u32> = all.iter().copied().collect();
            vanished.extend(local.keys().filter(|u| !present.contains(u)));
            server_uids = Some(all);
            report.did_uid_diff = true;
        }
    } else {
        let known: Vec<u32> = local.keys().copied().collect();
        let fetched = fetch_flags(conn, known, shared.config.flag_chunk).await?;
        let present: HashSet<u32> = fetched.iter().map(|c| c.uid).collect();
        vanished.extend(local.keys().filter(|u| !present.contains(u)));
        changes = fetched
            .into_iter()
            .filter(|c| local.get(&c.uid) != Some(&c.flags) || !c.keywords.is_empty())
            .collect();
        full_diff = true;
        report.did_uid_diff = true;
    }

    // New UIDs, newest first.
    let mut new_uids: Vec<u32> = match (&server_uids, info.uidnext) {
        (Some(all), _) => all
            .iter()
            .copied()
            .filter(|u| *u >= start && !local.contains_key(u))
            .collect(),
        (None, Some(next)) if next > start => {
            let span = (next - start) as usize;
            if span > 4 * shared.config.header_batch {
                // A sparse UID range: ask for the UIDs that exist.
                let all = search_all(conn).await?;
                let present: HashSet<u32> = all.iter().copied().collect();
                vanished.extend(local.keys().filter(|u| !present.contains(u)));
                report.did_uid_diff = true;
                let out = all
                    .iter()
                    .copied()
                    .filter(|u| *u >= start && !local.contains_key(u))
                    .collect();
                server_uids = Some(all);
                out
            } else {
                (start..next).filter(|u| !local.contains_key(u)).collect()
            }
        }
        (None, Some(_)) => Vec::new(),
        (None, None) => {
            let all = search_all(conn).await?;
            let out = all
                .iter()
                .copied()
                .filter(|u| *u >= start && !local.contains_key(u))
                .collect();
            server_uids = Some(all);
            out
        }
    };
    new_uids.sort_unstable_by(|a, b| b.cmp(a));
    // Only known messages get flag updates; unknown UIDs ≥ UIDNEXT are new mail.
    changes.retain(|c| local.contains_key(&c.uid) && !vanished.contains(&c.uid));
    let mut delta = Delta::default();
    let mut touched: BTreeSet<FolderId> = BTreeSet::from([folder_id]);
    apply_changes(
        shared,
        folder_id,
        changes,
        vanished.iter().copied().collect(),
        apply_gmail_labels,
        labels.as_mut(),
        &label_set,
        &mut delta,
    )
    .await?;

    let cutoff = shared.config.body_cutoff(shared.now());
    let mut max_seen = start.saturating_sub(1);
    let mut yielded = false;

    for (index, chunk) in new_uids
        .chunks(shared.config.header_batch.max(1))
        .enumerate()
    {
        if index > 0 && yield_flag.requested() {
            yielded = true;
            break;
        }
        let set: UidSet = chunk.iter().copied().collect();
        let fetched = conn.session.fetch_headers(&set).await;
        let mut headers = conn.track(fetched)?;
        headers.sort_unstable_by_key(|h| std::cmp::Reverse(h.uid));
        report.fetched += headers.len();
        if let Some(max) = headers.iter().map(|h| h.uid).max() {
            max_seen = max_seen.max(max);
        }
        if opts.claim_unassigned {
            claim_unassigned(shared, folder_id, &headers).await?;
        }
        let resolved: Vec<Vec<FolderId>> = match (&mut labels, apply_gmail_labels) {
            (Some(g), true) => {
                let lists: Vec<&[GmailLabel]> = headers
                    .iter()
                    .map(|h| h.gmail.as_ref().map_or(&[][..], |g| g.labels.as_slice()))
                    .collect();
                g.resolve(shared, &lists).await?
            }
            _ => vec![Vec::new(); headers.len()],
        };
        ingest_batch(
            shared,
            folder_id,
            headers,
            resolved,
            cutoff,
            &mut report,
            &mut delta,
            &mut touched,
        )
        .await?;
        std::mem::take(&mut delta).emit(&shared.events);
    }

    if !yielded {
        // A count mismatch means expunges the cheaper paths didn't report.
        let expected = local.len() - vanished.len() + report.fetched;
        if !initial && !full_diff && !report.did_uid_diff && expected != info.exists as usize {
            let all = search_all(conn).await?;
            let present: HashSet<u32> = all.iter().copied().collect();
            let extra: Vec<u32> = local
                .keys()
                .copied()
                .filter(|u| !present.contains(u) && !vanished.contains(u))
                .collect();
            report.did_uid_diff = true;
            apply_changes(
                shared,
                folder_id,
                Vec::new(),
                extra,
                apply_gmail_labels,
                labels.as_mut(),
                &label_set,
                &mut delta,
            )
            .await?;
            server_uids = Some(all);
        }
        let uidnext = info
            .uidnext
            .or_else(|| {
                server_uids
                    .as_ref()
                    .and_then(|all| all.iter().max())
                    .map(|m| m + 1)
            })
            .or(stored.uidnext)
            .unwrap_or(max_seen + 1)
            .max(max_seen + 1);
        let state = FolderSyncState {
            uidvalidity: Some(info.uidvalidity),
            uidnext: Some(uidnext),
            highestmodseq: info.highest_modseq,
        };
        shared
            .write(1, move |w| w.set_folder_sync_state(folder_id, state))
            .await?;
        report.complete = true;
    }

    touched.extend(delta.added.keys().copied());
    touched.extend(delta.changed.keys().copied());
    touched.extend(delta.removed.keys().copied());
    delta.emit(&shared.events);
    emit_counts(shared, touched).await?;
    Ok(report)
}

/// `UID SEARCH ALL`, ascending.
async fn search_all(conn: &mut Conn) -> Result<Vec<u32>> {
    let searched = conn.session.uid_search_all().await;
    let set = conn.track(searched)?;
    let mut uids: Vec<u32> = set
        .ranges()
        .iter()
        .filter(|r| r.end.is_some())
        .flat_map(|r| r.start..=r.end.unwrap_or(r.start))
        .collect();
    uids.sort_unstable();
    Ok(uids)
}

/// Flags of the given UIDs, in chunks.
async fn fetch_flags(conn: &mut Conn, mut uids: Vec<u32>, chunk: usize) -> Result<Vec<FlagChange>> {
    uids.sort_unstable();
    let mut out = Vec::with_capacity(uids.len());
    for part in uids.chunks(chunk.max(1)) {
        let set: UidSet = part.iter().copied().collect();
        let fetched = conn.session.fetch_flags(&set).await;
        out.extend(conn.track(fetched)?);
    }
    Ok(out)
}

/// Applies expunges and flag changes in one store transaction.
#[allow(clippy::too_many_arguments)]
async fn apply_changes(
    shared: &Shared,
    folder: FolderId,
    changes: Vec<FlagChange>,
    vanished: Vec<u32>,
    gmail_labels: bool,
    labels: Option<&mut GmailFolders>,
    label_set: &HashSet<FolderId>,
    delta: &mut Delta,
) -> Result<()> {
    if changes.is_empty() && vanished.is_empty() {
        return Ok(());
    }
    let resolved: Vec<Option<Vec<FolderId>>> = match (labels, gmail_labels) {
        (Some(g), true) => {
            let lists: Vec<&[GmailLabel]> = changes
                .iter()
                .map(|c| c.gmail_labels.as_deref().unwrap_or(&[]))
                .collect();
            let ids = g.resolve(shared, &lists).await?;
            changes
                .iter()
                .zip(ids)
                .map(|(c, ids)| c.gmail_labels.as_ref().map(|_| ids))
                .collect()
        }
        _ => vec![None; changes.len()],
    };
    let mut label_set = label_set.clone();
    if gmail_labels {
        for ids in resolved.iter().flatten() {
            label_set.extend(ids.iter().copied());
        }
    }
    let weight = changes.len() * 2 + vanished.len();
    let job_delta = shared
        .write(weight, move |w| {
            let mut delta = Delta::default();
            if !vanished.is_empty() {
                for message in w.expunge(folder, &vanished)? {
                    delta.removed(folder, message);
                    if gmail_labels {
                        drop_labels(w, message, &label_set, &mut delta)?;
                    }
                }
            }
            for (change, labels) in changes.into_iter().zip(resolved) {
                let updated = w.update_location_flags(
                    folder,
                    change.uid,
                    change.flags,
                    Some(&change.keywords),
                    change.modseq,
                )?;
                let Some(message) = updated else { continue };
                delta.changed(folder, message);
                if gmail_labels {
                    let desired = match labels {
                        Some(ids) => ids,
                        // No label data: keep the labels, follow the flags.
                        None => w
                            .message(message)?
                            .map(|d| {
                                d.locations
                                    .iter()
                                    .filter(|l| label_set.contains(&l.folder))
                                    .map(|l| l.folder)
                                    .collect()
                            })
                            .unwrap_or_default(),
                    };
                    apply_labels(
                        w,
                        message,
                        &desired,
                        change.flags,
                        &change.keywords,
                        &label_set,
                        &mut delta,
                    )?;
                }
            }
            Ok(delta)
        })
        .await?;
    delta.merge(job_delta);
    Ok(())
}

/// Hands one batch of headers to the ingest pipeline; tries body reuse for new
/// messages and collects body candidates.
#[allow(clippy::too_many_arguments)]
async fn ingest_batch(
    shared: &Shared,
    folder: FolderId,
    headers: Vec<HeaderFetch>,
    labels: Vec<Vec<FolderId>>,
    cutoff: Option<i64>,
    report: &mut PassReport,
    delta: &mut Delta,
    touched: &mut BTreeSet<FolderId>,
) -> Result<()> {
    if headers.is_empty() {
        return Ok(());
    }
    let account = shared.account();
    let now = shared.now();
    let mut meta = Vec::with_capacity(headers.len());
    let inputs: Vec<HeaderInput> = headers
        .into_iter()
        .zip(labels)
        .map(|(h, labels)| {
            let date = h.internal_date.map_or(now, |d| d.unix_timestamp());
            meta.push((h.size, date, h.header.clone(), labels.clone()));
            HeaderInput {
                account,
                location: NewLocation {
                    folder,
                    uid: Some(h.uid),
                    modseq: h.modseq,
                    flags: h.flags,
                    keywords: h.keywords,
                },
                labels,
                header: h.header,
                body: h.body,
                size: h.size,
                gmail: h.gmail,
                internal_date: h.internal_date,
            }
        })
        .collect();
    let expected = inputs.len();
    let outcomes = shared.ingest.ingest_headers_batch(inputs).await?;
    if outcomes.len() != expected {
        return Err(SyncError::Internal(format!(
            "ingest returned {} results for {expected} headers",
            outcomes.len()
        )));
    }
    for ((size, date, header, labels), outcome) in meta.into_iter().zip(outcomes) {
        let message = outcome.message;
        delta.added(folder, message);
        for label in labels {
            delta.added(label, message);
            touched.insert(label);
        }
        if !outcome.created {
            continue;
        }
        let query = ReuseQuery {
            message,
            rfc_message_id: None,
            size: Some(u64::from(size)),
            header: Some(header),
        };
        if let Some(raw) = shared.ingest.lookup_reused_body(&query).await? {
            shared
                .ingest
                .attach_body(message, raw, store::BodyState::Full)
                .await?;
            shared.events.emit(SyncEvent::BodyFetched(message));
        } else if cutoff.is_some_and(|c| date >= c) {
            report.body_candidates.push((message, date));
        }
    }
    Ok(())
}

/// Gives locations without a UID (an op's optimistic move/append) the UID of the
/// matching new header, by Message-ID, so ingest updates them instead of creating a
/// second message.
async fn claim_unassigned(
    shared: &Shared,
    folder: FolderId,
    headers: &[HeaderFetch],
) -> Result<()> {
    let ids: Vec<(u32, String)> = headers
        .iter()
        .filter_map(|h| {
            let parsed = mazemail_mime::parse_headers_only(h.header.expose()).ok()?;
            Some((h.uid, parsed.message_id?))
        })
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let account = shared.account();
    shared
        .write(ids.len(), move |w| {
            for (uid, rfc_id) in ids {
                if w.message_by_uid(folder, uid)?.is_some() {
                    continue;
                }
                for candidate in w.messages_by_rfc_id(account, &rfc_id)? {
                    let pending = w.message(candidate)?.is_some_and(|d| {
                        d.locations
                            .iter()
                            .any(|l| l.folder == folder && l.uid.is_none())
                    });
                    if pending {
                        w.set_location_uid(candidate, folder, uid)?;
                        break;
                    }
                }
            }
            Ok(())
        })
        .await
}

/// Emits the counts of the given folders.
pub(crate) async fn emit_counts(shared: &Shared, folders: BTreeSet<FolderId>) -> Result<()> {
    let ids: Vec<FolderId> = folders.into_iter().collect();
    let counts = shared
        .read(move |r| {
            ids.iter()
                .map(|id| r.folder(*id).map(|f| f.map(|f| (f.id, f.unread, f.total))))
                .collect::<store::Result<Vec<_>>>()
        })
        .await?;
    for (folder, unread, total) in counts.into_iter().flatten() {
        shared.events.emit(SyncEvent::Counts {
            folder,
            counts: FolderCounts { unread, total },
        });
    }
    Ok(())
}
