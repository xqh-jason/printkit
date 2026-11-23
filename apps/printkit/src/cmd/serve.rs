//! `printkit serve`: the long-running local service.
//!
//! Startup order is deliberate: take the data-directory lock, load the admin
//! token, migrate the database, then bind the port. A second process fails
//! before it can touch state.

use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use printkit_facade::{self as facade, FetchPolicy, JsonlSink, NullSink, RotateOnOpen, Sink};
use printkit_store::Store;
use salvo::catcher::Catcher;
use salvo::prelude::*;
use tokio::runtime::Runtime;

use crate::config::{AppConfig, CliOverrides};
use crate::error::Failure;
use crate::service;
use crate::service::AppState;
use crate::service::lock::{AdminToken, ServeLock};
use crate::service::plugins::{self, GatedSink};

#[derive(Debug, Args)]
pub struct ServeArgs {}

pub fn run(_args: ServeArgs, overrides: CliOverrides) -> Result<(), Failure> {
    let config = AppConfig::load(overrides).map_err(Failure::from_config)?;
    let data_dir = config.data_dir.value.clone();
    let port = config.server.port.value;
    let _lock = acquire(&data_dir, port)?;
    let state = build(config)?;
    let runtime = runtime()?;
    let server = runtime.spawn(serve(state.clone(), port));
    install_shutdown(&runtime, &state);
    install_retention(&runtime, &state);
    plugins::spawn_lease_sweep(&runtime, state.clone());
    install_idempotency_sweep(&runtime, &state);
    // The main thread pumps the run loop instead of blocking on the runtime:
    // WKWebView dispatches every render to the main queue, which only drains
    // while this loop runs.
    facade::run_main_loop_while(|| !server.is_finished());
    facade::close_all_previews();
    runtime
        .block_on(server)
        .map_err(|error| Failure::io(format!("server task failed: {error}")))?
}

/// Ctrl-C and SIGTERM share one path: mark the service draining, wait up to
/// 2 s for in-flight jobs, then stop the listener. The run loop exits next and
/// the lock's destructor runs; the trace sink drains when its owner is dropped.
fn install_shutdown(runtime: &Runtime, state: &Arc<AppState>) {
    let state = state.clone();
    runtime.spawn(async move {
        #[cfg(unix)]
        {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async {
                    match terminate.as_mut() {
                        Some(signal) => {
                            signal.recv().await;
                        }
                        None => futures::future::pending::<()>().await,
                    }
                } => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        let (drained, inflight) = service::drain_jobs(&state, Duration::from_secs(2)).await;
        eprintln!("printkit: shutdown drained={drained} inflight={inflight}");
        stop_listener(&state);
    });
}
/// Trim `print_record` while the service runs. Expiry is not only a startup
/// concern: a long-lived desktop service must still honour retention.
fn install_retention(runtime: &Runtime, state: &Arc<AppState>) {
    let interval = {
        #[cfg(debug_assertions)]
        if let Ok(value) = std::env::var("PRINTKIT_RECORDS_SWEEP_SECS")
            && let Ok(seconds) = value.parse::<u64>()
        {
            Duration::from_secs(seconds.max(1))
        } else {
            Duration::from_secs(3600)
        }
        #[cfg(not(debug_assertions))]
        Duration::from_secs(3600)
    };
    let state = state.clone();
    runtime.spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            if !state.plugins.active("records") {
                continue;
            }
            let store = Arc::clone(&state.store);
            let days = state.records_retention_days;
            let rows = state.records_max_rows;
            match tokio::task::spawn_blocking(move || store.cleanup_print_records(days, rows)).await
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    eprintln!("printkit: periodic record cleanup failed: {error}");
                }
                Err(error) => {
                    eprintln!("printkit: periodic record cleanup task failed: {error}");
                }
            }
        }
    });
}

/// Trim per-key idempotency locks that no request holds or waits on.
fn install_idempotency_sweep(runtime: &Runtime, state: &Arc<AppState>) {
    let state = state.clone();
    runtime.spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let mut locks = state
                .idempotency_locks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            service::prune_idempotency_locks(&mut locks);
        }
    });
}

/// Take the one-process-per-data-directory lock.
pub(crate) fn acquire(data_dir: &std::path::Path, port: u16) -> Result<ServeLock, Failure> {
    std::fs::create_dir_all(data_dir)
        .map_err(|error| Failure::io(format!("cannot create {}: {error}", data_dir.display())))?;
    ServeLock::acquire(data_dir, port).map_err(|error| match error {
        service::lock::LockError::Running { .. } | service::lock::LockError::Held { .. } => {
            Failure::runtime(
                "E_ALREADY_RUNNING",
                format!("{error}; stop it before starting another serve"),
                1,
            )
        }
        _ => Failure::io(error.to_string()),
    })
}

