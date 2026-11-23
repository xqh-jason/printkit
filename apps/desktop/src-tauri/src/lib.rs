mod commands;
mod control;
mod layout;
mod lifecycle;
mod tray;
mod window;

use std::sync::atomic::Ordering;

use tauri::{Manager, RunEvent, WindowEvent};

#[tauri::command]
async fn snapshot(window: tauri::WebviewWindow, app: tauri::AppHandle) -> Result<lifecycle::Snapshot, String> {
    commands::require_window(&window, commands::WindowCapability::Business)?;
    // `status` reads the lock and probes a loopback port; keep the IPC worker
    // free even when the service or the filesystem is slow.
    let handle = app.clone();
    let snapshot = tauri::async_runtime::spawn_blocking(move || lifecycle::snapshot(&handle))
        .await
        .unwrap_or_else(|_| lifecycle::snapshot(&app));
    Ok(snapshot)
}

#[tauri::command]
async fn start_service(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
) -> Result<lifecycle::ServiceStatus, String> {
    commands::require_window(&window, commands::WindowCapability::Business)?;
    let result = lifecycle::start(&app).await;
    let state = app.state::<lifecycle::DesktopState>();
    let next = match &result {
        Ok(_) => lifecycle::start_succeeded(),
        Err(error) => lifecycle::start_failed(error.clone()),
    };
    state.set_auto_start(next.state, next.error);
    result
}

#[tauri::command]
async fn stop_service(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
) -> Result<lifecycle::ServiceStatus, String> {
    commands::require_window(&window, commands::WindowCapability::Business)?;
    let result = lifecycle::stop(&app).await;
    if result.is_ok() {
        let next = lifecycle::stopped_by_user();
        app.state::<lifecycle::DesktopState>()
            .set_auto_start(next.state, next.error);
    }
    result
}

#[tauri::command]
async fn retry_start(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
) -> Result<lifecycle::ServiceStatus, String> {
    commands::require_window(&window, commands::WindowCapability::Business)?;
    lifecycle::retry_start(&app).await
}

#[tauri::command]
async fn refresh_config(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
) -> Result<lifecycle::ConfigView, String> {
    commands::require_window(&window, commands::WindowCapability::Business)?;
    lifecycle::refresh_config(&app).await
}

#[tauri::command]
fn set_locale(app: tauri::AppHandle, locale: String) {
    tray::set_locale(&app, &locale);
}

/// Keep the native title bar/scrollbars in step with the page. `data-theme`
/// stays the single source of truth; "system" hands control back to the OS.
#[tauri::command]
fn set_theme(window: tauri::WebviewWindow, theme: String) {
    let native = match theme.as_str() {
        "dark" => Some(tauri::Theme::Dark),
        "light" => Some(tauri::Theme::Light),
        _ => None,
    };
    let _ = window.set_theme(native);
}

pub fn run() {
    let app = tauri::Builder::default()
        // The single-instance plugin must be registered first: a second launch
        // hands its arguments to the running process and exits.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            window::show_business(app);
        }))
        .plugin(tauri_plugin_shell::init())
        .on_menu_event(|app, event| {
            if event.id().as_ref() == "app-quit" {
                tray::quit(app);
            }
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            start_service,
            stop_service,
            retry_start,
            refresh_config,
            commands::get_retention_days,
            commands::set_retention_days,
            set_locale,
            set_theme,
            commands::records_summary,
            commands::records_list,
            commands::records_stats,
            commands::export_records,
            commands::logs_events,
            commands::export_logs,

            commands::plugin_list,
            commands::plugin_set,
            commands::plugin_enable_owned,
            commands::plugin_lease,
            commands::list_printers,
            commands::list_papers,
            commands::print_test,
            commands::print_pdf_bytes,
            commands::origins_list,
            commands::origin_add,
            commands::origin_remove,
            commands::job_status,
            commands::job_cancel,
            commands::reveal_path,
            commands::open_data_dir
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            app.manage(lifecycle::DesktopState::new());
            layout::apply_saved(&handle);
            tray::install_app_menu(&handle)?;
            tray::install(&handle)?;
            lifecycle::install_plugin_refresh(&handle);
            lifecycle::install_signal_handlers(&handle);
            // Probing runs off the setup thread so a slow renderer or a
            // missing sidecar cannot delay the first window.
            tauri::async_runtime::spawn(async move {
                lifecycle::probe(&handle).await;
                lifecycle::auto_start(&handle).await;
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            match event {
                WindowEvent::Resized(_) | WindowEvent::Moved(_) => {
                    layout::remember_window(window);
                }
                WindowEvent::CloseRequested { api, .. } => {
                    // Flush the last drag/geometry before the window disappears.
                    layout::remember_window_now(window);
                    if window.label() == window::BUSINESS {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to build printkit desktop");

    app.run(|app, event| {
        match event {
            RunEvent::ExitRequested { api, .. } => {
                let state = app.state::<lifecycle::DesktopState>();
                if !state.quitting.load(Ordering::SeqCst) {
                    api.prevent_exit();
                    tray::shutdown_on_exit(app);
                }
            }
            RunEvent::Exit => {
                for (_, webview) in app.webview_windows() {
                    layout::remember_window_now(&webview.as_ref().window());
                }
                lifecycle::kill_owned_child(app);
            }
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => window::reopen(app),
            _ => {}
        }
    });
}
