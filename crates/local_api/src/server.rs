//! The Unix-socket server (PLAN §4.11).
//!
//! - Binds `<data_dir>/mazemail.sock`, or the short 0700 fallback directory when that
//!   path is too long for `sun_path` (and records the path in `<data_dir>/socket-path`).
//! - The socket file gets mode 0600. A leftover socket is removed only if nothing
//!   answers on it and it belongs to us; a live server makes `bind` fail with
//!   [`ServerError::AlreadyRunning`].
//! - Each connection is checked against our uid before anything is read, then served
//!   by its own task. Requests on one connection run concurrently (bounded), and their
//!   responses are written by a single writer task.
//! - [`LocalApiServer::serve`] stops accepting when the shutdown future resolves,
//!   lets in-flight requests finish for [`ServerConfig::drain_timeout`], then aborts
//!   the rest and removes the socket file.

use std::future::Future;
use std::io;
use std::os::unix::fs::{
    DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};
use settings::{Paths, SocketPath};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::codec::{Frame, FrameCodec};
use crate::dispatch::{DispatchConfig, Dispatcher, Services};
use crate::peer::{PeerInfo, check_peer_uid, current_uid};
use crate::protocol::{
    ApiError, ErrorCode, MAX_REQUEST_FRAME_LEN, MAX_RESPONSE_FRAME_LEN, Response,
};

/// Server limits and identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Request handling.
    pub dispatch: DispatchConfig,
    /// Largest accepted request frame.
    pub max_frame_len: usize,
    /// Simultaneous connections; more are closed right away.
    pub max_connections: usize,
    /// Requests processed at once per connection; further frames wait.
    pub max_in_flight_per_connection: usize,
    /// How long shutdown waits for in-flight requests.
    pub drain_timeout: Duration,
    /// The only uid allowed to connect (our own).
    pub expected_uid: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            dispatch: DispatchConfig::default(),
            max_frame_len: MAX_REQUEST_FRAME_LEN,
            max_connections: 64,
            max_in_flight_per_connection: 16,
            drain_timeout: Duration::from_secs(5),
            expected_uid: current_uid(),
        }
    }
}

/// Errors starting the server.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Another Maze answers on the socket.
    #[error("Maze is already running (socket {0})")]
    AlreadyRunning(PathBuf),
    /// Something that isn't our socket is in the way.
    #[error("refusing to replace {path}: {reason}")]
    Occupied {
        /// The path.
        path: PathBuf,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The socket directory is not private to us.
    #[error("socket directory {path} is not safe: {reason}")]
    InsecureDir {
        /// The directory.
        path: PathBuf,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// An I/O operation failed.
    #[error("{action} {path}: {source}")]
    Io {
        /// What was attempted.
        action: &'static str,
        /// On which path.
        path: PathBuf,
        /// The error.
        #[source]
        source: io::Error,
    },
}

fn io_error<'a>(
    action: &'static str,
    path: &'a Path,
) -> impl FnOnce(io::Error) -> ServerError + 'a {
    move |source| ServerError::Io {
        action,
        path: path.to_owned(),
        source,
    }
}

/// A bound local API server.
#[derive(Debug)]
pub struct LocalApiServer {
    listener: UnixListener,
    socket: BoundSocket,
    dispatcher: Arc<Dispatcher>,
    config: ServerConfig,
}

impl LocalApiServer {
    /// Binds the socket for `paths` (with the fallback and `socket-path` record).
    pub async fn bind(
        paths: &Paths,
        services: Services,
        config: ServerConfig,
    ) -> Result<Self, ServerError> {
        Self::bind_socket(
            &paths.socket_path(),
            Some(&paths.socket_path_file()),
            services,
            config,
        )
        .await
    }

