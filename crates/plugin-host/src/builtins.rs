//! The built-in plugin registry, composed from the `plugins/*` crates.

use std::sync::LazyLock;

use printkit_plugin_api::{CapabilitySpec, JobHook, Manifest, UiContribution};
use printkit_plugin_observability as observability;
use printkit_plugin_records as records;

/// Server-side capabilities of one built-in plugin.
///
/// The domain code stays in the host binary; this table is what the gate,
/// ownership checks and consistency tests read. A manifest alone cannot make a
/// plugin active.
#[derive(Clone)]
pub struct BuiltinSpec {
    /// Public manifest.
    pub manifest: Manifest,
    /// WebSocket methods owned by the plugin.
    pub methods: &'static [&'static str],
    /// `(method, path pattern)` HTTP routes owned by the plugin.
    pub admin_routes: &'static [(&'static str, &'static str)],
    /// CLI subcommands owned by the plugin.
    pub cli: &'static [&'static str],
    /// Store tables whose rows the plugin owns.
    pub tables: &'static [&'static str],

    /// Job hooks contributed by the plugin, empty in S1.
    pub hooks: &'static [&'static dyn JobHook],
    /// UI contributions, empty in S1.
    pub ui: &'static [UiContribution],
    /// Declared capabilities, empty in S1.
    pub capabilities: &'static [CapabilitySpec],
}

impl std::fmt::Debug for BuiltinSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BuiltinSpec")
            .field("id", &self.id())
            .field("methods", &self.methods.len())
            .field("admin_routes", &self.admin_routes.len())
            .field("cli", &self.cli.len())
            .field("tables", &self.tables.len())
            .field("hooks", &self.hooks.len())
            .field("ui", &self.ui.len())
            .field("capabilities", &self.capabilities.len())
            .finish()
    }
}

impl PartialEq for BuiltinSpec {
    fn eq(&self, other: &Self) -> bool {
        self.manifest == other.manifest
            && self.methods == other.methods
            && self.admin_routes == other.admin_routes
            && self.cli == other.cli
            && self.tables == other.tables
            && self.ui == other.ui
            && self.capabilities == other.capabilities
            && self.hooks.len() == other.hooks.len()
            && self
                .hooks
                .iter()
                .zip(other.hooks.iter())
                .all(|(left, right)| left.id() == right.id())
    }
}

impl BuiltinSpec {
    /// Build one registry row.
    pub fn new(
        manifest: Manifest,
        methods: &'static [&'static str],
        admin_routes: &'static [(&'static str, &'static str)],
        cli: &'static [&'static str],
        tables: &'static [&'static str],
    ) -> Self {
        Self {
            manifest,
            methods,
            admin_routes,
            cli,
            tables,
            hooks: &[],
            ui: &[],
            capabilities: &[],
        }
    }

    /// Attach the optional S2+ extension tables.
    pub fn with_extensions(
        mut self,
        hooks: &'static [&'static dyn JobHook],
        ui: &'static [UiContribution],
        capabilities: &'static [CapabilitySpec],
    ) -> Self {
        self.hooks = hooks;
        self.ui = ui;
        self.capabilities = capabilities;
        self
    }

    /// Plugin id.
    pub fn id(&self) -> &str {
        self.manifest.id.as_str()
    }
}

/// Every built-in plugin, in list order.
pub fn builtins() -> &'static [BuiltinSpec] {
    static BUILTINS: LazyLock<Vec<BuiltinSpec>> = LazyLock::new(build_builtins);
    BUILTINS.as_slice()
}

