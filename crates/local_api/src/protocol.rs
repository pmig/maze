//! The wire protocol: length-delimited JSON frames over the socket (PLAN §4.11).
//!
//! # Frames
//!
//! Every frame is a 4-byte big-endian length followed by that many bytes of UTF-8
//! JSON ([`FrameCodec`](crate::FrameCodec), the default `LengthDelimitedCodec` layout).
//! Requests are limited to [`MAX_REQUEST_FRAME_LEN`], responses to
//! [`MAX_RESPONSE_FRAME_LEN`].
//!
//! # Requests
//!
//! ```json
//! { "v": 1, "id": 7, "token": "mazemail_s_…", "cmd": "search", "args": { "query": "from:anna", "limit": 20 } }
//! ```
//!
//! - `v` is the protocol major version ([`PROTOCOL_VERSION`]).
//! - `id` is chosen by the client and echoed in every response frame for the request.
//!   Use ids `>= 1`; `0` is what the server answers when it can't read the id.
//! - `token` is optional: without it only `help`, `status` and `login` work.
//! - `cmd` is a wire name from the [registry](crate::registry) (`search`,
//!   `draft.new`, …), `args` its arguments object (default `{}`).
//!
//! # Responses
//!
//! Each request gets exactly one **final** frame, in one of the two shapes from the plan:
//!
//! ```json
//! { "v": 1, "id": 7, "ok": true, "data": [ { "id": "m_8f3a2c", "thread": "t_19bd" } ] }
//! { "v": 1, "id": 8, "ok": false, "error": { "code": "approval_denied", "message": "denied in Maze" } }
//! ```
//!
//! # Streaming
//!
//! Long-running commands (anything that waits for an approval, `export`, `import`) may
//! send any number of **progress** frames before the final one. Progress frames carry
//! `"more": true` and a `progress` object instead of `ok`:
//!
//! ```json
//! { "v": 1, "id": 8, "more": true, "progress": { "code": "approval_pending", "message": "waiting for approval in Maze…" } }
//! { "v": 1, "id": 9, "more": true, "progress": { "code": "working", "message": "exporting", "done": 120, "total": 4000 } }
//! ```
//!
//! A client keeps reading frames for an id until it gets one without `more`. Several
//! requests may be in flight on one connection; their frames can interleave and are
//! matched by `id`.
//!
//! # Versions
//!
//! Both sides only rely on `v` and `id` being readable in every future version. The
//! server answers a request whose `v` differs from its own with
//! [`ErrorCode::RestartRequired`] ("Maze was updated, restart it"), and the client
//! treats a response with a different `v` the same way. Within a major version changes
//! are additive: new commands, new optional output fields, new error and progress
//! codes (old clients map unknown codes to [`ErrorCode::Unknown`]). Unknown *argument*
//! fields are rejected with `invalid_args`, because silently ignoring an option the
//! caller asked for (say a future `--dry-run`) would be worse than failing.

use std::fmt;

use mazemail_types::Sensitive;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::token::SessionToken;

/// The protocol major version this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The hint shown when client and server speak different protocol versions.
pub const RESTART_HINT: &str = "Maze was updated, restart it";

/// The largest request frame the server accepts (8 MiB: drafts with attachments,
/// proposal batches).
pub const MAX_REQUEST_FRAME_LEN: usize = 8 * 1024 * 1024;

/// The largest response frame the client accepts (64 MiB: raw messages, attachments).
pub const MAX_RESPONSE_FRAME_LEN: usize = 64 * 1024 * 1024;

/// One request frame.
///
/// `Debug` never shows the token or the arguments (they can contain addresses, search
/// queries or message bodies).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Protocol major version, [`PROTOCOL_VERSION`].
    pub v: u32,
    /// Client-chosen request id, echoed in every response frame.
    pub id: u64,
    /// The session token, from `MAZEMAIL_SESSION`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<SessionToken>,
    /// The command's wire name (`search`, `draft.new`, …).
    pub cmd: String,
    /// The command's arguments.
    #[serde(default = "empty_args")]
    pub args: Sensitive<Value>,
}

fn empty_args() -> Sensitive<Value> {
    Sensitive::new(Value::Object(serde_json::Map::new()))
}

impl Request {
    /// A request for `cmd` with `args` at the current protocol version.
    pub fn new(id: u64, cmd: impl Into<String>, args: Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            token: None,
            cmd: cmd.into(),
            args: Sensitive::new(args),
        }
    }

    /// Attaches a session token.
    #[must_use]
    pub fn with_token(mut self, token: Option<SessionToken>) -> Self {
        self.token = token;
        self
    }
}