    /// Binds `socket`. For a fallback socket the path is written to `record_file` (if
    /// given); for a socket in the data directory a stale `record_file` is removed.
    pub async fn bind_socket(
        socket: &SocketPath,
        record_file: Option<&Path>,
        services: Services,
        config: ServerConfig,
    ) -> Result<Self, ServerError> {
        let path = socket.path().to_owned();
        match socket {
            SocketPath::Fallback { dir, .. } => ensure_private_dir(dir, config.expected_uid)?,
            SocketPath::InDataDir(_) => {
                if let Some(parent) = path.parent() {
                    std::fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(parent)
                        .map_err(io_error("create", parent))?;
                }
            }
        }
        clear_stale_socket(&path, config.expected_uid).await?;
        let listener = UnixListener::bind(&path).map_err(io_error("bind", &path))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(io_error("set permissions of", &path))?;
        let metadata = std::fs::symlink_metadata(&path).map_err(io_error("stat", &path))?;
        let mut bound = BoundSocket {
            path,
            identity: (metadata.dev(), metadata.ino()),
            record_file: None,
        };
        if let Some(record) = record_file {
            match socket {
                SocketPath::Fallback { .. } => {
                    write_record(record, &bound.path)?;
                    bound.record_file = Some(record.to_owned());
                }
                SocketPath::InDataDir(_) => match std::fs::remove_file(record) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(io_error("remove", record)(error)),
                },
            }
        }
        tracing::info!(socket = %bound.path.display(), "local api listening");
        Ok(Self {
            listener,
            socket: bound,
            dispatcher: Arc::new(Dispatcher::new(services, config.dispatch.clone())),
            config,
        })
    }

    /// The bound socket path.
    pub fn path(&self) -> &Path {
        &self.socket.path
    }

    /// Serves connections until `shutdown` resolves, then drains and removes the
    /// socket.
    pub async fn serve(self, shutdown: impl Future<Output = ()> + Send) {
        let Self {
            listener,
            socket,
            dispatcher,
            config,
        } = self;
        let config = Arc::new(config);
        let (stop_tx, stop_rx) = watch::channel(false);
        let slots = Arc::new(Semaphore::new(config.max_connections));
        let mut connections = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let Ok(slot) = slots.clone().try_acquire_owned() else {
                            tracing::warn!("too many local api connections, closing one");
                            continue;
                        };
                        let dispatcher = dispatcher.clone();
                        let config = config.clone();
                        let stop = stop_rx.clone();
                        connections.spawn(async move {
                            serve_connection(stream, dispatcher, config, stop).await;
                            drop(slot);
                        });
                    }
                    Err(error) => {
                        // Usually EMFILE; back off instead of spinning.
                        tracing::warn!(%error, "accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                },
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
        drop(listener);
        let _ = stop_tx.send(true);
        let drained = tokio::time::timeout(config.drain_timeout, async {
            while connections.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            tracing::info!("aborting local api requests still running at shutdown");
            connections.shutdown().await;
        }
        drop(socket);
        tracing::info!("local api stopped");
    }
}

/// Removes the socket file (and the `socket-path` record) on drop, but only if they
/// still belong to this server.
#[derive(Debug)]
struct BoundSocket {
    path: PathBuf,
    identity: (u64, u64),
    record_file: Option<PathBuf>,
}

impl Drop for BoundSocket {
    fn drop(&mut self) {
        let ours = std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.identity);
        if !ours {
            return;
        }
        let _ = std::fs::remove_file(&self.path);
        if let Some(record) = &self.record_file
            && std::fs::read_to_string(record)
                .is_ok_and(|content| Path::new(content.trim()) == self.path)
        {
            let _ = std::fs::remove_file(record);
        }
    }
}

/// Creates the fallback directory with mode 0700, or checks an existing one: a real
/// directory (not a symlink) owned by us. Group and other bits are removed.
fn ensure_private_dir(dir: &Path, uid: u32) -> Result<(), ServerError> {
    match std::fs::symlink_metadata(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => std::fs::DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .map_err(io_error("create", dir)),
        Err(error) => Err(io_error("stat", dir)(error)),
        Ok(metadata) => {
            if !metadata.file_type().is_dir() {
                return Err(ServerError::InsecureDir {
                    path: dir.to_owned(),
                    reason: "not a directory",
                });
            }
            if metadata.uid() != uid {
                return Err(ServerError::InsecureDir {
                    path: dir.to_owned(),
                    reason: "owned by another user",
                });
            }
            if metadata.mode() & 0o077 != 0 {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                    .map_err(io_error("set permissions of", dir))?;
            }
            Ok(())
        }
    }
}

/// Removes a leftover socket at `path` if no server answers on it.
async fn clear_stale_socket(path: &Path, uid: u32) -> Result<(), ServerError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("stat", path)(error)),
    };
    if !metadata.file_type().is_socket() {
        return Err(ServerError::Occupied {
            path: path.to_owned(),
            reason: "not a socket",
        });
    }
    if metadata.uid() != uid {
        return Err(ServerError::Occupied {
            path: path.to_owned(),
            reason: "owned by another user",
        });
    }
    match tokio::time::timeout(Duration::from_secs(1), UnixStream::connect(path)).await {
        Ok(Ok(_)) | Err(_) => Err(ServerError::AlreadyRunning(path.to_owned())),
        Ok(Err(error))
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            tracing::info!(socket = %path.display(), "removing stale socket");
            match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(io_error("remove", path)(error)),
            }
        }
        Ok(Err(error)) => Err(io_error("probe", path)(error)),
    }
}

/// Writes the fallback socket path atomically with mode 0600.
fn write_record(record: &Path, socket: &Path) -> Result<(), ServerError> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    if let Some(parent) = record.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(io_error("create", parent))?;
    }
    let tmp = record.with_extension(format!("tmp-{}", std::process::id()));
    let write = || -> io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(socket.as_os_str().as_encoded_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&tmp, record)
    };
    write().map_err(|source| {
        let _ = std::fs::remove_file(&tmp);
        io_error("write", record)(source)
    })
}

async fn serve_connection(
    stream: UnixStream,
    dispatcher: Arc<Dispatcher>,
    config: Arc<ServerConfig>,
    mut stop: watch::Receiver<bool>,
) {
    let peer = match PeerInfo::from_stream(&stream) {
        Ok(peer) => peer,
        Err(error) => {
            tracing::warn!(%error, "cannot read peer credentials, closing connection");
            return;
        }
    };
    if let Err(rejected) = check_peer_uid(peer.uid, config.expected_uid) {
        tracing::warn!(
            peer_uid = rejected.peer,
            pid = peer.process.pid,
            "rejecting connection from another user"
        );
        dispatcher.reject_peer(&peer);
        return;
    }
    let peer = Arc::new(peer);
    serve_frames(stream, peer, dispatcher, &config, &mut stop).await;
}

