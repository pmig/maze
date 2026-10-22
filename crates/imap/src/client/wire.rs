//! Low-level command I/O over an authenticated async-imap session: tagged commands,
//! literals and response reads, each bounded by a timeout. Any failure poisons the
//! wire because the protocol state is unknown afterwards.

use std::time::Duration;

use async_imap::Session;
use async_imap::imap_proto::{Outcome, RequestId, Response, ResponseCode, Status};
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;

use super::transport::BoxedStream;
use crate::error::{ImapError, Result};
use crate::types::{AppendUid, CopyUid};

/// Literal data is written in chunks so that the write timeout measures progress,
/// not the whole upload.
const LITERAL_CHUNK: usize = 64 * 1024;

pub(crate) struct Wire {
    session: Session<BoxedStream>,
    timeout: Duration,
    broken: bool,
    bye: Option<String>,
}

impl std::fmt::Debug for Wire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wire")
            .field("timeout", &self.timeout)
            .field("broken", &self.broken)
            .finish_non_exhaustive()
    }
}

impl Wire {
    pub(crate) fn new(session: Session<BoxedStream>, timeout: Duration) -> Self {
        Self {
            session,
            timeout,
            broken: false,
            bye: None,
        }
    }

    pub(crate) fn into_session(self) -> Session<BoxedStream> {
        self.session
    }

    pub(crate) fn is_broken(&self) -> bool {
        self.broken
    }

    pub(crate) fn mark_broken(&mut self) {
        self.broken = true;
    }

    pub(crate) fn ensure_usable(&self) -> Result<()> {
        if self.broken {
            Err(ImapError::Broken)
        } else {
            Ok(())
        }
    }

    fn poison(&mut self, err: ImapError) -> ImapError {
        self.broken = true;
        err
    }

    /// Send a tagged command line.
    pub(crate) async fn send(&mut self, command: &str, op: &'static str) -> Result<RequestId> {
        self.ensure_usable()?;
        tracing::trace!(command = op, "imap send");
        match timeout(self.timeout, self.session.run_command(command)).await {
            Ok(Ok(id)) => Ok(id),
            Ok(Err(e)) => Err(self.poison(e.into())),
            Err(_) => Err(self.poison(ImapError::Timeout {
                operation: op,
                after: self.timeout,
            })),
        }
    }

    /// Send an untagged line (`DONE`, SASL continuation data).
    pub(crate) async fn send_untagged(&mut self, line: &str, op: &'static str) -> Result<()> {
        self.ensure_usable()?;
        match timeout(self.timeout, self.session.run_command_untagged(line)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(self.poison(e.into())),
            Err(_) => Err(self.poison(ImapError::Timeout {
                operation: op,
                after: self.timeout,
            })),
        }
    }

    /// Write literal bytes followed by CRLF (the end of the command line).
    pub(crate) async fn write_literal(&mut self, data: &[u8], op: &'static str) -> Result<()> {
        self.ensure_usable()?;
        let limit = self.timeout;
        let result: std::result::Result<std::io::Result<()>, _> = async {
            for chunk in data.chunks(LITERAL_CHUNK) {
                match timeout(limit, self.session.get_mut().write_all(chunk)).await {
                    Ok(Ok(())) => {}
                    other => return other,
                }
            }
            timeout(limit, async {
                let stream = self.session.get_mut();
                stream.write_all(b"\r\n").await?;
                stream.flush().await
            })
            .await
        }
        .await;
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(self.poison(ImapError::Io(e))),
            Err(_) => Err(self.poison(ImapError::Timeout {
                operation: op,
                after: limit,
            })),
        }
    }

    /// Read the next response and hand it to `f`. `limit` overrides the command
    /// timeout; `None` waits forever (IDLE, guarded by the caller).
    pub(crate) async fn read_with<R>(
        &mut self,
        op: &'static str,
        limit: Option<Duration>,
        f: impl FnOnce(&Response<'_>) -> R,
    ) -> Result<R> {
        self.ensure_usable()?;
        let read = self.session.read_response();
        let next = match limit {
            Some(limit) => match timeout(limit, read).await {
                Ok(next) => next,
                Err(_) => {
                    return Err(self.poison(ImapError::Timeout {
                        operation: op,
                        after: limit,
                    }));
                }
            },
            None => read.await,
        };
        match next {
            Ok(Some(data)) => {
                let response = data.parsed();
                if let Response::Data {
                    status: Status::Bye,
                    outcome,
                } = response
                {
                    self.bye = Some(split_outcome(outcome).1);
                }
                Ok(f(response))
            }
            Ok(None) => {
                let bye = self.bye.take();
                Err(self.poison(ImapError::ConnectionLost(bye)))
            }
            Err(e) => Err(self.poison(ImapError::from_stream_io(e))),
        }
    }

    pub(crate) fn command_timeout(&self) -> Duration {
        self.timeout
    }
}