/// One response frame: final data, final error, or progress.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    /// Protocol major version of the server.
    pub v: u32,
    /// The request id this frame belongs to (`0` if the server couldn't read it).
    pub id: u64,
    /// What the frame carries.
    pub body: ResponseBody,
}

/// The payload of a [`Response`].
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseBody {
    /// Final success: `"ok": true, "data": …`.
    Data(Sensitive<Value>),
    /// Final failure: `"ok": false, "error": { … }`.
    Error(ApiError),
    /// Not final: `"more": true, "progress": { … }`.
    Progress(Progress),
}

impl Response {
    /// A final success frame.
    pub fn data(id: u64, data: Value) -> Self {
        Self::with_body(id, ResponseBody::Data(Sensitive::new(data)))
    }

    /// A final error frame.
    pub fn error(id: u64, error: ApiError) -> Self {
        Self::with_body(id, ResponseBody::Error(error))
    }

    /// A progress frame.
    pub fn progress(id: u64, progress: Progress) -> Self {
        Self::with_body(id, ResponseBody::Progress(progress))
    }

    fn with_body(id: u64, body: ResponseBody) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            body,
        }
    }

    /// Whether this is the last frame for its request.
    pub fn is_final(&self) -> bool {
        !matches!(self.body, ResponseBody::Progress(_))
    }
}

/// The JSON layout of a response frame, shared by serializer and deserializer.
#[derive(Serialize, Deserialize)]
struct WireResponse {
    v: u32,
    id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<ApiError>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    progress: Option<Progress>,
}

impl Serialize for Response {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut wire = WireResponse {
            v: self.v,
            id: self.id,
            ok: None,
            data: None,
            error: None,
            more: false,
            progress: None,
        };
        match &self.body {
            ResponseBody::Data(data) => {
                wire.ok = Some(true);
                wire.data = Some(data.expose().clone());
            }
            ResponseBody::Error(error) => {
                wire.ok = Some(false);
                wire.error = Some(error.clone());
            }
            ResponseBody::Progress(progress) => {
                wire.more = true;
                wire.progress = Some(progress.clone());
            }
        }
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Response {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let wire = WireResponse::deserialize(deserializer)?;
        let body = match (wire.more, wire.ok) {
            (true, _) => ResponseBody::Progress(
                wire.progress
                    .ok_or_else(|| D::Error::missing_field("progress"))?,
            ),
            (false, Some(true)) => {
                ResponseBody::Data(Sensitive::new(wire.data.unwrap_or(Value::Null)))
            }
            (false, Some(false)) => {
                ResponseBody::Error(wire.error.ok_or_else(|| D::Error::missing_field("error"))?)
            }
            (false, None) => return Err(D::Error::missing_field("ok")),
        };
        Ok(Self {
            v: wire.v,
            id: wire.id,
            body,
        })
    }
}

/// The error object of a final error frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message} ({code})")]
pub struct ApiError {
    /// Machine-readable code.
    pub code: ErrorCode,
    /// Human-readable message. Never contains mail content the caller didn't send.
    pub message: String,
}

impl ApiError {
    /// An error with `code` and `message`.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The protocol versions differ.
    pub fn restart_required() -> Self {
        Self::new(ErrorCode::RestartRequired, RESTART_HINT)
    }

    /// The process exit code the CLI uses for this error.
    pub fn exit_code(&self) -> ExitCode {
        self.code.exit_code()
    }
}

/// Machine-readable error codes. New codes may appear within a protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The frame isn't a valid request (not JSON, missing fields).
    BadRequest,
    /// The frame exceeds the size limit.
    FrameTooLarge,
    /// Client and server speak different protocol versions.
    RestartRequired,
    /// `cmd` names no known command.
    UnknownCommand,
    /// The arguments don't match the command's schema.
    InvalidArgs,
    /// No token, or the token is unknown, expired or revoked.
    NotSignedIn,
    /// The token's profile or scope doesn't allow this.
    PermissionDenied,
    /// The user denied the approval.
    ApprovalDenied,
    /// Nobody answered the approval in time.
    ApprovalTimeout,
    /// Too many approvals are already waiting.
    Busy,
    /// A message, thread, draft, folder or token doesn't exist (or is out of scope).
    NotFound,
    /// Maze is shutting down or not ready.
    Unavailable,
    /// Something went wrong inside Maze.
    Internal,
    /// A code this build doesn't know (sent by a newer Maze).
    #[serde(other)]
    Unknown,
}

