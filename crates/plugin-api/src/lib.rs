#![deny(missing_docs)]
#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used))]

//! Stable contract of a printkit plugin.
//!
//! This crate is a leaf: it depends on `serde` and `thiserror` only, never on
//! the host or any internal printkit crate. Built-in plugins and external
//! `plugin.toml` files describe themselves with the same [`Manifest`]; the
//! host side (registry, file, lock) lives in `printkit-plugin-host` so a plugin
//! can never reach it.

mod api;
mod capability;
mod config;
mod hook;
mod id;
mod manifest;
mod service;
mod ui;

pub use api::{ApiError, ApiVersion};
pub use capability::{CapabilityKind, CapabilityKindError, CapabilitySpec};
pub use config::{ConfigError, ConfigProperty, ConfigSchema, ConfigValueKind, validate_config};
pub use hook::{DEFAULT_HOOK_TIMEOUT, HookCtx, HookError, HookOutcome, HookPhase, JobHook};
pub use id::{IdError, PluginId};
pub use manifest::{
    BridgePermissions, FileAccess, Kind, Manifest, ManifestError, Permissions, Provides,
    RawExternal, Requires, UiPart,
};
pub use service::{
    RecordPage, RecordQuery, RecordRow, RecordScope, RecordStats, RecordStore, RecordSummary,
    UsageRow, UsageSink,
};
pub use ui::{UiContractError, UiContribution, UiSlot, validate_ui_contributions};

/// Plugin API major version the host implements.
///
/// A manifest declares a range such as `^1`; only a compatible major version
/// is enabled. A breaking contract change bumps this and the crate version.
pub const PLUGIN_API: u32 = 1;
