//! Menu bar icon and its lifecycle actions.

use std::sync::LazyLock;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};

use crate::{lifecycle, window};

const TRAY_ID: &str = "printkit";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Labels {
    show: String,
    start: String,
    stop: String,
    quit: String,
    quit_keep: String,
    app_quit: String,
    tooltip: String,
}

static EN_CATALOG: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!(
        "../../../../packages/i18n/catalog/en.json"
    ))
    .unwrap_or(serde_json::Value::Null)
});
static ZH_CATALOG: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!(
        "../../../../packages/i18n/catalog/zh-CN.json"
    ))
    .unwrap_or(serde_json::Value::Null)
});

fn message(catalog: &serde_json::Value, key: &str) -> String {
    catalog
        .get("messages")
        .and_then(|messages| messages.get(key))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn labels(tag: &str) -> Labels {
    let catalog = if tag.starts_with("zh") {
        &*ZH_CATALOG
    } else {
        &*EN_CATALOG
    };
    Labels {
        show: message(catalog, "tray.show"),
        start: message(catalog, "tray.start"),
        stop: message(catalog, "tray.stop"),
        quit: message(catalog, "tray.quit"),
        quit_keep: message(catalog, "tray.quitKeep"),
        app_quit: message(catalog, "tray.appQuit"),
        tooltip: message(catalog, "tray.tooltip"),
    }
}

static MENU_LOCALE: LazyLock<std::sync::Mutex<String>> =
    LazyLock::new(|| std::sync::Mutex::new("en".to_owned()));

fn locale_state() -> &'static std::sync::Mutex<String> {
    &MENU_LOCALE
}

fn current_tag() -> String {
    locale_state()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
}

fn normalize(tag: &str) -> String {
    if tag.starts_with("zh") {
        "zh-CN".to_owned()
    } else {
        "en".to_owned()
    }
}

/// Cmd+Q must go through the same stop-then-exit path as the tray item.
pub fn install_app_menu(app: &AppHandle) -> tauri::Result<()> {
    let labels = labels(&current_tag());
    let about = PredefinedMenuItem::about(app, None, None)?;
    let hide = PredefinedMenuItem::hide(app, None)?;
    let hide_others = PredefinedMenuItem::hide_others(app, None)?;
    let show_all = PredefinedMenuItem::show_all(app, None)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(
        app,
        "app-quit",
        labels.app_quit,
        true,
        Some("CmdOrCtrl+Q"),
    )?;
    let app_menu = Submenu::with_items(
        app,
        "printkit desktop",
        true,
        &[&about, &hide, &hide_others, &show_all, &separator, &quit],
    )?;
    let menu = Menu::with_items(app, &[&app_menu])?;
    app.set_menu(menu)?;
    Ok(())
}

fn build_tray_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let labels = labels(&current_tag());
    let show = MenuItem::with_id(app, "show", labels.show, true, None::<&str>)?;
    let start = MenuItem::with_id(app, "start", labels.start, true, None::<&str>)?;
    let stop = MenuItem::with_id(app, "stop", labels.stop, true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", labels.quit, true, None::<&str>)?;
    let quit_keep =
        MenuItem::with_id(app, "quit-keep", labels.quit_keep, true, None::<&str>)?;
    let first_separator = PredefinedMenuItem::separator(app)?;
    let second_separator = PredefinedMenuItem::separator(app)?;
    let third_separator = PredefinedMenuItem::separator(app)?;
    Menu::with_items(
        app,
        &[
            &show,
            &start,
            &stop,
            &first_separator,
            &quit,
            &quit_keep,
            &second_separator,
            &third_separator,
        ],
    )
}

pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let labels = labels(&current_tag());
    let menu = build_tray_menu(app)?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip(labels.tooltip)
        .on_menu_event(|app, event| handle_menu(app, event.id().as_ref()))
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                window::reopen(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

/// The WebView is the only place that knows the user's locale; mirror it into
/// the native menu bar instead of shipping a second translation source.
pub fn set_locale(app: &AppHandle, tag: &str) {
    {
        let mut locale = locale_state()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *locale = normalize(tag);
    }
    let _ = install_app_menu(app);
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Ok(menu) = build_tray_menu(app) {
            let _ = tray.set_menu(Some(menu));
        }
        let _ = tray.set_tooltip(Some(labels(&current_tag()).tooltip));
    }
}

fn handle_menu(app: &AppHandle, id: &str) {
    match id {
        "show" => window::reopen(app),
        "start" => {
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                let attempt = lifecycle::start_attempt();
                handle
                    .state::<lifecycle::DesktopState>()
                    .set_auto_start(attempt.state, attempt.error);
                match lifecycle::start(&handle).await {
                    Ok(_) => {
                        let next = lifecycle::start_succeeded();
                        handle
                            .state::<lifecycle::DesktopState>()
                            .set_auto_start(next.state, next.error);
                    }
                    Err(error) => {
                        let next = lifecycle::start_failed(error.clone());
                        handle
                            .state::<lifecycle::DesktopState>()
                            .set_auto_start(next.state, next.error);
                        let _ = handle.emit("desktop-notice", error);
                    }
                }
            });
        }
        "stop" => {
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                match lifecycle::stop(&handle).await {
                    Ok(_) => {
                        let next = lifecycle::stopped_by_user();
                        handle
                            .state::<lifecycle::DesktopState>()
                            .set_auto_start(next.state, next.error);
                    }
                    Err(error) => {
                        let _ = handle.emit("desktop-notice", error);
                    }
                }
            });
        }
        "quit" => quit(app),
        "quit-keep" => quit_keep(app),
        _ => {}
    }
}

pub fn quit(app: &AppHandle) {
    let state = app.state::<lifecycle::DesktopState>();
    state
        .keep_service
        .store(false, std::sync::atomic::Ordering::SeqCst);
    if state
        .quitting
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        // Owned services stop with the shell; an external service is left
        // running because this shell never owned it.
        let _ = lifecycle::stop(&handle).await;
        handle.exit(0);
    });
}

pub fn quit_keep(app: &AppHandle) {
    let state = app.state::<lifecycle::DesktopState>();
    state
        .keep_service
        .store(true, std::sync::atomic::Ordering::SeqCst);
    if state
        .quitting
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    app.exit(0);
}

pub fn shutdown_on_exit(app: &AppHandle) {
    quit(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_labels_follow_the_ui_locale() {
        let en = labels("en");
        let zh = labels("zh-CN");
        assert_eq!(en.show, "Show window");
        assert_ne!(zh.show, en.show);
        assert_ne!(en.quit_keep, zh.quit_keep);
        assert_eq!(normalize("zh-Hans"), "zh-CN");
        assert_eq!(normalize("fr"), "en");
        assert_eq!(normalize("zh-TW"), "zh-CN");
    }
}
