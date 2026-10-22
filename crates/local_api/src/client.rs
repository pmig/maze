//! The client the `mazemail` CLI uses to talk to Maze.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::Value;
use settings::{Paths, SocketPath};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::codec::{Frame, FrameCodec};
use crate::protocol::{
    ApiError, ExitCode, MAX_REQUEST_FRAME_LEN, MAX_RESPONSE_FRAME_LEN, PROTOCOL_VERSION, Progress,
    RESTART_HINT, Request, Response, ResponseBody,
};
use crate::token::SessionToken;

/// Where the Linux sandbox bridge listens (`host:port` on loopback, PLAN §4.18).
pub const API_ADDR_ENV: &str = "MAZEMAIL_API_ADDR";

/// Where to reach Maze.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// The local socket.
    Unix(PathBuf),
    /// The sandbox bridge's loopback TCP port.
    Tcp(String),
}

impl Endpoint {
    /// `MAZEMAIL_API_ADDR` if set, otherwise the socket for `paths`.
    pub fn discover(paths: &Paths) -> Self {
        let api_addr = std::env::var(API_ADDR_ENV)
            .ok()
            .filter(|addr| !addr.trim().is_empty());
        Self::discover_with(paths, api_addr)
    }

    /// Like [`Endpoint::discover`] with an explicit bridge address.
    ///
    /// For a fallback socket the path recorded by the server in
    /// `<data_dir>/socket-path` wins over the computed one, because the caller's
    /// `$TMPDIR` may differ from the server's (agent sandboxes get their own).
    pub fn discover_with(paths: &Paths, api_addr: Option<String>) -> Self {
        if let Some(addr) = api_addr {
            return Self::Tcp(addr.trim().to_owned());
        }
        match paths.socket_path() {
            SocketPath::InDataDir(path) => Self::Unix(path),
            SocketPath::Fallback { path, .. } => {
                Self::Unix(read_record(&paths.socket_path_file()).unwrap_or(path))
            }
        }
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "{}", path.display()),
            Self::Tcp(addr) => write!(f, "tcp://{addr}"),
        }
    }
}

fn read_record(file: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(file).ok()?;
    let path = PathBuf::from(content.trim());
    path.is_absolute().then_some(path)
}

/// Client errors, each with its CLI exit code.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Nothing listens at the endpoint.
    #[error("Maze is not running (cannot connect to {endpoint}: {source})")]
    NotRunning {
        /// Where we tried.
        endpoint: String,
        /// Why it failed.
        #[source]
        source: io::Error,
    },
    /// Maze answered with an error.
    #[error("{}", .0.message)]
    Api(ApiError),
    /// Maze speaks a different protocol version.
    #[error("{RESTART_HINT}")]
    RestartRequired,
    /// The request is larger than the frame limit.
    #[error("the request is too large ({len} bytes, at most {MAX_REQUEST_FRAME_LEN})")]
    RequestTooLarge {
        /// Its size.
        len: usize,
    },
    /// Maze closed the connection before answering.
    #[error("Maze closed the connection")]
    Closed,
    /// Maze sent something that isn't a valid response.
    #[error("invalid response from Maze: {0}")]
    Protocol(String),
    /// Reading or writing the connection failed.
    #[error("connection to Maze failed: {0}")]
    Io(#[source] io::Error),
}

impl ClientError {
    /// The process exit code (PLAN §4.11).
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::NotRunning { .. } | Self::RestartRequired | Self::Closed => ExitCode::NotRunning,
            Self::Api(error) => error.exit_code(),
            Self::RequestTooLarge { .. } => ExitCode::Usage,
            Self::Protocol(_) | Self::Io(_) => ExitCode::Failure,
        }
    }
}

type BoxedRead = Pin<Box<dyn AsyncRead + Send>>;
type BoxedWrite = Pin<Box<dyn AsyncWrite + Send>>;
type Reader = FramedRead<BoxedRead, FrameCodec>;
type Writer = FramedWrite<BoxedWrite, FrameCodec>;

/// A connection to Maze. Requests are sent one at a time.
pub struct LocalApiClient {
    reader: Reader,
    writer: Writer,
    token: Option<SessionToken>,
    next_id: u64,
}

