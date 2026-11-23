//! Plugin manifests.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::api::ApiVersion;
use crate::config::ConfigSchema;
use crate::id::PluginId;

/// Where a plugin comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Compiled into the host binary.
    Builtin,
    /// A manifest-only package with a static UI page.
    External,
}

/// Other plugins a plugin cannot run without.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requires {
    /// Plugin ids that must be active first.
    #[serde(default)]
    pub plugins: Vec<PluginId>,
    /// Host services that must be available: `records.store`, `records.usage`.
    #[serde(default)]
    pub services: Vec<String>,
}

/// Services a plugin makes available to others.
///
/// P0 built-ins provide none; the field exists so a future code plugin can
/// declare a service without changing the manifest shape.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provides {
    /// Opaque service names, for example `records.reader`.
    #[serde(default)]
    pub services: Vec<String>,
}

/// The UI windows a plugin contributes.
///
/// This is the P0/external compatibility shape and keeps `entry` and `titles`;
/// S4 migrates single-window pages to [`crate::UiContribution`] without
/// dropping either field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiPart {
    /// Window label. S4 has one window, `business`.
    pub window: String,
    /// Page id inside that window.
    pub page: String,
    /// Localized titles, keyed by locale. External pages may omit a locale.
    #[serde(default)]
    pub titles: BTreeMap<String, String>,
    /// External page entry, relative to the plugin directory. Built-ins use a
    /// compiled Vue component and leave this `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
}

/// Where a plugin may write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileAccess {
    /// No filesystem access.
    #[default]
    None,
    /// Only `plugin-data/<id>/`.
    PluginData,
}

/// Bridge keys an external page may request from the host.
///
/// Every key defaults to `false`: the bridge is denied unless the manifest
/// opts in explicitly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgePermissions {
    /// Read this plugin's own non-sensitive configuration.
    #[serde(default)]
    pub self_config: bool,
    /// Read the printer list.
    #[serde(default)]
    pub printers: bool,
    /// Read aggregate record summaries, never raw rows.
    #[serde(default)]
    pub records_summary: bool,
}

/// Declared capabilities of a plugin. The host enforces these values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    /// Filesystem scope.
    #[serde(default)]
    pub filesystem: FileAccess,
    /// Network access. Always `false` for P1 external plugins.
    #[serde(default)]
    pub network: bool,
    /// Process spawning. Always `false` for P1 external plugins.
    #[serde(default)]
    pub process: bool,
    /// Printer access. Always `false` for external plugins.
    #[serde(default)]
    pub printer: bool,
    /// Raw device access. Always `false` for external plugins.
    #[serde(default)]
    pub raw: bool,
    /// Read-only bridge permissions.
    #[serde(default)]
    pub bridge: BridgePermissions,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            filesystem: FileAccess::None,
            network: false,
            process: false,
            printer: false,
            raw: false,
            bridge: BridgePermissions::default(),
        }
    }
}

/// One plugin as the host sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Stable plugin id.
    pub id: PluginId,
    /// Display name in English; the window translates `name_key` when present.
    pub name: String,
    /// One-line description shown under the name.
    #[serde(default)]
    pub sub: String,
    /// Display version.
    pub version: String,
    /// Host API range this plugin was written for.
    pub api: ApiVersion,
    /// Built-in or external.
    pub kind: Kind,
    /// Longer description for diagnostics.
    #[serde(default)]
    pub description: String,
    /// Plugins this one depends on.
    #[serde(default)]
    pub requires: Requires,
    /// Services this plugin provides to others.
    #[serde(default)]
    pub provides: Provides,
    /// Whether the plugin is part of an essentials preset.
    ///
    /// printkit's `core` preset still means "no builtin plugins"; the flag is
    /// preserved for a future preset and never enables a plugin by itself.
    #[serde(default)]
    pub essential: bool,
    /// UI contributions.
    #[serde(default)]
    pub ui: Vec<UiPart>,
    /// Declared permissions.
    #[serde(default)]
    pub permissions: Permissions,
    /// Configuration surface, when the plugin has settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigSchema>,
    /// Translation key for the display name, for example
    /// `plugins.observability.name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_key: Option<String>,
}

