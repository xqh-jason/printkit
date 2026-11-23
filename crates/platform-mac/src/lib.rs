#![cfg_attr(
    not(target_os = "macos"),
    doc = "macOS platform implementation. This crate compiles to an empty library on other hosts."
)]
#![cfg(target_os = "macos")]
#![deny(missing_docs)]
#![cfg_attr(not(target_os = "macos"), allow(missing_docs))]
#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used))]
#![allow(unsafe_code)] // SAFETY comments on every unsafe block; see host.rs

//! macOS platform implementation.
//!
//! On any other host this crate compiles to an empty library: building it never
//! needs WebKit headers, and only the matching platform crate links framework
//! code.
//!
//! `unsafe` is allowed crate-wide because the WebKit and AppKit calls are
//! unsafe FFI by construction; each block carries a `// SAFETY:` note.

mod host;
mod merge;
mod preview;
mod print;

pub use host::{MacRenderHost, run_main_loop_while, webview_info};

/// Destroy every preview window. Called when the service stops serving.
pub fn close_all_previews() {
    preview::close_all();
}
pub use print::{MacPrinterBackend, SpoolerJob};
