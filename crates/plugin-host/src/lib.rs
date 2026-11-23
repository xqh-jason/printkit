#![deny(missing_docs)]
#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used))]

//! Host side of the built-in plugin system.
//!
//! Only the composition root (`apps/printkit`) depends on this crate. It holds
//! the static [`BuiltinSpec`] table, the `plugins.toml` format, the exclusive
//! lock and the deterministic state resolver. Plugin domain code receives
//! explicit handles and never sees this crate.

mod builtins;
mod file;
mod hook;
mod registry;
mod services;

pub use builtins::{BuiltinSpec, builtins};
pub use file::{
    CURRENT_SETTLED, FileError, LoadedFile, PLUGINS_BACKUP, PLUGINS_CORE_BACKUP, PLUGINS_FILE,
    PLUGINS_LOCK, PluginLock, PluginRow, PluginsFile, Preset, load, migrate_core, save,
};
pub use hook::HookRunner;
#[cfg(debug_assertions)]
pub use hook::test_hook_from_env;
pub use printkit_plugin_api::{
    CapabilityKind, CapabilitySpec, DEFAULT_HOOK_TIMEOUT, HookCtx, HookError, HookOutcome,
    HookPhase, JobHook, UiContractError, UiContribution, UiSlot, validate_ui_contributions,
};
pub use registry::{
    ActiveSet, PluginStatus, Registry, Source, State, active_families, manifest_of,
};
pub use services::{PluginServices, RECORD_STORE, RECORDS_USAGE, ServiceRegistry};
