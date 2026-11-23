#![cfg_attr(
    not(windows),
    doc = "Windows platform implementation; empty on other hosts, see platform-win sources."
)]
#![cfg(windows)]
#![cfg_attr(not(windows), allow(missing_docs))]
#![deny(missing_docs)]
#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used))]
#![allow(unsafe_code)] // SAFETY comments on every unsafe block; see host.rs/print.rs

//! Windows platform implementation.
//!
//! Implemented: WebView2 runtime detection, HTML to PDF through
//! `PrintToPdfStream` on a per-render STA thread with `HWND_MESSAGE` (offscreen
//! popup fallback), row measurement through `ExecuteScript`, printer and paper
//! enumeration, and raw spooler submission with a real job id plus
//! `JOB_CONTROL_DELETE` cancellation.
//!
//! Implemented but not yet verified on a Windows device: WebView2 `Print`
//! for direct driver printing, preview windows, mixed-paper rendering and the
//! K-instance render pool. Calls that cannot run return an explicit
//! `PlatformError` instead of a fake success or a panic.
//!
//! On any other host this crate compiles to an empty library, the same shape as
//! `platform-mac`: the workspace builds on every host, and only the matching
//! platform crate links OS libraries.

mod host;
mod preview;
mod print;
mod sys;

pub use host::{MIN_RUNTIME_VERSION, WinRenderHost, run_main_loop_while, webview_info};
pub use print::WinPrinterBackend;
pub use sys::{process_alive, restrict_to_current_user};

/// Default render-worker count for `cores` logical CPUs.
///
/// WebView2 wants one STA thread per instance, so the pool stays small: half
/// the cores, at least one and at most four. `render.workers` may still lower
/// it; the service reports the effective count in `sys.info`.
pub fn render_workers(cores: usize) -> u8 {
    u8::try_from((cores / 2).clamp(1, 4)).unwrap_or(1)
}

/// Destroy every preview window. Called when the service stops serving.
pub fn close_all_previews() {
    preview::close_all();
}

#[cfg(test)]
mod workers_tests {
    use super::render_workers;

    #[test]
    fn worker_count_halves_cores_and_caps_at_four() {
        assert_eq!(render_workers(1), 1);
        assert_eq!(render_workers(2), 1);
        assert_eq!(render_workers(4), 2);
        assert_eq!(render_workers(8), 4);
        assert_eq!(render_workers(64), 4);
    }
}