/// Build the service state. The caller already owns the data-directory lock.
pub(crate) fn build(config: AppConfig) -> Result<Arc<AppState>, Failure> {
    let data_dir = config.data_dir.value.clone();
    let port = config.server.port.value;
    let admin = AdminToken::load_or_create(&data_dir)
        .map_err(|error| Failure::io(format!("cannot prepare the admin token: {error}")))?;
    let store = std::sync::Arc::new(
        Store::open(&data_dir)
            .map_err(|error| Failure::io(format!("cannot open the database: {error}")))?,
    );
    let plugins = plugins::PluginsState::serve(
        &data_dir,
        std::sync::Arc::clone(&store),
        config.records.retention_days.value,
        config.records.max_rows.value,
    )
    .map_err(|error| match error {
        printkit_plugin_host::FileError::Locked => Failure::runtime(
            "E_ALREADY_RUNNING",
            "another process holds plugins.lock; stop it before starting another serve",
            1,
        ),
        _ => Failure::io(format!("cannot load the plugin state: {error}")),
    })?;
    if plugins.active("records") {
        let _ = store.cleanup_print_records(
            config.records.retention_days.value,
            config.records.max_rows.value,
        );
    }
    let doctor_report = crate::cmd::doctor::report(&config)?;

    let level = config.log.level.value;
    let base: Arc<dyn Sink> = if level.writes_events() {
        let rotate = if plugins.active("observability") {
            RotateOnOpen::Yes
        } else {
            RotateOnOpen::No
        };
        Arc::new(JsonlSink::with_retention_opts(
            facade::logs_dir(&data_dir),
            level,
            config.log.retention_days.value,
            rotate,
        ))
    } else {
        Arc::new(NullSink)
    };
    let sink: Arc<dyn Sink> = Arc::new(GatedSink::new(base, plugins.observability_gate()));
    let fetch = FetchPolicy {
        timeout_ms: config.fetch.timeout_ms.value,
        max_bytes: config.fetch.max_bytes.value,
        max_redirects: config.fetch.max_redirects.value,
        allow_private_networks: super::allow_private_fetch(),
    };
    // Whether the renderers start with serve or on the first job is a
    // configuration choice; a crash later only rebuilds the instance that died.
    let pool = facade::new_render_pool(
        u32::from(config.render.workers.value),
        u32::from(config.render.queue_max.value),
        Duration::from_millis(u64::from(config.render.queue_timeout_ms.value)),
        config.render.warmup.value,
    );
    Ok(Arc::new(AppState::new(
        data_dir,
        port,
        store,
        Arc::new(plugins),
        admin.value().to_owned(),
        sink,
        pool,
        facade::new_printer_backend(),
        facade::new_render_host(),
        fetch,
        config.records.retention_days.value,
        config.records.max_rows.value,
        config.diagnostics.export_ttl_hours.value,
        config.preview.max_windows.value,
        doctor_report,
    )))
}

pub(crate) fn runtime() -> Result<Runtime, Failure> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| Failure::io(format!("cannot start the async runtime: {error}")))
}

/// Stop the listener; the main loop then ends because the server task finishes.
pub(crate) fn stop_listener(state: &AppState) -> bool {
    let handle = state.server.lock().ok().and_then(|mut slot| slot.take());
    match handle {
        Some(handle) => {
            handle.stop_graceful(Some(Duration::from_secs(2)));
            true
        }
        None => false,
    }
}

pub(crate) async fn serve(state: Arc<AppState>, port: u16) -> Result<(), Failure> {
    let router = service::router(state.clone());
    let acceptor = TcpListener::new(format!("127.0.0.1:{port}"))
        .try_bind()
        .await
        .map_err(|error| Failure::io(format!("cannot bind 127.0.0.1:{port}: {error}")))?;
    let server = Server::new(acceptor);
    if let Ok(mut slot) = state.server.lock() {
        *slot = Some(server.handle());
    }
    println!("listening on http://127.0.0.1:{port}");
    let _ = std::io::stdout().flush();

    server
        .try_serve(Service::new(router).catcher(Catcher::new(service::json_catcher)))
        .await
        .map_err(|error| Failure::io(format!("server stopped: {error}")))
}