/// A response code, owned and reduced to what the client uses. Codes imap-proto
/// doesn't know (`NOMODSEQ`, `CLOSED`, `NONEXISTENT`, …) are recovered from the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Code {
    Alert,
    AppendUid(AppendUid),
    CopyUid(CopyUid),
    HighestModSeq(u64),
    NoModSeq,
    PermanentFlags(Vec<String>),
    ReadOnly,
    ReadWrite,
    UidNext(u32),
    UidValidity(u32),
    Unseen(u32),
    Closed,
    Other(String),
}

impl Code {
    pub(crate) fn name(&self) -> String {
        match self {
            Self::Alert => "ALERT".into(),
            Self::AppendUid(_) => "APPENDUID".into(),
            Self::CopyUid(_) => "COPYUID".into(),
            Self::HighestModSeq(_) => "HIGHESTMODSEQ".into(),
            Self::NoModSeq => "NOMODSEQ".into(),
            Self::PermanentFlags(_) => "PERMANENTFLAGS".into(),
            Self::ReadOnly => "READ-ONLY".into(),
            Self::ReadWrite => "READ-WRITE".into(),
            Self::UidNext(_) => "UIDNEXT".into(),
            Self::UidValidity(_) => "UIDVALIDITY".into(),
            Self::Unseen(_) => "UNSEEN".into(),
            Self::Closed => "CLOSED".into(),
            Self::Other(name) => name.clone(),
        }
    }
}

fn expand_uid_set(members: &[async_imap::imap_proto::rfc4315::UidSetMember]) -> Vec<u32> {
    use async_imap::imap_proto::rfc4315::UidSetMember;
    let mut out = Vec::new();
    for m in members {
        match m {
            UidSetMember::Uid(u) => out.push(*u),
            UidSetMember::UidRange(r) => {
                let (a, b) = (*r.start(), *r.end());
                out.extend(a.min(b)..=a.max(b));
            }
        }
    }
    out
}

fn known_code(code: &ResponseCode<'_>) -> Code {
    match code {
        ResponseCode::Alert => Code::Alert,
        ResponseCode::AppendUid(validity, uids) => {
            let uids = expand_uid_set(uids);
            match uids.as_slice() {
                [uid] => Code::AppendUid(AppendUid {
                    uidvalidity: *validity,
                    uid: *uid,
                }),
                // MULTIAPPEND results are not used by this client.
                _ => Code::Other("APPENDUID".into()),
            }
        }
        ResponseCode::CopyUid(validity, source, destination) => {
            let mut source = expand_uid_set(source);
            let mut destination = expand_uid_set(destination);
            // RFC 4315: the sets correspond in ascending order.
            source.sort_unstable();
            destination.sort_unstable();
            Code::CopyUid(CopyUid {
                uidvalidity: *validity,
                source,
                destination,
            })
        }
        ResponseCode::HighestModSeq(m) => Code::HighestModSeq(*m),
        ResponseCode::PermanentFlags(flags) => {
            Code::PermanentFlags(flags.iter().map(|f| f.to_string()).collect())
        }
        ResponseCode::ReadOnly => Code::ReadOnly,
        ResponseCode::ReadWrite => Code::ReadWrite,
        ResponseCode::UidNext(n) => Code::UidNext(*n),
        ResponseCode::UidValidity(n) => Code::UidValidity(*n),
        ResponseCode::Unseen(n) => Code::Unseen(*n),
        ResponseCode::TryCreate => Code::Other("TRYCREATE".into()),
        ResponseCode::Parse => Code::Other("PARSE".into()),
        ResponseCode::UidNotSticky => Code::Other("UIDNOTSTICKY".into()),
        ResponseCode::Capabilities(_) => Code::Other("CAPABILITY".into()),
        ResponseCode::BadCharset(_) => Code::Other("BADCHARSET".into()),
        _ => Code::Other("UNKNOWN".into()),
    }
}

