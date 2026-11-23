//! Deterministic plugin state resolution.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use printkit_plugin_api::{Manifest, PluginId};
use serde::Serialize;

use crate::builtins::BuiltinSpec;
use crate::file::PluginsFile;

/// Where the current choice came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The `preset` default because no row exists.
    Preset,
    /// An explicit `[[plugins]]` row.
    User,
}

impl Source {
    /// Wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preset => "preset",
            Self::User => "user",
        }
    }
}

/// Resolved state of one plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Chosen and every dependency is active.
    On,
    /// Not chosen.
    Off,
    /// Chosen, but a required plugin is off or missing.
    Pending,
    /// Written for another plugin API major.
    Incompatible,
    /// Manifest failed validation.
    Invalid,
    /// The manifest file is broken; only the core runs.
    Broken,
}

impl State {
    /// Wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Pending => "pending",
            Self::Incompatible => "incompatible",
            Self::Invalid => "invalid",
            Self::Broken => "broken",
        }
    }
}

/// One plugin after composition and dependency resolution.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginStatus {
    /// Plugin id.
    pub id: PluginId,
    /// Resolved state.
    pub state: State,
    /// The explicit or preset choice, before dependencies.
    pub chosen: bool,
    /// Where the choice came from.
    pub source: Source,
    /// Why the state is not `on`, when it is not.
    pub reason: Option<String>,
}

/// A lock-free membership test for the active set.
#[derive(Debug)]
pub struct ActiveSet {
    bits: AtomicU64,
    positions: BTreeMap<String, u64>,
}

impl ActiveSet {
    /// Build from the built-in order and per-plugin active flags.
    pub fn new(specs: &[BuiltinSpec], active: &BTreeMap<String, bool>) -> Self {
        let mut positions = BTreeMap::new();
        let mut bits = 0u64;
        for (index, spec) in specs.iter().enumerate() {
            let bit = 1u64 << index;
            positions.insert(spec.id().to_owned(), bit);
            if active.get(spec.id()).copied().unwrap_or(false) {
                bits |= bit;
            }
        }
        Self {
            bits: AtomicU64::new(bits),
            positions,
        }
    }

    /// One atomic load, no allocation.
    pub fn active(&self, id: &str) -> bool {
        self.positions
            .get(id)
            .is_some_and(|bit| self.bits.load(Ordering::Relaxed) & *bit != 0)
    }

    /// Remove one plugin after its runtime service failed to start.
    pub fn clear(&self, id: &str) {
        if let Some(bit) = self.positions.get(id) {
            self.bits.fetch_and(!*bit, Ordering::SeqCst);
        }
    }
}

/// The whole resolved registry.
#[derive(Debug)]
pub struct Registry {
    /// Statuses in built-in order.
    pub statuses: Vec<PluginStatus>,
    /// Fast membership test.
    pub active: ActiveSet,
    /// Global broken diagnosis; when set, no plugin is active.
    pub broken: Option<String>,
}

impl Registry {
    /// Mark one plugin unavailable at runtime, without rewriting its file.
    pub fn fail(&mut self, id: &str, reason: String) {
        if let Some(status) = self
            .statuses
            .iter_mut()
            .find(|status| status.id.as_str() == id)
        {
            status.state = State::Invalid;
            status.reason = Some(reason);
        }
        self.active.clear(id);
    }

    /// Every plugin off because the file is broken.
    pub fn broken(specs: &[BuiltinSpec], reason: String) -> Self {
        let statuses = specs
            .iter()
            .map(|spec| PluginStatus {
                id: spec.manifest.id.clone(),
                state: State::Off,
                chosen: false,
                source: Source::Preset,
                reason: Some("the plugin file is broken".to_owned()),
            })
            .collect();
        Self {
            statuses,
            active: ActiveSet::new(specs, &BTreeMap::new()),
            broken: Some(reason),
        }
    }