/// The on-disk shape of an external `plugin.toml`.
///
/// It has no `kind` field: an external file can never claim to be built in.
/// Server capabilities (methods, routes, CLI, tables) do not exist here either,
/// because P1 external plugins run no server code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawExternal {
    /// Stable plugin id.
    pub id: PluginId,
    /// Display name in English.
    pub name: String,
    /// One-line description.
    #[serde(default)]
    pub sub: String,
    /// Display version.
    pub version: String,
    /// Host API range this plugin was written for.
    pub api: ApiVersion,
    /// Longer description.
    #[serde(default)]
    pub description: String,
    /// Plugins this one depends on.
    #[serde(default)]
    pub requires: Requires,
    /// UI contributions; S4 allows `window = "business"` only.
    #[serde(default)]
    pub ui: Vec<UiPart>,
    /// Declared permissions.
    #[serde(default)]
    pub permissions: Permissions,
    /// Configuration surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigSchema>,
    /// Translation key, when the page supplies one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_key: Option<String>,
}

impl RawExternal {
    /// Convert a parsed external manifest into the host contract.
    pub fn into_manifest(self) -> Manifest {
        Manifest {
            id: self.id,
            name: self.name,
            sub: self.sub,
            version: self.version,
            api: self.api,
            kind: Kind::External,
            description: self.description,
            requires: self.requires,
            provides: Provides::default(),
            essential: false,
            ui: self.ui,
            permissions: self.permissions,
            config: self.config,
            name_key: self.name_key,
        }
    }
}

/// Why a manifest cannot be enabled.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ManifestError {
    /// The declared API major does not match the host.
    #[error("plugin {id} is written for plugin API {}; this host implements 1", api.major())]
    Api {
        /// Plugin id.
        id: PluginId,
        /// Declared range.
        api: ApiVersion,
    },
    /// The version is not three dot-separated numbers.
    #[error("plugin {id} has an invalid version: {version}")]
    Version {
        /// Plugin id.
        id: PluginId,
        /// Declared version.
        version: String,
    },
    /// A built-in declared an external entry.
    #[error("built-in plugin {0} must not declare a UI entry")]
    BuiltinEntry(PluginId),
    /// An external plugin omitted its entry.
    #[error("external plugin {0} must declare ui entry ui/index.html")]
    ExternalEntry(PluginId),
    /// A manifest declared a window other than `business`.
    #[error("plugin {id} may only contribute to the business window, not {window}")]
    ExternalWindow {
        /// Plugin id.
        id: PluginId,
        /// Declared window.
        window: String,
    },
    /// An external plugin requested a permission P1 never grants.
    #[error("external plugin {id} requests a forbidden permission: {permission}")]
    ExternalPermission {
        /// Plugin id.
        id: PluginId,
        /// Permission name.
        permission: &'static str,
    },
}

/// Whether a version is exactly `major.minor.patch` of ASCII digits.
fn valid_version(version: &str) -> bool {
    let mut parts = version.split('.');
    parts
        .by_ref()
        .map(|part| !part.is_empty() && part.chars().all(|character| character.is_ascii_digit()))
        .take(3)
        .filter(|valid| *valid)
        .count()
        == 3
        && parts.next().is_none()
}