impl ErrorCode {
    /// The wire string (`not_signed_in`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadRequest => "bad_request",
            Self::FrameTooLarge => "frame_too_large",
            Self::RestartRequired => "restart_required",
            Self::UnknownCommand => "unknown_command",
            Self::InvalidArgs => "invalid_args",
            Self::NotSignedIn => "not_signed_in",
            Self::PermissionDenied => "permission_denied",
            Self::ApprovalDenied => "approval_denied",
            Self::ApprovalTimeout => "approval_timeout",
            Self::Busy => "busy",
            Self::NotFound => "not_found",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
            Self::Unknown => "unknown",
        }
    }

    /// The process exit code the CLI uses for this error (PLAN §4.11).
    pub fn exit_code(self) -> ExitCode {
        match self {
            Self::BadRequest | Self::FrameTooLarge | Self::UnknownCommand | Self::InvalidArgs => {
                ExitCode::Usage
            }
            Self::PermissionDenied => ExitCode::PermissionDenied,
            Self::ApprovalDenied | Self::ApprovalTimeout => ExitCode::ApprovalDenied,
            Self::RestartRequired | Self::Unavailable => ExitCode::NotRunning,
            Self::NotSignedIn => ExitCode::NotSignedIn,
            Self::Busy | Self::NotFound | Self::Internal | Self::Unknown => ExitCode::Failure,
        }
    }

    /// Every known code, for documentation.
    pub const ALL: [Self; 13] = [
        Self::BadRequest,
        Self::FrameTooLarge,
        Self::RestartRequired,
        Self::UnknownCommand,
        Self::InvalidArgs,
        Self::NotSignedIn,
        Self::PermissionDenied,
        Self::ApprovalDenied,
        Self::ApprovalTimeout,
        Self::Busy,
        Self::NotFound,
        Self::Unavailable,
        Self::Internal,
    ];
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `mazemail` CLI's exit codes (PLAN §4.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ExitCode {
    /// 0: success.
    Ok = 0,
    /// 1: any other failure.
    Failure = 1,
    /// 2: usage error (bad arguments, unknown command).
    Usage = 2,
    /// 3: the token's profile or scope doesn't allow the command.
    PermissionDenied = 3,
    /// 4: the approval was denied or timed out.
    ApprovalDenied = 4,
    /// 5: Maze isn't running (or runs an incompatible version).
    NotRunning = 5,
    /// 6: not signed in (no, unknown, expired or revoked token).
    NotSignedIn = 6,
}

impl ExitCode {
    /// The numeric code.
    pub fn code(self) -> u8 {
        self as u8
    }

    /// What the code means, for help output.
    pub fn description(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failure => "other failure",
            Self::Usage => "usage error",
            Self::PermissionDenied => "permission denied",
            Self::ApprovalDenied => "approval denied",
            Self::NotRunning => "Maze is not running",
            Self::NotSignedIn => "not signed in (no or expired token)",
        }
    }

    /// Every exit code, in numeric order.
    pub const ALL: [Self; 7] = [
        Self::Ok,
        Self::Failure,
        Self::Usage,
        Self::PermissionDenied,
        Self::ApprovalDenied,
        Self::NotRunning,
        Self::NotSignedIn,
    ];
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(code: ExitCode) -> Self {
        Self::from(code.code())
    }
}

/// A progress frame's payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// What is happening.
    pub code: ProgressCode,
    /// Human-readable status line.
    pub message: String,
    /// Items done so far, if countable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<u64>,
    /// Total items, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

impl Progress {
    /// "waiting for approval in Maze…".
    pub fn approval_pending() -> Self {
        Self {
            code: ProgressCode::ApprovalPending,
            message: format!("waiting for approval in {}…", mazemail_types::PRODUCT_NAME),
            done: None,
            total: None,
        }
    }

    /// A generic progress update.
    pub fn working(message: impl Into<String>, done: Option<u64>, total: Option<u64>) -> Self {
        Self {
            code: ProgressCode::Working,
            message: message.into(),
            done,
            total,
        }
    }
}