impl std::fmt::Debug for LocalApiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalApiClient")
            .field("token", &self.token)
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl LocalApiClient {
    /// Connects to the endpoint for `paths` (see [`Endpoint::discover`]) with the token
    /// from `MAZEMAIL_SESSION`.
    pub async fn connect_default(paths: &Paths) -> Result<Self, ClientError> {
        Ok(Self::connect(&Endpoint::discover(paths))
            .await?
            .with_token(SessionToken::from_env()))
    }

    /// Connects to `endpoint` without a token.
    pub async fn connect(endpoint: &Endpoint) -> Result<Self, ClientError> {
        let not_running = |source| ClientError::NotRunning {
            endpoint: endpoint.to_string(),
            source,
        };
        let (read, write): (BoxedRead, BoxedWrite) = match endpoint {
            Endpoint::Unix(path) => {
                let (read, write) = UnixStream::connect(path)
                    .await
                    .map_err(not_running)?
                    .into_split();
                (Box::pin(read), Box::pin(write))
            }
            Endpoint::Tcp(addr) => {
                let stream = TcpStream::connect(addr.as_str())
                    .await
                    .map_err(not_running)?;
                let _ = stream.set_nodelay(true);
                let (read, write) = stream.into_split();
                (Box::pin(read), Box::pin(write))
            }
        };
        Ok(Self {
            reader: FramedRead::new(read, FrameCodec::new(MAX_RESPONSE_FRAME_LEN)),
            writer: FramedWrite::new(write, FrameCodec::new(MAX_REQUEST_FRAME_LEN)),
            token: None,
            next_id: 1,
        })
    }

    /// Sets the session token sent with every request.
    #[must_use]
    pub fn with_token(mut self, token: Option<SessionToken>) -> Self {
        self.token = token;
        self
    }

    /// Runs `cmd` and returns the final `data`, ignoring progress.
    pub async fn call(&mut self, cmd: &str, args: Value) -> Result<Value, ClientError> {
        self.call_with_progress(cmd, args, |_| {}).await
    }

    /// Runs `cmd`, reporting progress frames (such as "waiting for approval in Maze…")
    /// to `on_progress`, and returns the final `data`.
    pub async fn call_with_progress(
        &mut self,
        cmd: &str,
        args: Value,
        mut on_progress: impl FnMut(&Progress) + Send,
    ) -> Result<Value, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = Request::new(id, cmd, args).with_token(self.token.clone());
        let bytes = serde_json::to_vec(&request)
            .map_err(|error| ClientError::Protocol(error.to_string()))?;
        if bytes.len() > MAX_REQUEST_FRAME_LEN {
            return Err(ClientError::RequestTooLarge { len: bytes.len() });
        }
        self.writer
            .send(Bytes::from(bytes))
            .await
            .map_err(ClientError::Io)?;
        loop {
            let frame = match self.reader.next().await {
                None => return Err(ClientError::Closed),
                Some(Err(error)) => return Err(ClientError::Io(error)),
                Some(Ok(Frame::Oversized { len })) => {
                    return Err(ClientError::Protocol(format!(
                        "response of {len} bytes exceeds the limit"
                    )));
                }
                Some(Ok(Frame::Data(bytes))) => bytes,
            };
            // Check the version before the shape: a different major may have another.
            let value: Value = serde_json::from_slice(&frame)
                .map_err(|error| ClientError::Protocol(error.to_string()))?;
            if value.get("v").and_then(Value::as_u64) != Some(u64::from(PROTOCOL_VERSION)) {
                return Err(ClientError::RestartRequired);
            }
            let response: Response = serde_json::from_value(value)
                .map_err(|error| ClientError::Protocol(error.to_string()))?;
            // Id 0 answers a frame the server couldn't read (too large, not JSON).
            if response.id != id && !(response.id == 0 && response.is_final()) {
                continue;
            }
            match response.body {
                ResponseBody::Progress(progress) => on_progress(&progress),
                ResponseBody::Data(data) => return Ok(data.into_inner()),
                ResponseBody::Error(error) => return Err(ClientError::Api(error)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ErrorCode;

    #[test]
    fn discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_dirs(tmp.path(), tmp.path());
        assert_eq!(
            Endpoint::discover_with(&paths, Some(" 127.0.0.1:4100 ".into())),
            Endpoint::Tcp("127.0.0.1:4100".into())
        );
        assert_eq!(
            Endpoint::discover_with(&paths, None),
            Endpoint::Unix(tmp.path().join("mazemail.sock"))
        );

        // A data dir too long for sun_path uses the recorded fallback path.
        let long = tmp.path().join("d".repeat(120));
        std::fs::create_dir_all(&long).unwrap();
        let paths = Paths::from_dirs(&long, &long);
        let SocketPath::Fallback { path: computed, .. } = paths.socket_path() else {
            panic!("expected fallback");
        };
        assert_eq!(
            Endpoint::discover_with(&paths, None),
            Endpoint::Unix(computed)
        );
        std::fs::write(paths.socket_path_file(), "/tmp/elsewhere/mazemail.sock\n").unwrap();
        assert_eq!(
            Endpoint::discover_with(&paths, None),
            Endpoint::Unix("/tmp/elsewhere/mazemail.sock".into())
        );
        // A relative record is ignored.
        std::fs::write(paths.socket_path_file(), "relative.sock").unwrap();
        assert!(
            matches!(Endpoint::discover_with(&paths, None), Endpoint::Unix(p) if p != Path::new("relative.sock"))
        );
    }

    #[test]
    fn exit_codes() {
        let not_running = ClientError::NotRunning {
            endpoint: "x".into(),
            source: io::ErrorKind::NotFound.into(),
        };
        assert_eq!(not_running.exit_code().code(), 5);
        assert_eq!(ClientError::RestartRequired.exit_code().code(), 5);
        assert_eq!(
            ClientError::RestartRequired.to_string(),
            "Maze was updated, restart it"
        );
        assert_eq!(
            ClientError::Api(ApiError::new(ErrorCode::NotSignedIn, "x"))
                .exit_code()
                .code(),
            6
        );
        assert_eq!(
            ClientError::RequestTooLarge { len: 1 }.exit_code().code(),
            2
        );
    }

    #[tokio::test]
    async fn connecting_to_nothing_is_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let error = LocalApiClient::connect(&Endpoint::Unix(tmp.path().join("none.sock")))
            .await
            .unwrap_err();
        assert_eq!(error.exit_code(), ExitCode::NotRunning);
    }
}
