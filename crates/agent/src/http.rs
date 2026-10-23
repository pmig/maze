//! Downloads behind a trait, so tests never touch the network.

use std::path::Path;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt as _;

/// Progress of a download: bytes so far and the total, when the server says.
pub type ProgressFn<'a> = &'a (dyn Fn(u64, Option<u64>) + Send + Sync);

/// HTTPS downloads.
#[async_trait]
pub trait Http: Send + Sync {
    /// The body of `url` (small files: the registry index).
    async fn get(&self, url: &str) -> Result<Vec<u8>, HttpError>;

    /// Streams `url` into `dest`, reporting progress; returns the size.
    async fn download(
        &self,
        url: &str,
        dest: &Path,
        progress: ProgressFn<'_>,
    ) -> Result<u64, HttpError>;
}

/// A download failed.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// Only `https://` URLs are fetched.
    #[error("refusing to download {0}: only https:// URLs are allowed")]
    NotHttps(String),
    /// The request failed.
    #[error("downloading {url} failed: {message}")]
    Request {
        /// What was requested.
        url: String,
        /// Why it failed.
        message: String,
    },
    /// Writing the file failed.
    #[error("saving a download: {0}")]
    Io(#[from] std::io::Error),
}

fn check_https(url: &str) -> Result<(), HttpError> {
    if url.starts_with("https://") {
        Ok(())
    } else {
        Err(HttpError::NotHttps(url.to_owned()))
    }
}

/// [`Http`] over `reqwest` with rustls and the system proxy.
#[derive(Debug, Clone)]
pub struct ReqwestHttp {
    client: reqwest::Client,
}

impl ReqwestHttp {
    /// A client with Maze's user agent.
    pub fn new() -> Result<Self, HttpError> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("mazemail/", env!("CARGO_PKG_VERSION")))
            .https_only(true)
            .build()
            .map_err(|error| HttpError::Request {
                url: String::new(),
                message: error.to_string(),
            })?;
        Ok(Self { client })
    }

    async fn send(&self, url: &str) -> Result<reqwest::Response, HttpError> {
        check_https(url)?;
        let request_error = |error: reqwest::Error| HttpError::Request {
            url: url.to_owned(),
            message: error.to_string(),
        };
        self.client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(request_error)
    }
}

#[async_trait]
impl Http for ReqwestHttp {
    async fn get(&self, url: &str) -> Result<Vec<u8>, HttpError> {
        let response = self.send(url).await?;
        let bytes = response.bytes().await.map_err(|error| HttpError::Request {
            url: url.to_owned(),
            message: error.to_string(),
        })?;
        Ok(bytes.to_vec())
    }

    async fn download(
        &self,
        url: &str,
        dest: &Path,
        progress: ProgressFn<'_>,
    ) -> Result<u64, HttpError> {
        let mut response = self.send(url).await?;
        let total = response.content_length();
        let mut file = tokio::fs::File::create(dest).await?;
        let mut done = 0u64;
        progress(0, total);
        while let Some(chunk) = response.chunk().await.map_err(|error| HttpError::Request {
            url: url.to_owned(),
            message: error.to_string(),
        })? {
            file.write_all(&chunk).await?;
            done += chunk.len() as u64;
            progress(done, total);
        }
        file.flush().await?;
        Ok(done)
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod tests {
    //! An in-memory [`Http`] for tests.

    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Serves fixed bodies by URL; everything else fails like a network error.
    #[derive(Debug, Default)]
    pub struct MemoryHttp {
        bodies: Mutex<HashMap<String, Vec<u8>>>,
        requests: AtomicUsize,
    }

    impl MemoryHttp {
        /// Serves `body` at `url`.
        pub fn insert(&self, url: &str, body: &[u8]) {
            self.bodies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(url.to_owned(), body.to_vec());
        }

        /// Requests made so far.
        pub fn requests(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }

        fn body(&self, url: &str) -> Result<Vec<u8>, HttpError> {
            check_https(url)?;
            self.requests.fetch_add(1, Ordering::SeqCst);
            self.bodies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(url)
                .cloned()
                .ok_or_else(|| HttpError::Request {
                    url: url.to_owned(),
                    message: "offline (test)".to_owned(),
                })
        }
    }

    #[async_trait]
    impl Http for MemoryHttp {
        async fn get(&self, url: &str) -> Result<Vec<u8>, HttpError> {
            self.body(url)
        }

        async fn download(
            &self,
            url: &str,
            dest: &Path,
            progress: ProgressFn<'_>,
        ) -> Result<u64, HttpError> {
            let body = self.body(url)?;
            progress(0, Some(body.len() as u64));
            tokio::fs::write(dest, &body).await?;
            progress(body.len() as u64, Some(body.len() as u64));
            Ok(body.len() as u64)
        }
    }

    #[test]
    fn only_https_is_fetched() {
        assert!(matches!(
            check_https("http://example.com/x"),
            Err(HttpError::NotHttps(_))
        ));
        assert!(check_https("https://example.com/x").is_ok());
    }
}
