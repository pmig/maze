//! The Tokio runtime the core runs on (PLAN §4.1).

use std::future::Future;
use std::sync::{Mutex, PoisonError};

use tokio::runtime::{Handle, Runtime};

use crate::config::RuntimeConfig;
use crate::{Error, Result};

/// The runtime handle, and the runtime itself if the core owns it.
#[derive(Debug)]
pub(crate) struct RuntimeHolder {
    handle: Handle,
    owned: Mutex<Option<Runtime>>,
}

impl RuntimeHolder {
    pub(crate) fn new(config: &RuntimeConfig) -> Result<Self> {
        match config {
            RuntimeConfig::Handle(handle) => Ok(Self {
                handle: handle.clone(),
                owned: Mutex::new(None),
            }),
            RuntimeConfig::Owned { worker_threads } => {
                let mut builder = tokio::runtime::Builder::new_multi_thread();
                builder.enable_all().thread_name("mazemail-core");
                if let Some(threads) = worker_threads {
                    builder.worker_threads((*threads).max(1));
                }
                let runtime = builder
                    .build()
                    .map_err(Error::io("starting the core runtime"))?;
                Ok(Self {
                    handle: runtime.handle().clone(),
                    owned: Mutex::new(Some(runtime)),
                })
            }
        }
    }

    pub(crate) fn handle(&self) -> &Handle {
        &self.handle
    }

    /// Runs `future` on the core runtime and waits for it from any executor (GPUI's
    /// included): Tokio-dependent work such as DNS always has its runtime.
    pub(crate) async fn run<F, T>(&self, future: F) -> Result<T>
    where
        F: Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        self.handle.spawn(future).await?
    }

    /// Runs blocking work on the core runtime's blocking pool.
    pub(crate) async fn blocking<F, T>(&self, work: F) -> Result<T>
    where
        F: FnOnce() -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        self.handle.spawn_blocking(work).await?
    }

    /// Stops an owned runtime without blocking (safe inside async code).
    pub(crate) fn shutdown(&self) {
        let runtime = self
            .owned
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(runtime) = runtime {
            runtime.shutdown_background();
        }
    }
}

impl Drop for RuntimeHolder {
    fn drop(&mut self) {
        self.shutdown();
    }
}