/// Split a response's text into its code and the remaining human-readable text.
pub(crate) fn split_outcome(outcome: &Outcome<'_>) -> (Option<Code>, String) {
    let text = outcome.information.as_deref().unwrap_or("").trim();
    if let Some(code) = &outcome.code {
        return (Some(known_code(code)), text.to_owned());
    }
    if let Some(rest) = text.strip_prefix('[')
        && let Some((inner, after)) = rest.split_once(']')
    {
        let name = inner
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        let code = match name.as_str() {
            "NOMODSEQ" => Code::NoModSeq,
            "CLOSED" => Code::Closed,
            _ => Code::Other(name),
        };
        return (Some(code), after.trim().to_owned());
    }
    (None, text.to_owned())
}

/// Turn the tagged completion of a command into a result.
pub(crate) fn tagged_result(
    command: &'static str,
    status: &Status,
    outcome: &Outcome<'_>,
) -> Result<Option<Code>> {
    let (code, text) = split_outcome(outcome);
    match status {
        Status::Ok => Ok(code),
        Status::No => Err(ImapError::No {
            command,
            code: code.map(|c| c.name()),
            text,
        }),
        Status::Bad => Err(ImapError::Bad { command, text }),
        Status::Bye | Status::PreAuth => Err(ImapError::Protocol(format!(
            "unexpected tagged {status:?} for {command}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    fn outcome(code: Option<ResponseCode<'static>>, text: &'static str) -> Outcome<'static> {
        Outcome {
            code,
            information: Some(Cow::Borrowed(text)),
        }
    }

    #[test]
    fn recovers_unknown_codes_from_text() {
        let (code, text) = split_outcome(&outcome(None, "[NOMODSEQ] Sorry, no modseqs"));
        assert_eq!(code, Some(Code::NoModSeq));
        assert_eq!(text, "Sorry, no modseqs");
        let (code, _) = split_outcome(&outcome(None, "[CLOSED] Previous mailbox closed"));
        assert_eq!(code, Some(Code::Closed));
        let (code, _) = split_outcome(&outcome(None, "[NONEXISTENT] No such mailbox"));
        assert_eq!(code, Some(Code::Other("NONEXISTENT".into())));
        let (code, text) = split_outcome(&outcome(None, "Completed"));
        assert_eq!(code, None);
        assert_eq!(text, "Completed");
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)]
    fn copyuid_ranges_are_expanded_in_order() {
        use async_imap::imap_proto::rfc4315::UidSetMember;
        let code = ResponseCode::CopyUid(
            9,
            vec![UidSetMember::UidRange(3..=5), UidSetMember::Uid(1)],
            // RFC 4315 allows ranges in either order.
            vec![UidSetMember::UidRange(std::ops::RangeInclusive::new(
                103, 100,
            ))],
        );
        let (code, _) = split_outcome(&outcome(Some(code), "Done"));
        let Some(Code::CopyUid(c)) = code else {
            panic!("expected COPYUID")
        };
        assert_eq!(c.source, [1, 3, 4, 5]);
        assert_eq!(c.destination, [100, 101, 102, 103]);
        assert_eq!(c.destination_of(4), Some(102));
    }

    #[test]
    fn tagged_no_carries_code() {
        let err = tagged_result(
            "SELECT",
            &Status::No,
            &outcome(None, "[NONEXISTENT] Unknown mailbox"),
        )
        .unwrap_err();
        match err {
            ImapError::No {
                command,
                code,
                text,
            } => {
                assert_eq!(command, "SELECT");
                assert_eq!(code.as_deref(), Some("NONEXISTENT"));
                assert_eq!(text, "Unknown mailbox");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
