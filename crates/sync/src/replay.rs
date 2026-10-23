//! Pending-op replay: translates [`OpPayload`]s into IMAP (or Gmail) commands, in
//! queue order (see [`crate::ops`] for the contract).

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::time::Duration;

use imap::{FlagOp, GmailLabel, LabelOp, UidSet};
use mazemail_types::{BlobId, Flags, FolderId, MessageId};
use store::{OpState, PendingOp, Queries};

use crate::backoff::jitter;
use crate::connect::Conn;
use crate::error::{Result, SyncError};
use crate::events::{OpEvent, SyncEvent};
use crate::gmail::GmailFolders;
use crate::ops::{ArchiveTarget, FlagTarget, LocationRef, OpPayload};
use crate::shared::{Shared, YieldFlag};

/// What a replay round did.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReplayReport {
    /// When the oldest queued op is due next (Unix seconds), if it waits for a retry.
    pub(crate) next_due: Option<i64>,
    /// Folders whose server state changed: they get a pass.
    pub(crate) touched: BTreeSet<FolderId>,
    /// Folders with locations whose UID the engine couldn't learn: their next pass
    /// matches new mail by Message-ID.
    pub(crate) unresolved: BTreeSet<FolderId>,
    /// A folder was created, renamed or deleted: the folder list needs a sync.
    pub(crate) folders_changed: bool,
}

#[derive(Debug, Default)]
struct OpOutcome {
    /// `(message, folder, uid)` learned from COPYUID/APPENDUID or a Message-ID search.
    uids: Vec<(MessageId, FolderId, u32)>,
    touched: BTreeSet<FolderId>,
    unresolved: BTreeSet<FolderId>,
    folders_changed: bool,
}

/// Replays the account's due ops in order until the queue is empty, the oldest op
/// waits for its retry time, or the job should yield.
///
/// Connection-level errors end the round with `Err` and leave the op queued without
/// counting an attempt; any other failure counts an attempt (see
/// [`store::WriteTx::op_failed`]).
pub(crate) async fn replay_ops(
    shared: &Shared,
    conn: &mut Conn,
    yield_flag: &YieldFlag,
) -> Result<ReplayReport> {
    let account = shared.account();
    let mut report = ReplayReport::default();
    let mut attempted = BTreeSet::new();
    loop {
        let now = shared.now();
        let Some(op) = shared
            .read(move |r| r.next_pending_op(account, now))
            .await?
        else {
            break;
        };
        // One attempt per op and round, even if the clock says it's due again.
        if !attempted.insert(op.id) {
            break;
        }
        match replay_one(shared, conn, &op).await {
            Ok(outcome) => {
                finish_op(shared, &op, &outcome).await?;
                report.touched.extend(outcome.touched);
                report.unresolved.extend(outcome.unresolved);
                report.folders_changed |= outcome.folders_changed;
                shared.events.emit(SyncEvent::Op(OpEvent::Done(op.id)));
                tracing::debug!(op = %op.id, kind = %op.kind, "pending op replayed");
            }
            Err(err) if err.is_transient() || err.breaks_session() || is_store_closed(&err) => {
                return Err(err);
            }
            Err(err) => {
                let text = err.to_string();
                let id = op.id;
                let spread = i64::try_from(jitter(Duration::from_secs(15)).as_secs()).unwrap_or(0);
                let message = text.clone();
                let state = shared
                    .write(1, move |w| w.op_failed(id, &message, spread))
                    .await?;
                tracing::warn!(op = %id, kind = %op.kind, error = %text, ?state, "pending op failed");
                shared.events.emit(SyncEvent::Op(match state {
                    OpState::Stuck => OpEvent::Stuck {
                        op: id,
                        error: text,
                    },
                    OpState::Queued => OpEvent::Failed {
                        op: id,
                        error: text,
                        attempts: op.attempts.saturating_add(1),
                    },
                }));
            }
        }
        if yield_flag.requested() {
            break;
        }
    }
    let now = shared.now();
    let remaining = shared.read(move |r| r.pending_ops(account)).await?;
    report.next_due = remaining
        .iter()
        .find(|op| op.state == OpState::Queued)
        .map(|op| op.next_attempt)
        .filter(|due| *due > now);
    // Folders that still expect locations from queued ops: their passes match new
    // mail by Message-ID before ingesting it.
    for op in &remaining {
        if let Ok(payload) = OpPayload::from_pending(op) {
            report.unresolved.extend(payload.destinations());
        }
    }
    Ok(report)
}

