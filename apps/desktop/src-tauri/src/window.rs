//! The single business window.
//!
//! S4 removed the developer window and the on-demand data windows; Tauri
//! creates and sizes `business` from `tauri.conf.json`, and this module only
//! raises it again from the Dock or a second launch.

use tauri::{AppHandle, Manager};

pub const BUSINESS: &str = "business";

pub fn show_business(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(BUSINESS) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Reopen from the Dock/menu bar. There is only one window.
pub fn reopen(app: &AppHandle) {
    show_business(app);
}