/// Progress codes. New codes may appear within a protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressCode {
    /// The request waits for the user to approve it in Maze.
    ApprovalPending,
    /// A long-running command made progress.
    Working,
    /// A code this build doesn't know.
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    #[test]
    fn request_shape_matches_the_plan() {
        let request = Request::new(7, "search", json!({ "query": "from:anna", "limit": 20 }))
            .with_token(Some(SessionToken::from_string("mazemail_s_abc".to_owned())));
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value,
            json!({ "v": 1, "id": 7, "token": "mazemail_s_abc", "cmd": "search",
                    "args": { "query": "from:anna", "limit": 20 } })
        );
        let back: Request = serde_json::from_value(value).unwrap();
        assert_eq!(back, request);
    }

    #[test]
    fn request_defaults() {
        let request: Request = serde_json::from_str(r#"{"v":1,"id":1,"cmd":"status"}"#).unwrap();
        assert_eq!(request.token, None);
        assert_eq!(request.args.expose(), &json!({}));
    }

    #[test]
    fn request_debug_hides_token_and_args() {
        let request = Request::new(1, "search", json!({ "query": "from:anna@example.com" }))
            .with_token(Some(SessionToken::from_string(
                "mazemail_s_secret".to_owned(),
            )));
        let debug = format!("{request:?}");
        assert!(!debug.contains("anna"), "{debug}");
        assert!(!debug.contains("secret"), "{debug}");
    }

    #[test]
    fn response_shapes_match_the_plan() {
        let data = Response::data(7, json!([{ "id": "m_8f3a2c" }]));
        assert_eq!(
            serde_json::to_value(&data).unwrap(),
            json!({ "v": 1, "id": 7, "ok": true, "data": [{ "id": "m_8f3a2c" }] })
        );
        let error = Response::error(8, ApiError::new(ErrorCode::ApprovalDenied, "denied"));
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            json!({ "v": 1, "id": 8, "ok": false,
                    "error": { "code": "approval_denied", "message": "denied" } })
        );
        let progress = Response::progress(8, Progress::approval_pending());
        assert_eq!(
            serde_json::to_value(&progress).unwrap(),
            json!({ "v": 1, "id": 8, "more": true,
                    "progress": { "code": "approval_pending",
                                  "message": "waiting for approval in Maze…" } })
        );
        for response in [data, error, progress] {
            let text = serde_json::to_string(&response).unwrap();
            assert_eq!(serde_json::from_str::<Response>(&text).unwrap(), response);
        }
    }

    #[test]
    fn malformed_responses_are_rejected() {
        for text in [
            r#"{"v":1,"id":1}"#,
            r#"{"v":1,"id":1,"ok":false}"#,
            r#"{"v":1,"id":1,"more":true}"#,
        ] {
            assert!(serde_json::from_str::<Response>(text).is_err(), "{text}");
        }
        // `data` may be absent on success (commands without output).
        let response: Response = serde_json::from_str(r#"{"v":1,"id":1,"ok":true}"#).unwrap();
        assert_eq!(
            response.body,
            ResponseBody::Data(Sensitive::new(Value::Null))
        );
    }

    #[test]
    fn unknown_codes_from_newer_servers() {
        let error: ApiError =
            serde_json::from_str(r#"{"code":"quota_exceeded","message":"x"}"#).unwrap();
        assert_eq!(error.code, ErrorCode::Unknown);
        assert_eq!(error.exit_code(), ExitCode::Failure);
        let progress: Progress =
            serde_json::from_str(r#"{"code":"indexing","message":"x"}"#).unwrap();
        assert_eq!(progress.code, ProgressCode::Other);
    }

    #[test]
    fn error_codes_round_trip_and_map_to_exit_codes() {
        for code in ErrorCode::ALL {
            let text = serde_json::to_string(&code).unwrap();
            assert_eq!(text, format!("\"{}\"", code.as_str()));
            assert_eq!(serde_json::from_str::<ErrorCode>(&text).unwrap(), code);
        }
        assert_eq!(ErrorCode::NotSignedIn.exit_code().code(), 6);
        assert_eq!(ErrorCode::PermissionDenied.exit_code().code(), 3);
        assert_eq!(ErrorCode::ApprovalDenied.exit_code().code(), 4);
        assert_eq!(ErrorCode::ApprovalTimeout.exit_code().code(), 4);
        assert_eq!(ErrorCode::InvalidArgs.exit_code().code(), 2);
        assert_eq!(ErrorCode::Unavailable.exit_code().code(), 5);
        assert_eq!(ApiError::restart_required().message, RESTART_HINT);
    }
}