/// Reads frames and dispatches them until EOF, an I/O error or shutdown, then waits for
/// the requests in flight.
async fn serve_frames(
    stream: UnixStream,
    peer: Arc<PeerInfo>,
    dispatcher: Arc<Dispatcher>,
    config: &ServerConfig,
    stop: &mut watch::Receiver<bool>,
) {
    let (read, write) = stream.into_split();
    let max_frame_len = config.max_frame_len;
    let mut frames = FramedRead::new(read, FrameCodec::new(max_frame_len));
    let mut sink = FramedWrite::new(write, FrameCodec::new(MAX_RESPONSE_FRAME_LEN));
    let (tx, mut rx) = mpsc::channel::<Response>(64);
    let writer = tokio::spawn(async move {
        while let Some(response) = rx.recv().await {
            if sink.send(encode_response(&response)).await.is_err() {
                break;
            }
        }
    });

    let in_flight = Arc::new(Semaphore::new(config.max_in_flight_per_connection));
    let mut requests = JoinSet::new();
    loop {
        tokio::select! {
            () = stopped(stop) => break,
            frame = frames.next() => match frame {
                None => break,
                Some(Err(error)) => {
                    tracing::debug!(%error, "local api connection read failed");
                    break;
                }
                Some(Ok(Frame::Oversized { len })) => {
                    let response = dispatcher.oversized(&peer, len, max_frame_len);
                    if tx.send(response).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Frame::Data(bytes))) => {
                    let Ok(permit) = in_flight.clone().acquire_owned().await else { break };
                    let dispatcher = dispatcher.clone();
                    let peer = peer.clone();
                    let tx = tx.clone();
                    requests.spawn(async move {
                        let response = dispatcher.dispatch(&peer, &bytes, &tx).await;
                        let _ = tx.send(response).await;
                        drop(permit);
                    });
                }
            },
            Some(_) = requests.join_next(), if !requests.is_empty() => {}
        }
    }
    while requests.join_next().await.is_some() {}
    drop(tx);
    let _ = writer.await;
}

/// Resolves once shutdown was requested (or the server is gone).
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await.map(|_| ());
}

/// Serializes a response; one that is too large becomes an `internal` error.
fn encode_response(response: &Response) -> Bytes {
    let encoded = serde_json::to_vec(response).map_err(|error| error.to_string());
    match encoded {
        Ok(bytes) if bytes.len() <= MAX_RESPONSE_FRAME_LEN => Bytes::from(bytes),
        Ok(bytes) => fallback(
            response.id,
            format!("response of {} bytes is too large", bytes.len()),
        ),
        Err(error) => fallback(response.id, format!("cannot encode response: {error}")),
    }
}

fn fallback(id: u64, message: String) -> Bytes {
    let response = Response::error(id, ApiError::new(ErrorCode::Internal, message));
    Bytes::from(serde_json::to_vec(&response).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_dir_checks() {
        let tmp = tempfile::tempdir().unwrap();
        let uid = current_uid();
        let dir = tmp.path().join("sock");
        ensure_private_dir(&dir, uid).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700);
        // Loose permissions are tightened.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&dir, uid).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        // Someone else's directory, a file, or a symlink are refused.
        assert!(matches!(
            ensure_private_dir(&dir, uid.wrapping_add(1)),
            Err(ServerError::InsecureDir { .. })
        ));
        let file = tmp.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(ensure_private_dir(&file, uid).is_err());
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(ensure_private_dir(&link, uid).is_err());
    }

    #[tokio::test]
    async fn stale_socket_handling() {
        let tmp = tempfile::tempdir().unwrap();
        let uid = current_uid();
        let path = tmp.path().join("s.sock");
        // Nothing there.
        clear_stale_socket(&path, uid).await.unwrap();
        // A regular file is never removed.
        std::fs::write(&path, b"x").unwrap();
        assert!(matches!(
            clear_stale_socket(&path, uid).await,
            Err(ServerError::Occupied { .. })
        ));
        std::fs::remove_file(&path).unwrap();
        // A live listener means another server.
        let listener = UnixListener::bind(&path).unwrap();
        assert!(matches!(
            clear_stale_socket(&path, uid).await,
            Err(ServerError::AlreadyRunning(_))
        ));
        // A dead socket is removed.
        drop(listener);
        clear_stale_socket(&path, uid).await.unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn oversized_responses_become_errors() {
        let big = "x".repeat(MAX_RESPONSE_FRAME_LEN + 1);
        let bytes = encode_response(&Response::data(3, serde_json::Value::String(big)));
        let response: Response = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response.id, 3);
        assert!(matches!(
            response.body,
            crate::ResponseBody::Error(ApiError {
                code: ErrorCode::Internal,
                ..
            })
        ));
    }
}
