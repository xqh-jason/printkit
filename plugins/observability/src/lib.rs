#![deny(missing_docs)]
#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used))]

//! Built-in observability plugin (P0: manifest and owned capabilities).
//!
//! The domain code lives in the host binary; this crate only declares what the
//! plugin owns and depends on `printkit-plugin-api` alone.

use std::collections::BTreeMap;

use printkit_plugin_api::{
    ApiVersion, Kind, Manifest, Permissions, PluginId, Provides, Requires, UiPart,
};

/// WebSocket methods owned by this plugin.
pub const METHODS: &[&str] = &["trace.subscribe", "trace.tail", "logs.export"];

/// `(method, path pattern)` routes owned by this plugin.
pub const ADMIN_ROUTES: &[(&str, &str)] =
    &[("GET", "admin/logs/events"), ("POST", "admin/logs/export")];

/// CLI subcommands owned by this plugin.
pub const CLI: &[&str] = &["logs"];

/// Store tables whose rows this plugin owns.
pub const TABLES: &[&str] = &[];

/// The plugin manifest.
pub fn manifest() -> Manifest {
    Manifest {
        id: PluginId::new("observability").expect("a literal plugin id"),
        name: "Observability".to_owned(),
        sub: "Trace logs and exports".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        api: ApiVersion::new("^1").expect("a literal API range"),
        kind: Kind::Builtin,
        description: "Trace logs and exports".to_owned(),
        requires: Requires::default(),
        provides: Provides::default(),
        essential: false,
        ui: [("business", "jobs"), ("business", "logs")]
            .into_iter()
            .map(|(window, page)| UiPart {
                window: window.to_owned(),
                page: page.to_owned(),
                titles: BTreeMap::new(),
                entry: None,
            })
            .collect(),
        permissions: Permissions::default(),
        config: None,
        name_key: Some("plugins.observability.name".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_is_valid() {
        manifest().validate().unwrap();
        assert_eq!(manifest().kind, Kind::Builtin);
    }
}
