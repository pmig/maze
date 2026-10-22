//! Run Tokio futures from GPUI.
//!
//! Ported to `gpui-pre` from Zed's `gpui_tokio` crate (Copyright 2022 - 2025 Zed
//! Industries, Inc., Apache-2.0). GPUI has its own executors, but Maze's core, sync and
//! networking run on Tokio. [`init`] installs a Tokio runtime as a GPUI global, and
//! [`Tokio::spawn`] runs a future on it and hands back a GPUI [`Task`]. Dropping the
//! task aborts the Tokio task, so work started by a view stops with the view.

use std::future::Future;

use gpui::{App, AppContext, Global, ReadGlobal, Task};

pub use tokio::task::JoinError;

/// Worker threads of the runtime [`init`] creates. GPUI already runs its own thread
/// pool, so the Tokio side stays small until the core brings its own runtime.
const WORKER_THREADS: usize = 2;

/// Creates a multi-threaded Tokio runtime and installs it for [`Tokio::spawn`].
///
/// To share a runtime that already exists (the core's, for example), use
/// [`init_from_handle`] instead.
pub fn init(cx: &mut App) -> std::io::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .thread_name("mazemail-tokio")
        .enable_all()
        .build()?;
    let handle = runtime.handle().clone();
    cx.set_global(GlobalTokio {
        owned_runtime: Some(runtime),
        handle,
    });
    Ok(())
}

/// Installs an existing Tokio runtime for [`Tokio::spawn`]. The caller keeps owning
/// the runtime.
pub fn init_from_handle(cx: &mut App, handle: tokio::runtime::Handle) {
    cx.set_global(GlobalTokio {
        owned_runtime: None,
        handle,
    });
}

struct GlobalTokio {
    owned_runtime: Option<tokio::runtime::Runtime>,
    handle: tokio::runtime::Handle,
}

impl Global for GlobalTokio {}

impl Drop for GlobalTokio {
    fn drop(&mut self) {
        if let Some(runtime) = self.owned_runtime.take() {
            // Don't block the GPUI thread on shutdown; in-flight tasks are abandoned.
            runtime.shutdown_background();
        }
    }
}

/// Aborts a Tokio task when dropped, unless disarmed.
struct AbortOnDrop(Option<tokio::task::AbortHandle>);

impl AbortOnDrop {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// Entry points for spawning onto the installed Tokio runtime.
#[derive(Debug)]
pub struct Tokio;

impl Tokio {
    /// Spawns `future` on Tokio's thread pool and returns its output through a GPUI
    /// task. Dropping the task aborts the Tokio task.
    ///
    /// Panics if neither [`init`] nor [`init_from_handle`] ran: that is a startup
    /// bug, not a runtime condition.
    pub fn spawn<C, Fut, R>(cx: &C, future: Fut) -> Task<Result<R, JoinError>>
    where
        C: AppContext,
        Fut: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        cx.read_global(|tokio: &GlobalTokio, cx| {
            let join_handle = tokio.handle.spawn(future);
            let abort = AbortOnDrop(Some(join_handle.abort_handle()));
            cx.background_spawn(async move {
                let result = join_handle.await;
                abort.disarm();
                result
            })
        })
    }

    /// Like [`Tokio::spawn`] for fallible futures: a Tokio join error (panic or
    /// abort) is converted into the future's own error type.
    pub fn spawn_result<C, Fut, R, E>(cx: &C, future: Fut) -> Task<Result<R, E>>
    where
        C: AppContext,
        Fut: Future<Output = Result<R, E>> + Send + 'static,
        R: Send + 'static,
        E: From<JoinError> + Send + 'static,
    {
        cx.read_global(|tokio: &GlobalTokio, cx| {
            let join_handle = tokio.handle.spawn(future);
            let abort = AbortOnDrop(Some(join_handle.abort_handle()));
            cx.background_spawn(async move {
                let result = join_handle.await;
                abort.disarm();
                result?
            })
        })
    }

    /// A handle to the installed runtime, for code that needs Tokio outside a task.
    pub fn handle(cx: &App) -> tokio::runtime::Handle {
        GlobalTokio::global(cx).handle.clone()
    }

    /// Whether a runtime is installed.
    pub fn is_initialized(cx: &App) -> bool {
        cx.has_global::<GlobalTokio>()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;

    use gpui::TestAppContext;

    use super::*;

    #[derive(Debug, PartialEq, thiserror::Error)]
    enum TestError {
        #[error("join failed")]
        Join,
        #[error("failed")]
        Failed,
    }

    impl From<JoinError> for TestError {
        fn from(_: JoinError) -> Self {
            Self::Join
        }
    }

    #[gpui::test]
    async fn spawns_onto_tokio(cx: &mut TestAppContext) {
        cx.update(|cx| init(cx).unwrap());
        cx.executor().allow_parking();
        let task = cx.update(|cx| {
            Tokio::spawn(cx, async {
                // Needs a Tokio reactor: fails outside the runtime.
                tokio::time::sleep(Duration::from_millis(1)).await;
                tokio::runtime::Handle::current().runtime_flavor()
            })
        });
        let flavor = task.await.unwrap();
        assert_eq!(flavor, tokio::runtime::RuntimeFlavor::MultiThread);

        let ok: Result<u8, TestError> = cx
            .update(|cx| Tokio::spawn_result(cx, async { Ok(7) }))
            .await;
        assert_eq!(ok, Ok(7));
        let err: Result<u8, TestError> = cx
            .update(|cx| Tokio::spawn_result(cx, async { Err(TestError::Failed) }))
            .await;
        assert_eq!(err, Err(TestError::Failed));
    }

    #[gpui::test]
    async fn dropping_the_task_aborts_tokio_work(cx: &mut TestAppContext) {
        cx.update(|cx| init(cx).unwrap());
        cx.executor().allow_parking();
        let finished = Arc::new(AtomicBool::new(false));
        let flag = finished.clone();
        let task = cx.update(|cx| {
            Tokio::spawn(cx, async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                flag.store(true, Ordering::SeqCst);
            })
        });
        drop(task);
        let handle = cx.update(|cx| Tokio::handle(cx));
        // Give the aborted task more than enough time to have finished otherwise.
        handle
            .spawn(async { tokio::time::sleep(Duration::from_millis(400)).await })
            .await
            .unwrap();
        assert!(!finished.load(Ordering::SeqCst));
    }
}