fn build_builtins() -> Vec<BuiltinSpec> {
    vec![
        BuiltinSpec::new(
            observability::manifest(),
            observability::METHODS,
            observability::ADMIN_ROUTES,
            observability::CLI,
            observability::TABLES,
        ),
        BuiltinSpec::new(
            records::manifest(),
            records::METHODS,
            records::ADMIN_ROUTES,
            records::CLI,
            records::TABLES,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn every_builtin_is_valid_and_unique() {
        let mut ids = BTreeSet::new();
        for spec in builtins() {
            spec.manifest.validate().unwrap_or_else(|error| {
                panic!("{} is invalid: {error}", spec.id());
            });
            assert!(ids.insert(spec.id()), "duplicate plugin id {}", spec.id());
            assert!(
                spec.manifest.name_key.is_some(),
                "{} has no nameKey",
                spec.id()
            );
        }
        assert_eq!(ids.len(), 2);
    }
    #[test]
    fn every_plugin_ui_page_is_unique_and_business_only() {
        let mut pages = BTreeSet::new();
        for spec in builtins() {
            for part in &spec.manifest.ui {
                assert_eq!(part.window, "business", "{} in {}", part.page, spec.id());
                assert!(
                    pages.insert(format!("{}/{}", part.window, part.page)),
                    "duplicate UI page in {}",
                    spec.id()
                );
            }
        }
    }

    const CORE_MIGRATIONS: &[&str] = &[include_str!(
        "../../../crates/store/migrations/V2__print_record.sql"
    )];

    fn synthetic(id: &str, tables: &'static [&'static str], gated: bool) -> BuiltinSpec {
        let mut manifest = observability::manifest();
        manifest.id = printkit_plugin_api::PluginId::new(id).unwrap();
        manifest.name = id.to_owned();
        manifest.requires = printkit_plugin_api::Requires::default();
        let (methods, routes) = if gated {
            (&["synthetic.method"][..], &[("GET", "admin/synthetic")][..])
        } else {
            (&[][..], &[][..])
        };
        BuiltinSpec::new(manifest, methods, routes, &[], tables)
    }

    fn table_contract_violations(specs: &[BuiltinSpec], migrations: &[&str]) -> Vec<String> {
        let mut owners: std::collections::BTreeMap<&str, Vec<&str>> =
            std::collections::BTreeMap::new();
        let mut violations = Vec::new();
        for spec in specs {
            for table in spec.tables {
                owners.entry(*table).or_default().push(spec.id());
                let created = migrations.iter().any(|sql| {
                    sql.contains(&format!("TABLE IF NOT EXISTS {table}"))
                        || sql.contains(&format!("TABLE {table}"))
                });
                if !created {
                    violations.push(format!("{} declares missing table {table}", spec.id()));
                }
                if spec.methods.is_empty() && spec.admin_routes.is_empty() {
                    violations.push(format!(
                        "{} owns table {table} without a gated entry point",
                        spec.id()
                    ));
                }
            }
        }
        for (table, ids) in owners {
            if ids.len() > 1 {
                violations.push(format!("table {table} is owned by {}", ids.join(", ")));
            }
        }
        violations
    }

    #[test]
    fn every_table_has_one_gated_owner_and_exists() {
        assert_eq!(
            table_contract_violations(builtins(), CORE_MIGRATIONS),
            Vec::<String>::new()
        );
    }

    #[test]
    fn duplicate_table_ownership_is_rejected() {
        let specs = [
            synthetic("owner-a", &["print_record"], true),
            synthetic("owner-b", &["print_record"], true),
        ];
        let violations = table_contract_violations(&specs, CORE_MIGRATIONS);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("owned by")),
            "{violations:?}"
        );
    }

    #[test]
    fn a_missing_table_is_rejected() {
        let specs = [synthetic("missing-table", &["no_such_table"], true)];
        let violations = table_contract_violations(&specs, CORE_MIGRATIONS);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("missing table")),
            "{violations:?}"
        );
    }

    #[test]
    fn a_table_owner_without_a_gated_entry_is_rejected() {
        let specs = [synthetic("ungated", &["print_record"], false)];
        let violations = table_contract_violations(&specs, CORE_MIGRATIONS);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("gated entry")),
            "{violations:?}"
        );
    }
}
