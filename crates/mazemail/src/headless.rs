//! `mazemail --headless`: the core and the local API without a window (PLAN §4.11).
//!
//! Opens the core on a multi-thread Tokio runtime, binds the socket with the
//! terminal approver (or the debug-only test auto-approver), serves until SIGINT or
//! SIGTERM, then stops accepting, lets in-flight requests finish, removes the socket
//! and shuts the core down (index committed, store closed, data directory unlocked).

use std::future::Future;

use anyhow::Context as _;
use local_api::LocalApiServer;
use mazemail::{approve, keys, startup};
use mazemail_core::CoreHandle;
use mazemail_core::api::{self, TokenAuthorizer};
use mazemail_types::PRODUCT_NAME;
use settings::Paths;
use time::UtcOffset;

/// Runs until interrupted.
pub(crate) fn run(utc_offset: UtcOffset) -> anyhow::Result<()> {
    let paths = Paths::current().context("finding the data directory")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("mazemail-headless")
        .build()
        .context("starting the runtime")?;
    runtime.block_on(serve(paths, utc_offset))
}

async fn serve(paths: Paths, utc_offset: UtcOffset) -> anyhow::Result<()> {
    let key_store = keys::key_store(paths.data_dir())?;
    let authorizer = TokenAuthorizer::new();
    let config = startup::core_config(&paths, key_store, &authorizer, utc_offset)
        .with_runtime(tokio::runtime::Handle::current());
    let core = match CoreHandle::open(config).await {
        Ok(core) => core,
        Err(error) => anyhow::bail!(startup::describe_open_error(&error)),
    };

    let services = api::services(&core, &authorizer, approve::headless_approver());
    let server = match LocalApiServer::bind(&paths, services, startup::server_config()).await {
        Ok(server) => server,
        Err(error) => {
            // Release the data directory before reporting.
            if let Err(shutdown) = core.shutdown().await {
                tracing::warn!(%shutdown, "shutting down the core");
            }
            return Err(error).context("starting the local API");
        }
    };
    if let Err(error) = core.mark_healthy() {
        tracing::warn!(%error, "clearing the startup sentinel");
    }
    tracing::info!(
        socket = %server.path().display(),
        data_dir = %paths.data_dir().display(),
        "{PRODUCT_NAME} is running headless; stop it with Ctrl-C"
    );
    server.serve(shutdown_signal()?).await;
    core.shutdown().await.context("shutting down")?;
    tracing::info!("{PRODUCT_NAME} stopped");
    Ok(())
}

/// Resolves on the first SIGINT or SIGTERM.
fn shutdown_signal() -> anyhow::Result<impl Future<Output = ()> + Send> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt()).context("listening for SIGINT")?;
    let mut terminate = signal(SignalKind::terminate()).context("listening for SIGTERM")?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => tracing::info!("interrupted; stopping"),
            _ = terminate.recv() => tracing::info!("terminated; stopping"),
        }
    })
}