impl Manifest {
    /// Check the invariants the host relies on before enabling a plugin.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if !self.api.accepts(crate::PLUGIN_API) {
            return Err(ManifestError::Api {
                id: self.id.clone(),
                api: self.api.clone(),
            });
        }
        if !valid_version(&self.version) {
            return Err(ManifestError::Version {
                id: self.id.clone(),
                version: self.version.clone(),
            });
        }
        if let Some(part) = self.ui.iter().find(|part| part.window != "business") {
            return Err(ManifestError::ExternalWindow {
                id: self.id.clone(),
                window: part.window.clone(),
            });
        }
        match self.kind {
            Kind::Builtin => {
                if self.ui.iter().any(|part| part.entry.is_some()) {
                    return Err(ManifestError::BuiltinEntry(self.id.clone()));
                }
            }
            Kind::External => {
                if !self
                    .ui
                    .iter()
                    .any(|part| part.entry.as_deref() == Some("ui/index.html"))
                {
                    return Err(ManifestError::ExternalEntry(self.id.clone()));
                }
                for (requested, name) in [
                    (self.permissions.network, "network"),
                    (self.permissions.process, "process"),
                    (self.permissions.printer, "printer"),
                    (self.permissions.raw, "raw"),
                ] {
                    if requested {
                        return Err(ManifestError::ExternalPermission {
                            id: self.id.clone(),
                            permission: name,
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn builtin() -> Manifest {
        Manifest {
            id: PluginId::new("observability").unwrap(),
            name: "Observability".to_owned(),
            sub: "Logs and records".to_owned(),
            version: "0.1.0".to_owned(),
            api: ApiVersion::new("^1").unwrap(),
            kind: Kind::Builtin,
            description: String::new(),
            requires: Requires::default(),
            provides: Provides::default(),
            essential: false,
            ui: vec![UiPart {
                window: "business".to_owned(),
                page: "logs".to_owned(),
                titles: BTreeMap::new(),
                entry: None,
            }],
            permissions: Permissions::default(),
            config: None,
            name_key: Some("plugins.observability.name".to_owned()),
        }
    }

    #[test]
    fn a_builtin_manifest_is_valid() {
        assert_eq!(builtin().validate(), Ok(()));
    }

    #[test]
    fn a_non_business_ui_part_is_rejected() {
        let mut manifest = builtin();
        manifest.ui[0].window = "dev".to_owned();
        match manifest.validate() {
            Err(ManifestError::ExternalWindow { window, .. }) => {
                assert_eq!(window, "dev");
            }
            other => panic!("expected ExternalWindow, got {other:?}"),
        }
    }

    #[test]
    fn bridge_permissions_default_to_false() {
        let parsed: Permissions = serde_json::from_value(json!({})).unwrap();
        assert_eq!(parsed, Permissions::default());
        assert!(!parsed.bridge.self_config);
        assert!(!parsed.bridge.printers);
        assert!(!parsed.bridge.records_summary);
    }

    #[test]
    fn bridge_permissions_are_opt_in() {
        let parsed: Permissions =
            serde_json::from_value(json!({"bridge": {"self_config": true}})).unwrap();
        assert!(parsed.bridge.self_config);
        assert!(!parsed.bridge.printers);
        assert!(!parsed.bridge.records_summary);
    }

    #[test]
    fn an_external_file_cannot_declare_kind() {
        let raw = json!({
            "id": "acme-receipts",
            "name": "Acme receipts",
            "version": "1.2.0",
            "api": "^1",
            "kind": "builtin"
        });
        assert!(serde_json::from_value::<RawExternal>(raw).is_err());
    }

    #[test]
    fn an_external_manifest_needs_one_business_entry_and_no_server_permissions() {
        let raw = RawExternal {
            id: PluginId::new("acme-receipts").unwrap(),
            name: "Acme receipts".to_owned(),
            sub: String::new(),
            version: "1.2.0".to_owned(),
            api: ApiVersion::new("^1").unwrap(),
            description: String::new(),
            requires: Requires::default(),
            ui: vec![UiPart {
                window: "business".to_owned(),
                page: "acme-receipts".to_owned(),
                titles: BTreeMap::new(),
                entry: Some("ui/index.html".to_owned()),
            }],
            permissions: Permissions::default(),
            config: None,
            name_key: None,
        }
        .into_manifest();
        assert_eq!(raw.kind, Kind::External);
        assert_eq!(raw.validate(), Ok(()));

        let mut forbidden = raw.clone();
        forbidden.permissions.raw = true;
        assert!(matches!(
            forbidden.validate(),
            Err(ManifestError::ExternalPermission {
                permission: "raw",
                ..
            })
        ));
    }

    #[test]
    fn versions_are_three_numbers() {
        for version in ["1.2.0", "0.1.0"] {
            assert!(valid_version(version));
        }
        for version in ["1", "1.2", "1.2.3.4", "v1.2.3", "1.2.x", ""] {
            assert!(!valid_version(version), "{version:?} was accepted");
        }
    }
}