    /// Resolve the file against the static registry.
    pub fn resolve(file: &PluginsFile, specs: &[BuiltinSpec]) -> Self {
        let mut chosen = BTreeMap::new();
        let mut source = BTreeMap::new();
        for spec in specs {
            let id = spec.id();
            let has_row = file.has_row(id);
            chosen.insert(id.to_owned(), file.chosen(id));
            source.insert(
                id.to_owned(),
                if has_row {
                    Source::User
                } else {
                    Source::Preset
                },
            );
        }

        let mut invalid = BTreeMap::new();
        let mut incompatible = BTreeMap::new();
        for spec in specs {
            let id = spec.id();
            if !spec.manifest.api.accepts(printkit_plugin_api::PLUGIN_API) {
                incompatible.insert(id.to_owned(), spec.manifest.api.to_string());
                continue;
            }
            if let Err(error) = spec.manifest.validate() {
                invalid.insert(id.to_owned(), error.to_string());
            }
        }

        for id in dependency_cycles(specs) {
            invalid
                .entry(id)
                .or_insert_with(|| "dependency cycle".to_owned());
        }
        for id in duplicate_providers(specs) {
            invalid
                .entry(id)
                .or_insert_with(|| "service provided by more than one plugin".to_owned());
        }

        let mut active: BTreeMap<String, bool> = BTreeMap::new();
        loop {
            let mut changed = false;
            for spec in specs {
                let id = spec.id();
                if active.get(id).copied().unwrap_or(false) {
                    continue;
                }
                if !chosen.get(id).copied().unwrap_or(false)
                    || invalid.contains_key(id)
                    || incompatible.contains_key(id)
                {
                    continue;
                }
                let dependencies = &spec.manifest.requires.plugins;
                if dependencies
                    .iter()
                    .all(|dependency| active.get(dependency.as_str()).copied().unwrap_or(false))
                {
                    active.insert(id.to_owned(), true);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        let statuses = specs
            .iter()
            .map(|spec| {
                let id = spec.id();
                let pick = chosen.get(id).copied().unwrap_or(false);
                let from = source.get(id).copied().unwrap_or(Source::Preset);
                let (state, reason) = if let Some(reason) = incompatible.get(id) {
                    (State::Incompatible, Some(reason.clone()))
                } else if let Some(reason) = invalid.get(id) {
                    (State::Invalid, Some(reason.clone()))
                } else if !pick {
                    (State::Off, None)
                } else if active.get(id).copied().unwrap_or(false) {
                    (State::On, None)
                } else {
                    let missing = missing_dependencies(spec, specs, &active);
                    (
                        State::Pending,
                        Some(format!("requires {}", missing.join(", "))),
                    )
                };
                PluginStatus {
                    id: spec.manifest.id.clone(),
                    state,
                    chosen: pick,
                    source: from,
                    reason,
                }
            })
            .collect();

        Self {
            statuses,
            active: ActiveSet::new(specs, &active),
            broken: None,
        }
    }
}

fn dependency_cycles(specs: &[BuiltinSpec]) -> std::collections::BTreeSet<String> {
    use std::collections::{BTreeSet, HashMap};

    let known: BTreeSet<String> = specs.iter().map(|spec| spec.id().to_owned()).collect();
    let graph: BTreeMap<String, Vec<String>> = specs
        .iter()
        .map(|spec| {
            (
                spec.id().to_owned(),
                spec.manifest
                    .requires
                    .plugins
                    .iter()
                    .map(ToString::to_string)
                    .filter(|id| known.contains(id))
                    .collect(),
            )
        })
        .collect();
    let mut colors: HashMap<String, u8> = HashMap::new();
    let mut stack = Vec::new();
    let mut cycles = BTreeSet::new();
    for id in graph.keys() {
        visit(id, &graph, &mut colors, &mut stack, &mut cycles);
    }
    cycles
}

fn visit(
    id: &str,
    graph: &BTreeMap<String, Vec<String>>,
    colors: &mut std::collections::HashMap<String, u8>,
    stack: &mut Vec<String>,
    cycles: &mut std::collections::BTreeSet<String>,
) {
    match colors.get(id).copied().unwrap_or(0) {
        2 => return,
        1 => {
            if let Some(position) = stack.iter().position(|entry| entry == id) {
                cycles.extend(stack[position..].iter().cloned());
            }
            return;
        }
        _ => {}
    }
    colors.insert(id.to_owned(), 1);
    stack.push(id.to_owned());
    if let Some(neighbours) = graph.get(id) {
        for neighbour in neighbours {
            visit(neighbour, graph, colors, stack, cycles);
        }
    }
    stack.pop();
    colors.insert(id.to_owned(), 2);
}

fn duplicate_providers(specs: &[BuiltinSpec]) -> std::collections::BTreeSet<String> {
    let mut providers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for spec in specs {
        for service in &spec.manifest.provides.services {
            providers
                .entry(service.clone())
                .or_default()
                .push(spec.id().to_owned());
        }
    }
    providers
        .into_values()
        .filter(|ids| ids.len() > 1)
        .flatten()
        .collect()
}
fn missing_dependencies(
    spec: &BuiltinSpec,
    specs: &[BuiltinSpec],
    active: &BTreeMap<String, bool>,
) -> Vec<String> {
    spec.manifest
        .requires
        .plugins
        .iter()
        .filter_map(|dependency| {
            if active.get(dependency.as_str()).copied().unwrap_or(false) {
                None
            } else if specs.iter().any(|known| known.id() == dependency.as_str()) {
                Some(dependency.to_string())
            } else {
                Some(format!("{dependency} (missing)"))
            }
        })
        .collect()
}

/// The manifest of one built-in spec, for callers that only need metadata.
pub fn manifest_of<'a>(specs: &'a [BuiltinSpec], id: &str) -> Option<&'a Manifest> {
    specs
        .iter()
        .find(|spec| spec.id() == id)
        .map(|spec| &spec.manifest)
}

/// Active ids among the protocol family list, in the caller's order.
pub fn active_families(active: &ActiveSet, ids: &[&str]) -> Vec<String> {
    ids.iter()
        .filter(|id| active.active(id))
        .map(|id| (*id).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::builtins;
    use crate::file::Preset;

    #[test]
    fn core_is_the_default_and_all_turns_every_builtin_on() {
        let core = Registry::resolve(&PluginsFile::default(), builtins());
        assert!(!core.active.active("observability"));
        assert!(!core.active.active("records"));
        let all = Registry::resolve(
            &PluginsFile {
                preset: Preset::All,
                ..PluginsFile::default()
            },
            builtins(),
        );
        assert!(all.active.active("observability"));
        assert!(all.active.active("records"));
        assert!(all.statuses.iter().all(|status| status.state == State::On));
    }

    #[test]
    fn core_turns_every_builtin_off() {
        let registry = Registry::resolve(&PluginsFile::core(), builtins());
        assert!(!registry.active.active("observability"));
        assert!(
            registry
                .statuses
                .iter()
                .all(|status| status.state == State::Off)
        );
    }

    #[test]
    fn a_row_beats_the_preset_and_is_sourced_from_the_user() {
        let file = PluginsFile {
            preset: Preset::All,
            plugins: vec![crate::file::PluginRow {
                id: PluginId::new("observability").unwrap(),
                enabled: false,
                version: None,
                source: None,
                integrity: None,
                config: None,
            }],
            ..PluginsFile::default()
        };
        let registry = Registry::resolve(&file, builtins());
        let status = registry
            .statuses
            .iter()
            .find(|status| status.id.as_str() == "observability")
            .unwrap();
        assert_eq!(status.state, State::Off);
        assert_eq!(status.source, Source::User);
        assert!(!status.chosen);
    }

    #[test]
    fn a_broken_file_activates_nothing() {
        let registry = Registry::broken(builtins(), "bad file".to_owned());
        assert!(registry.statuses.iter().all(|status| !status.chosen));
        assert!(!registry.active.active("records"));
    }

    fn spec(id: &str, requires: &[&str], provides: &[&str]) -> BuiltinSpec {
        use printkit_plugin_api::{ApiVersion, Kind, Permissions, Provides, Requires, UiPart};

        BuiltinSpec::new(
            Manifest {
                id: PluginId::new(id).unwrap(),
                name: id.to_owned(),
                sub: String::new(),
                version: "0.1.0".to_owned(),
                api: ApiVersion::new("^1").unwrap(),
                kind: Kind::Builtin,
                description: String::new(),
                requires: Requires {
                    plugins: requires
                        .iter()
                        .map(|id| PluginId::new(id).unwrap())
                        .collect(),
                    services: Vec::new(),
                },
                provides: Provides {
                    services: provides
                        .iter()
                        .map(|service| (*service).to_owned())
                        .collect(),
                },
                essential: false,
                ui: Vec::<UiPart>::new(),
                permissions: Permissions::default(),
                config: None,
                name_key: None,
            },
            &[],
            &[("GET", "admin/test")],
            &[],
            &[],
        )
    }

    #[test]
    fn a_requires_cycle_is_invalid_instead_of_pending() {
        let specs = [
            spec("cycle-a", &["cycle-b"], &[]),
            spec("cycle-b", &["cycle-a"], &[]),
        ];
        let registry = Registry::resolve(&PluginsFile::default(), &specs);
        for status in &registry.statuses {
            assert_eq!(status.state, State::Invalid, "{status:?}");
            assert_eq!(status.reason.as_deref(), Some("dependency cycle"));
            assert!(!registry.active.active(status.id.as_str()));
        }
    }

    #[test]
    fn a_duplicate_provided_service_is_invalid() {
        let specs = [
            spec("provide-a", &[], &["records.reader"]),
            spec("provide-b", &[], &["records.reader"]),
        ];
        let registry = Registry::resolve(&PluginsFile::default(), &specs);
        for status in &registry.statuses {
            assert_eq!(status.state, State::Invalid, "{status:?}");
            assert!(
                status
                    .reason
                    .as_deref()
                    .unwrap_or("")
                    .contains("provided by more than one")
            );
        }
    }
}