fn is_store_closed(err: &SyncError) -> bool {
    matches!(err, SyncError::Store(store::Error::Closed))
}

/// Removes the op, records learned UIDs locally and in later ops.
async fn finish_op(shared: &Shared, op: &PendingOp, outcome: &OpOutcome) -> Result<()> {
    let id = op.id;
    let account = shared.account();
    let uids = outcome.uids.clone();
    shared
        .write(1 + uids.len(), move |w| {
            w.op_done(id)?;
            if uids.is_empty() {
                return Ok(());
            }
            for (message, folder, uid) in &uids {
                match w.set_location_uid(*message, *folder, *uid) {
                    Ok(()) | Err(store::Error::NotFound { .. }) => {}
                    Err(err) => return Err(err),
                }
            }
            for later in w.pending_ops(account)? {
                let Ok(mut payload) = OpPayload::from_pending(&later) else {
                    continue;
                };
                let mut changed = false;
                for (message, folder, uid) in &uids {
                    changed |= payload.fill_uid(*message, *folder, *uid);
                }
                if changed {
                    w.op_update_payload(later.id, &payload.to_json()?)?;
                }
            }
            Ok(())
        })
        .await
}

async fn replay_one(shared: &Shared, conn: &mut Conn, op: &PendingOp) -> Result<OpOutcome> {
    let payload = OpPayload::from_pending(op).map_err(|e| SyncError::InvalidOp(e.to_string()))?;
    let gmail = if conn.caps.gmail() {
        Some(GmailFolders::load(shared).await?)
    } else {
        None
    };
    let mut replayer = Replayer {
        shared,
        conn,
        gmail,
        out: OpOutcome::default(),
    };
    replayer.run(payload).await?;
    Ok(replayer.out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transfer {
    Move,
    Copy,
}

struct Replayer<'a> {
    shared: &'a Shared,
    conn: &'a mut Conn,
    gmail: Option<GmailFolders>,
    out: OpOutcome,
}

impl<'a> Replayer<'a> {
    async fn run(&mut self, payload: OpPayload) -> Result<()> {
        let gmail = self.gmail.is_some();
        match payload {
            OpPayload::SetFlags { targets } => self.set_flags(targets).await,
            OpPayload::Move { items, to } if gmail => self.gm_move(&items, to).await,
            OpPayload::Move { items, to } => self.transfer(&items, to, Transfer::Move).await,
            OpPayload::Copy { items, to } if gmail => self.gm_copy(&items, to).await,
            OpPayload::Copy { items, to } => self.transfer(&items, to, Transfer::Copy).await,
            OpPayload::Delete {
                items,
                permanent,
                trash,
            } => {
                if gmail {
                    self.gm_delete(&items, permanent).await
                } else {
                    self.delete(&items, permanent, trash).await
                }
            }
            OpPayload::Archive { items, target } => match (gmail, target) {
                (true, _) => self.gm_archive(&items).await,
                (false, ArchiveTarget::Folder(to)) => {
                    self.transfer(&items, to, Transfer::Move).await
                }
                (false, ArchiveTarget::GmailRemoveInbox) => Err(SyncError::InvalidOp(
                    "gmail_remove_inbox on an account that is not Gmail".into(),
                )),
            },
            OpPayload::Junk { items, to } => self.junk(&items, to, true).await,
            OpPayload::NotJunk { items, to } => self.junk(&items, to, false).await,
            OpPayload::Labels {
                messages,
                add,
                remove,
            } => self.gm_labels(&messages, &add, &remove).await,
            OpPayload::CreateFolder {
                name, subscribe, ..
            } => self.create_folder(&name, subscribe).await,
            OpPayload::RenameFolder { from, to, .. } => self.rename_folder(&from, &to).await,
            OpPayload::DeleteFolder { name, .. } => self.delete_folder(&name).await,
            OpPayload::Subscribe { name, subscribed } => {
                let result = self.conn.session.subscribe(&name, subscribed).await;
                self.conn.track(result)?;
                self.out.folders_changed = true;
                Ok(())
            }
            OpPayload::AppendDraft {
                folder,
                blob,
                flags,
                message,
                replaces,
            } => {
                self.append(folder, blob, flags | Flags::DRAFT, message, replaces)
                    .await
            }
            OpPayload::Append {
                folder,
                blob,
                flags,
            } => self.append(folder, blob, flags, None, None).await,
        }
    }

    // ---- lookups ------------------------------------------------------------------

    /// The server name of a folder (a mailbox, or a Gmail label that is listed as one).
    fn remote(&self, id: FolderId) -> impl Future<Output = Result<String>> + Send + 'a {
        let shared = self.shared;
        async move {
            folder_of(shared, id)
                .await?
                .remote_name
                .filter(|n| !n.starts_with('\\'))
                .ok_or(SyncError::FolderNotSynced(id))
        }
    }

    async fn select(&mut self, folder: FolderId) -> Result<()> {
        let remote = self.remote(folder).await?;
        self.conn.ensure_selected(&remote).await?;
        Ok(())
    }

    /// UIDs for the locations, looked up in the store where the op didn't have one.
    /// Locations without a server copy are skipped.
    fn resolve(
        &self,
        items: &[LocationRef],
    ) -> impl Future<Output = Result<Vec<(MessageId, FolderId, u32)>>> + Send + 'a {
        let items = items.to_vec();
        let shared = self.shared;
        async move {
            shared
                .read(move |r| {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        let uid = match item.uid {
                            Some(uid) => Some(uid),
                            None => r.message(item.message)?.and_then(|d| {
                                d.locations
                                    .iter()
                                    .find(|l| l.folder == item.folder)
                                    .and_then(|l| l.uid)
                            }),
                        };
                        match uid {
                            Some(uid) => out.push((item.message, item.folder, uid)),
                            None => tracing::debug!(
                                message = %item.message,
                                folder = %item.folder,
                                "op location has no UID; skipped"
                            ),
                        }
                    }
                    Ok(out)
                })
                .await
        }
    }

    /// Resolved locations grouped by folder.
    fn grouped(
        &self,
        items: &[LocationRef],
    ) -> impl Future<Output = Result<BTreeMap<FolderId, Vec<(MessageId, u32)>>>> + Send + 'a {
        let resolving = self.resolve(items);
        async move {
            let mut groups: BTreeMap<FolderId, Vec<(MessageId, u32)>> = BTreeMap::new();
            for (message, folder, uid) in resolving.await? {
                groups.entry(folder).or_default().push((message, uid));
            }
            Ok(groups)
        }
    }

    fn gmail(&self) -> Result<&GmailFolders> {
        self.gmail.as_ref().ok_or_else(|| {
            SyncError::InvalidOp("a Gmail op on an account that is not Gmail".into())
        })
    }

    fn all_mail(&self) -> Result<FolderId> {
        self.gmail()?
            .all_mail
            .ok_or_else(|| SyncError::InvalidOp("the account has no All Mail folder".into()))
    }

    /// Each message's UID in All Mail.
    fn all_mail_uids(
        &self,
        messages: Vec<MessageId>,
    ) -> impl Future<Output = Result<Vec<(MessageId, u32)>>> + Send + 'a {
        let all_mail = self.all_mail();
        let shared = self.shared;
        async move {
            let all_mail = all_mail?;
            shared
                .read(move |r| {
                    let mut out = Vec::new();
                    for m in messages {
                        let uid = r.message(m)?.and_then(|d| {
                            d.locations
                                .iter()
                                .find(|l| l.folder == all_mail)
                                .and_then(|l| l.uid)
                        });
                        if let Some(uid) = uid {
                            out.push((m, uid));
                        }
                    }
                    Ok(out)
                })
                .await
        }
    }

    fn label(&self, folder: FolderId) -> Result<GmailLabel> {
        self.gmail()?
            .label_of(folder)
            .ok_or_else(|| SyncError::InvalidOp(format!("folder {folder} is not a Gmail label")))
    }

    // ---- flags --------------------------------------------------------------------

    async fn set_flags(&mut self, targets: Vec<FlagTarget>) -> Result<()> {
        let locations: Vec<LocationRef> = targets.iter().map(FlagTarget::location).collect();
        let mut wanted: BTreeMap<(MessageId, FolderId), (Flags, Vec<String>)> = BTreeMap::new();
        for t in &targets {
            let mut keywords: Vec<String> = t.keywords.clone();
            keywords.sort();
            keywords.dedup();
            wanted.insert((t.message, t.folder), (t.flags, keywords));
        }
        // Gmail keeps one flag state per message: label locations act on All Mail.
        let mut resolved = Vec::new();
        if let Some(g) = &self.gmail {
            let all_mail = self.all_mail()?;
            let (in_all, in_boxes): (Vec<LocationRef>, Vec<LocationRef>) = locations
                .iter()
                .partition(|l| g.is_label(l.folder) || l.folder == all_mail);
            let messages: Vec<MessageId> = in_all.iter().map(|l| l.message).collect();
            let uids = self.all_mail_uids(messages).await?;
            for (m, uid) in uids {
                if let Some(l) = in_all.iter().find(|l| l.message == m) {
                    resolved.push((m, l.folder, all_mail, uid));
                }
            }
            for (m, f, uid) in self.resolve(&in_boxes).await? {
                resolved.push((m, f, f, uid));
            }
        } else {
            for (m, f, uid) in self.resolve(&locations).await? {
                resolved.push((m, f, f, uid));
            }
        }
        let mut groups: BTreeMap<(FolderId, u32, Vec<String>), Vec<u32>> = BTreeMap::new();
        for (message, origin, target_folder, uid) in resolved {
            if let Some((flags, keywords)) = wanted.get(&(message, origin)) {
                groups
                    .entry((target_folder, flags.bits(), keywords.clone()))
                    .or_default()
                    .push(uid);
            }
        }
        for ((folder, bits, keywords), uids) in groups {
            self.select(folder).await?;
            let op = FlagOp::set(Flags::from_bits_retain(bits)).with_keywords(keywords);
            let set: UidSet = uids.into_iter().collect();
            let result = self.conn.session.store_flags(&set, &op).await;
            self.conn.track(result)?;
            self.out.touched.insert(folder);
        }
        Ok(())
    }

    async fn store(&mut self, folder: FolderId, uids: &[u32], op: FlagOp) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        self.select(folder).await?;
        let set: UidSet = uids.iter().copied().collect();
        let result = self.conn.session.store_flags(&set, &op).await;
        self.conn.track(result)?;
        self.out.touched.insert(folder);
        Ok(())
    }

    /// `+FLAGS (\Deleted)` and, with UIDPLUS, `UID EXPUNGE` of exactly these UIDs.
    async fn expunge(&mut self, folder: FolderId, uids: &[u32]) -> Result<()> {
        self.store(folder, uids, FlagOp::add(Flags::DELETED))
            .await?;
        if self.conn.caps.uidplus() && !uids.is_empty() {
            let set: UidSet = uids.iter().copied().collect();
            let result = self.conn.session.expunge_uids(&set).await;
            self.conn.track(result)?;
        }
        Ok(())
    }

    // ---- move / copy ----------------------------------------------------------------

    /// IMAP move or copy, with UID remapping from COPYUID or a Message-ID search.
    async fn transfer(
        &mut self,
        items: &[LocationRef],
        to: FolderId,
        mode: Transfer,
    ) -> Result<()> {
        let dest = self.remote(to).await?;
        for (source, entries) in self.grouped(items).await? {
            if source == to {
                continue;
            }
            self.select(source).await?;
            let before = if self.conn.caps.uidplus() {
                None
            } else {
                let status = self.conn.session.status(&dest).await;
                self.conn.track(status).ok().and_then(|s| s.uidnext)
            };
            let set: UidSet = entries.iter().map(|(_, uid)| *uid).collect();
            let result = match mode {
                Transfer::Move => self.conn.session.r#move(&set, &dest).await,
                Transfer::Copy => self.conn.session.copy(&set, &dest).await,
            };
            let copied = self.conn.track(result)?;
            self.out.touched.insert(source);
            self.out.touched.insert(to);
            let mut unmapped = Vec::new();
            for (message, uid) in entries {
                match copied.as_ref().and_then(|c| c.destination_of(uid)) {
                    Some(new_uid) => self.out.uids.push((message, to, new_uid)),
                    None => unmapped.push(message),
                }
            }
            if !unmapped.is_empty() {
                match before {
                    Some(from_uid) => {
                        self.find_by_message_id(to, &dest, from_uid, unmapped)
                            .await?
                    }
                    None => {
                        self.out.unresolved.insert(to);
                    }
                }
            }
        }
        Ok(())
    }

    /// Without UIDPLUS: finds the copies among the destination's new UIDs by
    /// Message-ID (PLAN §4.5).
    async fn find_by_message_id(
        &mut self,
        folder: FolderId,
        remote: &str,
        from_uid: u32,
        messages: Vec<MessageId>,
    ) -> Result<()> {
        let wanted: Vec<(MessageId, Option<String>)> = self
            .shared
            .read(move |r| {
                messages
                    .into_iter()
                    .map(|m| Ok((m, r.message(m)?.and_then(|d| d.rfc_message_id))))
                    .collect()
            })
            .await?;
        self.conn.select(remote, None).await?;
        let fetched = self
            .conn
            .session
            .fetch_headers(&UidSet::from_uid_onwards(from_uid))
            .await;
        let headers = self.conn.track(fetched)?;
        let found: Vec<(u32, String)> = headers
            .iter()
            .filter(|h| h.uid >= from_uid)
            .filter_map(|h| {
                let parsed = mazemail_mime::parse_headers_only(h.header.expose()).ok()?;
                Some((h.uid, parsed.message_id?))
            })
            .collect();
        for (message, rfc_id) in wanted {
            let hit = rfc_id
                .as_deref()
                .and_then(|id| found.iter().find(|(_, f)| f == id).map(|(uid, _)| *uid));
            match hit {
                Some(uid) => self.out.uids.push((message, folder, uid)),
                None => {
                    self.out.unresolved.insert(folder);
                }
            }
        }
        Ok(())
    }

    async fn delete(
        &mut self,
        items: &[LocationRef],
        permanent: bool,
        trash: Option<FolderId>,
    ) -> Result<()> {
        for (source, entries) in self.grouped(items).await? {
            let refs: Vec<LocationRef> = entries
                .iter()
                .map(|(m, uid)| LocationRef::new(*m, source, Some(*uid)))
                .collect();
            let uids: Vec<u32> = entries.iter().map(|(_, uid)| *uid).collect();
            match trash {
                Some(t) if !permanent && t != source => {
                    self.transfer(&refs, t, Transfer::Move).await?;
                }
                // No trash folder: mark deleted (IMAP's own model), don't expunge.
                None if !permanent => {
                    self.store(source, &uids, FlagOp::add(Flags::DELETED))
                        .await?;
                }
                _ => self.expunge(source, &uids).await?,
            }
        }
        Ok(())
    }

    async fn junk(
        &mut self,
        items: &[LocationRef],
        to: Option<FolderId>,
        junk: bool,
    ) -> Result<()> {
        if self.gmail.is_some() {
            let target = if junk {
                self.gmail()?.spam
            } else {
                to.or_else(|| {
                    self.gmail.as_ref().and_then(|g| {
                        g.labels.iter().copied().find(|l| {
                            g.folder(*l)
                                .is_some_and(|f| f.role == Some(mazemail_types::FolderRole::Inbox))
                        })
                    })
                })
            };
            let target = target.ok_or_else(|| {
                SyncError::InvalidOp("no Spam or Inbox folder for a Gmail junk op".into())
            })?;
            return self.gm_move(items, target).await;
        }
        let (add, remove) = if junk {
            (Flags::JUNK, Flags::NOT_JUNK)
        } else {
            (Flags::NOT_JUNK, Flags::JUNK)
        };
        for (source, entries) in self.grouped(items).await? {
            let uids: Vec<u32> = entries.iter().map(|(_, uid)| *uid).collect();
            self.store(source, &uids, FlagOp::add(add)).await?;
            self.store(source, &uids, FlagOp::remove(remove)).await?;
            if let Some(to) = to.filter(|t| *t != source) {
                let refs: Vec<LocationRef> = entries
                    .iter()
                    .map(|(m, uid)| LocationRef::new(*m, source, Some(*uid)))
                    .collect();
                self.transfer(&refs, to, Transfer::Move).await?;
            }
        }
        Ok(())
    }

    // ---- Gmail ------------------------------------------------------------------------

    async fn labels_op(&mut self, uids: &[u32], op: LabelOp) -> Result<()> {
        if uids.is_empty() || op.labels.is_empty() {
            return Ok(());
        }
        let all_mail = self.all_mail()?;
        self.select(all_mail).await?;
        let set: UidSet = uids.iter().copied().collect();
        let result = self.conn.session.store_gmail_labels(&set, &op).await;
        self.conn.track(result)?;
        self.out.touched.insert(all_mail);
        Ok(())
    }

    /// Splits locations into those "in All Mail" (label folders and All Mail itself)
    /// and those in the Trash/Spam mailboxes.
    fn split_gmail(&self, items: &[LocationRef]) -> Result<(Vec<LocationRef>, Vec<LocationRef>)> {
        let g = self.gmail()?;
        let all_mail = self.all_mail()?;
        Ok(items
            .iter()
            .partition(|l| g.is_label(l.folder) || l.folder == all_mail))
    }

    /// Gmail move (PLAN §4.5): labels are label changes, Trash/Spam are mailboxes.
    async fn gm_move(&mut self, items: &[LocationRef], to: FolderId) -> Result<()> {
        let all_mail = self.all_mail()?;
        let to_label = self.gmail()?.is_label(to);
        let (in_all, in_boxes) = self.split_gmail(items)?;

        if !in_all.is_empty() {
            let uids = self
                .all_mail_uids(in_all.iter().map(|l| l.message).collect())
                .await?;
            if to_label || to == all_mail {
                if to_label {
                    let label = self.label(to)?;
                    let all: Vec<u32> = uids.iter().map(|(_, u)| *u).collect();
                    self.labels_op(&all, LabelOp::add([label])).await?;
                    self.out.touched.insert(to);
                }
                // Remove the source label (moving out of All Mail itself only adds).
                let mut by_source: BTreeMap<FolderId, Vec<u32>> = BTreeMap::new();
                for l in in_all
                    .iter()
                    .filter(|l| l.folder != all_mail && l.folder != to)
                {
                    if let Some((_, uid)) = uids.iter().find(|(m, _)| *m == l.message) {
                        by_source.entry(l.folder).or_default().push(*uid);
                    }
                }
                for (source, source_uids) in by_source {
                    let label = self.label(source)?;
                    self.labels_op(&source_uids, LabelOp::remove([label]))
                        .await?;
                    self.out.touched.insert(source);
                }
            } else {
                // To Trash or Spam: a real move out of All Mail.
                let refs: Vec<LocationRef> = uids
                    .iter()
                    .map(|(m, uid)| LocationRef::new(*m, all_mail, Some(*uid)))
                    .collect();
                self.transfer(&refs, to, Transfer::Move).await?;
            }
        }

        if !in_boxes.is_empty() {
            if to_label || to == all_mail {
                // Out of Trash/Spam: back into All Mail, then label.
                let before = self.out.uids.len();
                self.transfer(&in_boxes, all_mail, Transfer::Move).await?;
                if to_label {
                    let label = self.label(to)?;
                    let new_uids: Vec<u32> = self.out.uids[before..]
                        .iter()
                        .filter(|(_, f, _)| *f == all_mail)
                        .map(|(_, _, u)| *u)
                        .collect();
                    self.labels_op(&new_uids, LabelOp::add([label])).await?;
                    self.out.touched.insert(to);
                }
            } else {
                self.transfer(&in_boxes, to, Transfer::Move).await?;
            }
        }
        Ok(())
    }

    async fn gm_copy(&mut self, items: &[LocationRef], to: FolderId) -> Result<()> {
        let all_mail = self.all_mail()?;
        if self.gmail()?.is_label(to) {
            let label = self.label(to)?;
            let uids = self
                .all_mail_uids(items.iter().map(|l| l.message).collect())
                .await?;
            let all: Vec<u32> = uids.iter().map(|(_, u)| *u).collect();
            self.labels_op(&all, LabelOp::add([label])).await?;
            self.out.touched.insert(to);
            return Ok(());
        }
        let (in_all, in_boxes) = self.split_gmail(items)?;
        let uids = self
            .all_mail_uids(in_all.iter().map(|l| l.message).collect())
            .await?;
        let mut refs: Vec<LocationRef> = uids
            .iter()
            .map(|(m, uid)| LocationRef::new(*m, all_mail, Some(*uid)))
            .collect();
        refs.extend(in_boxes);
        self.transfer(&refs, to, Transfer::Copy).await
    }

    /// Gmail delete: move to `[Gmail]/Trash` (a `\Deleted` + EXPUNGE in a label would
    /// only remove the label); in Trash or Spam, or `permanent`, delete for good.
    async fn gm_delete(&mut self, items: &[LocationRef], permanent: bool) -> Result<()> {
        let trash = self
            .gmail()?
            .trash
            .ok_or_else(|| SyncError::InvalidOp("the account has no Trash folder".into()))?;
        let all_mail = self.all_mail()?;
        let (in_all, in_boxes) = self.split_gmail(items)?;
        for (source, entries) in self.grouped(&in_boxes).await? {
            let uids: Vec<u32> = entries.iter().map(|(_, u)| *u).collect();
            self.expunge(source, &uids).await?;
        }
        if !in_all.is_empty() {
            let uids = self
                .all_mail_uids(in_all.iter().map(|l| l.message).collect())
                .await?;
            let refs: Vec<LocationRef> = uids
                .iter()
                .map(|(m, uid)| LocationRef::new(*m, all_mail, Some(*uid)))
                .collect();
            let before = self.out.uids.len();
            self.transfer(&refs, trash, Transfer::Move).await?;
            if permanent {
                let in_trash: Vec<u32> = self.out.uids[before..]
                    .iter()
                    .filter(|(_, f, _)| *f == trash)
                    .map(|(_, _, u)| *u)
                    .collect();
                self.expunge(trash, &in_trash).await?;
            }
        }
        Ok(())
    }

    /// Gmail archive: `-X-GM-LABELS (\Inbox)`.
    async fn gm_archive(&mut self, items: &[LocationRef]) -> Result<()> {
        let uids = self
            .all_mail_uids(items.iter().map(|l| l.message).collect())
            .await?;
        let all: Vec<u32> = uids.iter().map(|(_, u)| *u).collect();
        self.labels_op(&all, LabelOp::archive()).await?;
        if let Some(g) = &self.gmail {
            for l in items.iter().filter(|l| g.is_label(l.folder)) {
                self.out.touched.insert(l.folder);
            }
        }
        Ok(())
    }

    async fn gm_labels(
        &mut self,
        messages: &[MessageId],
        add: &[FolderId],
        remove: &[FolderId],
    ) -> Result<()> {
        let add_labels = add
            .iter()
            .map(|f| self.label(*f))
            .collect::<Result<Vec<_>>>()?;
        let remove_labels = remove
            .iter()
            .map(|f| self.label(*f))
            .collect::<Result<Vec<_>>>()?;
        let uids = self.all_mail_uids(messages.to_vec()).await?;
        let all: Vec<u32> = uids.iter().map(|(_, u)| *u).collect();
        self.labels_op(&all, LabelOp::add(add_labels)).await?;
        self.labels_op(&all, LabelOp::remove(remove_labels)).await?;
        self.out.touched.extend(add.iter().chain(remove).copied());
        Ok(())
    }

    // ---- folders ------------------------------------------------------------------------

    async fn folder_exists(&mut self, name: &str) -> Result<bool> {
        let listed = self.conn.session.list().await;
        Ok(self.conn.track(listed)?.iter().any(|f| f.name == name))
    }

    async fn create_folder(&mut self, name: &str, subscribe: bool) -> Result<()> {
        let result = self.conn.session.create(name).await;
        match self.conn.track(result) {
            Ok(()) => {}
            Err(err) if is_no(&err) && self.folder_exists(name).await? => {}
            Err(err) => return Err(err),
        }
        if subscribe {
            let result = self.conn.session.subscribe(name, true).await;
            self.conn.track(result)?;
        }
        self.out.folders_changed = true;
        Ok(())
    }

    async fn rename_folder(&mut self, from: &str, to: &str) -> Result<()> {
        if self.conn.selected_folder() == Some(from) {
            self.conn.forget_selection();
        }
        let result = self.conn.session.rename(from, to).await;
        match self.conn.track(result) {
            Ok(()) => {}
            Err(err)
                if is_no(&err)
                    && !self.folder_exists(from).await?
                    && self.folder_exists(to).await? => {}
            Err(err) => return Err(err),
        }
        self.out.folders_changed = true;
        Ok(())
    }

    async fn delete_folder(&mut self, name: &str) -> Result<()> {
        if self.conn.selected_folder() == Some(name) {
            self.conn.forget_selection();
        }
        let result = self.conn.session.delete(name).await;
        match self.conn.track(result) {
            Ok(()) => {}
            Err(err) if is_no(&err) && !self.folder_exists(name).await? => {}
            Err(err) => return Err(err),
        }
        self.out.folders_changed = true;
        Ok(())
    }

    // ---- drafts and sent copies ----------------------------------------------------------

    /// `APPEND`s a stored message with exactly `flags`.
    async fn append(
        &mut self,
        folder: FolderId,
        blob: BlobId,
        flags: Flags,
        message: Option<MessageId>,
        replaces: Option<LocationRef>,
    ) -> Result<()> {
        let remote = self.remote(folder).await?;
        let store = self.shared.store.clone();
        let raw = tokio::task::spawn_blocking(move || store.blobs().read(&blob))
            .await
            .map_err(|e| SyncError::Internal(format!("blob read task: {e}")))??;
        let result = self.conn.session.append(&remote, &raw, flags).await;
        let appended = self.conn.track(result)?;
        self.out.touched.insert(folder);
        let label_folder = self.gmail.as_ref().is_some_and(|g| g.is_label(folder));
        if let Some(message) = message
            && !label_folder
        {
            match appended {
                Some(a) => self.out.uids.push((message, folder, a.uid)),
                None => {
                    self.out.unresolved.insert(folder);
                }
            }
        }
        if let Some(old) = replaces {
            if self.gmail.is_some() {
                self.gm_delete(&[old], true).await?;
            } else {
                for (source, entries) in self.grouped(&[old]).await? {
                    let uids: Vec<u32> = entries.iter().map(|(_, u)| *u).collect();
                    self.expunge(source, &uids).await?;
                }
            }
        }
        Ok(())
    }
}

async fn folder_of(shared: &Shared, id: FolderId) -> Result<store::Folder> {
    shared
        .read(move |r| r.folder(id))
        .await?
        .ok_or(SyncError::FolderNotSynced(id))
}

fn is_no(err: &SyncError) -> bool {
    matches!(err, SyncError::Imap(imap::ImapError::No { .. }))
}
