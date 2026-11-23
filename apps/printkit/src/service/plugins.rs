//! Plugin state, the `/admin/plugins` face and read gates.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use printkit_plugin_api::{PluginId, validate_config};
use printkit_plugin_host::{
    BuiltinSpec, FileError, PluginLock, PluginRow, PluginsFile, Preset, RECORD_STORE,
    RECORDS_USAGE, Registry, ServiceRegistry, active_families, builtins, load, save,
};
use printkit_plugin_records::RecordsDomain;
use printkit_store::Store;
use salvo::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::AppState;
use super::admin::{self, WriteDenied};

/// Leases expire after this long without a heartbeat.
pub const LEASE_TTL: Duration = Duration::from_secs(60);

fn lease_ttl() -> Duration {
    #[cfg(debug_assertions)]
    if let Ok(value) = std::env::var("PRINTKIT_PLUGIN_LEASE_TTL_SECS")
        && let Ok(seconds) = value.parse::<u64>()
        && seconds > 0
    {
        return Duration::from_secs(seconds);
    }
    LEASE_TTL
}

/// The plugin state shared by the service.
pub struct PluginsState {
    dir: PathBuf,
    store: Option<Arc<Store>>,
    file: RwLock<PluginsFile>,
    registry: RwLock<Registry>,
    write: Mutex<()>,
    leases: Mutex<HashMap<(String, String), Instant>>,
    ws_subscribers: AtomicUsize,
    observability: Arc<AtomicBool>,
    usage: RwLock<Option<printkit_facade::UsageRecorder>>,
    records: RwLock<Option<RecordsDomain>>,
    records_error: Mutex<Option<String>>,
    fail_records_writer: AtomicBool,
    retention_days: u16,
    max_rows: u32,
    _lock: Mutex<Option<PluginLock>>,
}

impl PluginsState {
    /// Load the file and keep `plugins.lock` for this process.
    ///
    /// The store is optional only so tests can load state without a database.
    pub fn serve(
        dir: &Path,
        store: Arc<Store>,
        retention_days: u16,
        max_rows: u32,
    ) -> Result<Self, FileError> {
        let lock = match PluginLock::try_acquire(dir)? {
            Some(lock) => lock,
            None => return Err(FileError::Locked),
        };
        Ok(Self::from_lock(
            dir,
            Some(lock),
            Some(store),
            retention_days,
            max_rows,
            false,
        ))
    }

    /// Load the file without taking the lock; tests and one-shot readers.
    #[cfg(test)]
    pub fn load_only(dir: &Path) -> Self {
        Self::from_lock(dir, None, None, 0, 0, false)
    }

    fn from_lock(
        dir: &Path,
        lock: Option<PluginLock>,
        store: Option<Arc<Store>>,
        retention_days: u16,
        max_rows: u32,
        fail_records_writer: bool,
    ) -> Self {
        let loaded = if lock.is_some() {
            match printkit_plugin_host::migrate_core(dir) {
                Ok(_) => load(dir),
                Err(error) => printkit_plugin_host::LoadedFile {
                    file: PluginsFile::core(),
                    broken: Some(format!("cannot migrate plugins.toml: {error}")),
                    exists: true,
                },
            }
        } else {
            load(dir)
        };
        let mut registry = loaded.registry(builtins());
        let records_on = registry.active.active("records");
        let (usage, records, records_error) = if records_on {
            match store.as_ref() {
                Some(store) if !fail_records_writer => {
                    match printkit_facade::UsageRecorder::try_from_store(
                        Arc::clone(store),
                        String::new(),
                        retention_days,
                        max_rows,
                    ) {
                        Ok(recorder) => {
                            let domain = start_records(store, &recorder);
                            (Some(recorder), Some(domain), None)
                        }
                        Err(error) => (None, None, Some(error)),
                    }
                }
                Some(_) => (
                    None,
                    None,
                    Some("injected records writer startup failure".to_owned()),
                ),
                None => (
                    None,
                    None,
                    Some("the plugin store is unavailable".to_owned()),
                ),
            }
        } else {
            (None, None, None)
        };
        if let Some(reason) = &records_error {
            eprintln!("printkit: records is unavailable: {reason}");
            registry.fail("records", reason.clone());
        }
        let observability = Arc::new(AtomicBool::new(registry.active.active("observability")));
        Self {
            dir: dir.to_path_buf(),
            store,
            file: RwLock::new(loaded.file),
            registry: RwLock::new(registry),
            write: Mutex::new(()),
            leases: Mutex::new(HashMap::new()),
            ws_subscribers: AtomicUsize::new(0),
            observability,
            usage: RwLock::new(usage),
            records: RwLock::new(records),
            records_error: Mutex::new(records_error),
            fail_records_writer: AtomicBool::new(false),
            retention_days,
            max_rows,
            _lock: Mutex::new(lock),
        }
    }

    /// Startup with records requested but the writer failing.
    #[cfg(test)]
    pub fn all_for_tests_with_writer_failure(dir: &Path) -> Self {
        let file = PluginsFile {
            preset: Preset::All,
            ..PluginsFile::default()
        };
        let _ = save(dir, &file);
        let store = Arc::new(Store::open(dir).expect("a test store"));
        Self::from_lock(dir, None, Some(store), 90, 5000, true)
    }

    /// Core-preset state without a usage writer, for gate tests.
    #[cfg(test)]
    pub fn core_for_tests(dir: &Path) -> Self {
        let file = PluginsFile {
            preset: Preset::Core,
            ..PluginsFile::default()
        };
        let _ = save(dir, &file);
        let store = Arc::new(Store::open(dir).expect("a test store"));
        Self::from_lock(dir, None, Some(store), 90, 5000, false)
    }

    /// All-on state with a usage writer, used by service unit tests.
    #[cfg(test)]
    pub fn all_for_tests(dir: &Path) -> Self {
        let file = PluginsFile {
            preset: Preset::All,
            ..PluginsFile::default()
        };
        let _ = save(dir, &file);
        let store = Arc::new(Store::open(dir).expect("a test store"));
        Self::from_lock(dir, None, Some(store), 90, 5000, false)
    }

    /// Whether a plugin is active. Hot path: one atomic load.
    pub fn active(&self, id: &str) -> bool {
        self.registry
            .read()
            .map(|registry| registry.active.active(id))
            .unwrap_or(false)
    }

    /// Active plugin ids among the protocol families.
    pub fn active_families(&self) -> Vec<String> {
        let registry = match self.registry.read() {
            Ok(registry) => registry,
            Err(_) => return Vec::new(),
        };
        active_families(&registry.active, &["observability", "records"])
    }

    /// The gate read by [`GatedSink`].
    pub fn observability_gate(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.observability)
    }

    /// A usage writer clone bound to one request origin.
    ///
    /// Records off means no writer and no rows, matching the plugin gate.
    pub fn usage_recorder(&self, origin: &str) -> Option<printkit_facade::UsageRecorder> {
        if !self.active("records") {
            return None;
        }
        self.usage
            .read()
            .ok()?
            .as_ref()
            .map(|recorder| recorder.with_origin(origin.to_owned()))
    }

    /// The bound records domain, when the plugin is on and services resolved.
    pub fn records_domain(&self) -> Option<RecordsDomain> {
        self.records.read().ok()?.clone()
    }

    fn records_error(&self) -> Option<String> {
        self.records_error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    fn clear_records_error(&self) {
        *self
            .records_error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
    }

    /// Loss counters on the live records writer.
    pub fn usage_dropped(&self) -> Option<UsageDroppedView> {
        self.usage
            .read()
            .ok()?
            .as_ref()
            .map(|recorder| UsageDroppedView {
                dropped_after_close: recorder.dropped_after_close(),
                dropped_full: recorder.dropped_full(),
            })
    }

    /// The diagnosis for a broken file.
    pub fn broken(&self) -> Option<String> {
        self.registry
            .read()
            .ok()
            .and_then(|registry| registry.broken.clone())
    }

    /// The in-memory file.
    pub fn file(&self) -> PluginsFile {
        self.file
            .read()
            .map(|file| file.clone())
            .unwrap_or_default()
    }

    /// Count of live trace subscriptions.
    pub fn ws_subscribers(&self) -> usize {
        self.ws_subscribers.load(Ordering::Relaxed)
    }

    /// One trace subscription became active.
    pub fn subscribe_begin(&self) {
        self.ws_subscribers.fetch_add(1, Ordering::Relaxed);
    }

    /// One trace subscription ended.
    pub fn subscribe_end(&self) {
        self.ws_subscribers.fetch_sub(1, Ordering::Relaxed);
    }

    fn sweep_leases(&self, now: Instant) -> HashMap<(String, String), Instant> {
        let mut leases = self
            .leases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        leases.retain(|_, expiry| *expiry > now);
        leases.clone()
    }

    fn in_use_detail(&self, id: &str, leases: &HashMap<(String, String), Instant>) -> (u64, u64) {
        let ws = if id == "observability" {
            u64::try_from(self.ws_subscribers()).unwrap_or(u64::MAX)
        } else {
            0
        };
        let leased = leases.keys().filter(|(plugin, _)| plugin == id).count();
        (ws, u64::try_from(leased).unwrap_or(u64::MAX))
    }

    /// Active sessions plus leases for one plugin.
    pub fn in_use(&self, id: &str) -> u64 {
        let leases = self.sweep_leases(Instant::now());
        let (ws, leased) = self.in_use_detail(id, &leases);
        ws.saturating_add(leased)
    }

    /// Acquire, heartbeat or release a desktop lease.
    pub fn lease(&self, id: &str, op: &str, lease_id: &str) -> Result<u64, PluginError> {
        self.known_plugin(id)?;
        if lease_id.trim().is_empty() {
            return Err(PluginError::Schema("a lease needs a leaseId".to_owned()));
        }
        let now = Instant::now();
        match op {
            "acquire" | "heartbeat" => {
                let mut leases = self
                    .leases
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                leases.retain(|_, expiry| *expiry > now);
                leases.insert((id.to_owned(), lease_id.to_owned()), now + lease_ttl());
            }
            "release" => {
                let mut leases = self
                    .leases
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                leases.retain(|_, expiry| *expiry > now);
                leases.remove(&(id.to_owned(), lease_id.to_owned()));
            }
            other => {
                return Err(PluginError::Schema(format!(
                    "unknown lease operation: {other}"
                )));
            }
        }
        Ok(self.in_use(id))
    }

    /// Drop expired leases; the service calls this periodically.
    pub fn sweep(&self) {
        self.sweep_leases(Instant::now());
    }

    fn known_plugin(&self, id: &str) -> Result<(), PluginError> {
        if builtins().iter().any(|spec| spec.id() == id) {
            Ok(())
        } else {
            Err(PluginError::NotFound(id.to_owned()))
        }
    }

    /// Build the list response.
    pub fn views(&self, counts: &BTreeMap<String, u64>) -> Value {
        let file = self.file();
        let leases = self.sweep_leases(Instant::now());
        let registry = match self.registry.read() {
            Ok(registry) => registry,
            Err(poisoned) => poisoned.into_inner(),
        };
        let specs = builtins();
        let plugins: Vec<PluginView> = specs
            .iter()
            .map(|spec| {
                let status = registry
                    .statuses
                    .iter()
                    .find(|status| status.id.as_str() == spec.id())
                    .cloned();
                let (ws, leased) = self.in_use_detail(spec.id(), &leases);
                let row = file.row(spec.id());
                PluginView {
                    id: spec.id().to_owned(),
                    name: spec.manifest.name.clone(),
                    sub: spec.manifest.sub.clone(),
                    name_key: spec.manifest.name_key.clone(),
                    version: spec.manifest.version.clone(),
                    api: spec.manifest.api.to_string(),
                    kind: kind_name(spec),
                    description: spec.manifest.description.clone(),
                    state: status
                        .as_ref()
                        .map(|status| status.state.as_str().to_owned())
                        .unwrap_or_else(|| "off".to_owned()),
                    active: registry.active.active(spec.id()),
                    chosen: status.as_ref().is_some_and(|status| status.chosen),
                    source: status
                        .as_ref()
                        .map(|status| status.source.as_str().to_owned())
                        .unwrap_or_else(|| "preset".to_owned()),
                    requires: spec
                        .manifest
                        .requires
                        .plugins
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                    requires_services: spec.manifest.requires.services.clone(),
                    required_by: required_by(spec.id(), specs),
                    in_use: ws.saturating_add(leased),
                    in_use_detail: InUseDetail { ws, leases: leased },
                    data_count: counts.get(spec.id()).copied().unwrap_or(0),
                    usage_dropped: if spec.id() == "records" {
                        self.usage_dropped()
                    } else {
                        None
                    },
                    config: row
                        .and_then(|row| row.config.as_ref())
                        .and_then(|config| serde_json::to_value(config).ok()),
                    schema: spec
                        .manifest
                        .config
                        .as_ref()
                        .and_then(|schema| serde_json::to_value(schema).ok()),
                    permissions: permissions_view(&spec.manifest.permissions),
                    why: status.and_then(|status| status.reason),
                    ui: spec
                        .manifest
                        .ui
                        .iter()
                        .map(|part| UiView {
                            window: part.window.clone(),
                            page: part.page.clone(),
                            titles: part.titles.clone(),
                            entry: part.entry.clone(),
                        })
                        .collect(),
                }
            })
            .collect();
        json!({
            "ok": true,
            "preset": if file.preset == Preset::All { "all" } else { "core" },
            "settled": file.settled,
            "broken": registry.broken,
            "plugins": plugins,
        })
    }

    /// Apply one change after checking every affected plugin.
    ///
    /// `data_count` is called for each plugin that would be disabled; the
    /// caller reads the store.
    pub fn change<F>(
        &self,
        change: Change,
        confirm_data_count: bool,
        data_count: F,
    ) -> Result<(), PluginError>
    where
        F: Fn(&str) -> u64,
    {
        let _guard = self
            .write
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(reason) = self.broken() {
            return Err(PluginError::Broken(reason));
        }
        let mut file = self.file();
        let records_error = self.records_error();
        let explicit_records_enable = matches!(
            &change,
            Change::One { id, on: Some(true), .. } if id == "records"
        );
        let leases = self.sweep_leases(Instant::now());
        let specs = builtins();
        let registry = Registry::resolve(&file, specs);
        let mut conflicts = Vec::new();
        let mut data_counts = Vec::new();

        match &change {
            Change::One { id, on, config } => {
                self.known_plugin(id)?;
                let spec = specs
                    .iter()
                    .find(|spec| spec.id() == id)
                    .ok_or_else(|| PluginError::NotFound(id.clone()))?;
                if let Some(config) = config {
                    validate_plugin_config(spec, config)?;
                }
                if *on == Some(false) && registry.active.active(id) {
                    self.check_disable(id, &leases, &mut conflicts, &mut data_counts, &data_count);
                    let dependents = active_dependents(id, specs, &registry);
                    if !dependents.is_empty() {
                        return Err(PluginError::Dependency {
                            id: id.clone(),
                            dependents,
                        });
                    }
                }
                if *on == Some(true) {
                    for dependency in &spec.manifest.requires.plugins {
                        if !registry.active.active(dependency.as_str())
                            && !specs.iter().any(|known| known.id() == dependency.as_str())
                        {
                            return Err(PluginError::Dependency {
                                id: id.clone(),
                                dependents: vec![dependency.to_string()],
                            });
                        }
                    }
                }
                upsert_row(&mut file, id, *on, config.clone())?;
            }
            Change::Preset(preset) => {
                let next = *preset;
                for spec in specs {
                    if file.has_row(spec.id()) {
                        continue;
                    }
                    let currently = registry.active.active(spec.id());
                    let next_on = next == Preset::All;
                    if currently && !next_on {
                        self.check_disable(
                            spec.id(),
                            &leases,
                            &mut conflicts,
                            &mut data_counts,
                            &data_count,
                        );
                    }
                }
                file.preset = next;
            }
        }

        if !conflicts.is_empty() {
            return Err(PluginError::InUse { conflicts });
        }
        if !data_counts.is_empty() && !confirm_data_count {
            return Err(PluginError::DataCount { ids: data_counts });
        }
        file.version = 1;
        file.settled = printkit_plugin_host::CURRENT_SETTLED;
        let will_records = Registry::resolve(&file, builtins())
            .active
            .active("records");
        let should_prepare = will_records
            && !self.active("records")
            && (records_error.is_none() || explicit_records_enable);
        let pending = if should_prepare {
            self.prepare_records()?
        } else {
            None
        };
        let preserved_error = if will_records && !should_prepare {
            records_error.clone()
        } else {
            None
        };
        save(&self.dir, &file).map_err(|error| PluginError::Io(error.to_string()))?;
        if !will_records || explicit_records_enable {
            self.clear_records_error();
        }
        if let Some((recorder, domain)) = pending {
            if let Ok(mut slot) = self.usage.write() {
                *slot = Some(recorder);
            }
            if let Ok(mut slot) = self.records.write() {
                *slot = Some(domain);
            }
        }
        let mut registry = Registry::resolve(&file, builtins());
        if let Some(reason) = preserved_error {
            registry.fail("records", reason);
        }
        self.observability
            .store(registry.active.active("observability"), Ordering::Relaxed);
        if let Ok(mut slot) = self.file.write() {
            *slot = file;
        }
        if let Ok(mut slot) = self.registry.write() {
            *slot = registry;
        }
        self.sync_records();
        Ok(())
    }

    fn prepare_records(
        &self,
    ) -> Result<Option<(printkit_facade::UsageRecorder, RecordsDomain)>, PluginError> {
        let Some(store) = self.store.as_ref() else {
            return Ok(None);
        };
        if self.fail_records_writer.swap(false, Ordering::SeqCst) {
            let reason = "injected records writer startup failure".to_owned();
            self.mark_records_failed(&reason);
            return Err(PluginError::Io(reason));
        }
        let recorder = match printkit_facade::UsageRecorder::try_from_store(
            Arc::clone(store),
            String::new(),
            self.retention_days,
            self.max_rows,
        ) {
            Ok(recorder) => recorder,
            Err(reason) => {
                self.mark_records_failed(&reason);
                return Err(PluginError::Io(reason));
            }
        };
        let domain = start_records(store, &recorder);
        Ok(Some((recorder, domain)))
    }

    fn mark_records_failed(&self, reason: &str) {
        *self
            .records_error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(reason.to_owned());
        if let Ok(mut registry) = self.registry.write() {
            registry.fail("records", reason.to_owned());
        }
    }

    fn sync_records(&self) {
        if !self.active("records") {
            if let Ok(mut slot) = self.records.write() {
                slot.take();
            }
            let recorder = self.usage.write().ok().and_then(|mut slot| slot.take());
            if let Some(recorder) = recorder {
                let result = recorder.close_and_drain();
                if result.timed_out {
                    eprintln!(
                        "printkit: usage writer timed out; {} row(s) were dropped after close",
                        result.dropped_after_close
                    );
                }
            }
        }
    }

    fn check_disable<F>(
        &self,
        id: &str,
        leases: &HashMap<(String, String), Instant>,
        conflicts: &mut Vec<Conflict>,
        data_counts: &mut Vec<DataCount>,
        data_count: &F,
    ) where
        F: Fn(&str) -> u64,
    {
        let (ws, leased) = self.in_use_detail(id, leases);
        if ws.saturating_add(leased) > 0 {
            conflicts.push(Conflict {
                id: id.to_owned(),
                in_use: ws.saturating_add(leased),
                detail: InUseDetail { ws, leases: leased },
            });
        }
        let count = data_count(id);
        if count > 0 {
            data_counts.push(DataCount {
                id: id.to_owned(),
                data_count: count,
            });
        }
    }
}

/// One applied change.
#[derive(Debug, Clone)]
pub enum Change {
    /// Enable or disable one plugin, and optionally replace its config.
    One {
        /// Plugin id.
        id: String,
        /// `None` leaves the current choice untouched.
        on: Option<bool>,
        /// Full replacement configuration.
        config: Option<toml::Value>,
    },
    /// Apply a preset to every plugin without an explicit row.
    Preset(Preset),
}

/// A failed plugin operation.
#[derive(Debug, Clone)]
pub enum PluginError {
    /// The id is not a built-in plugin.
    NotFound(String),
    /// The file is broken; writes are refused.
    Broken(String),
    /// Live sessions or leases block a disable.
    InUse {
        /// One entry per blocked plugin.
        conflicts: Vec<Conflict>,
    },
    /// Historical rows need a confirmation.
    DataCount {
        /// One entry per plugin with data.
        ids: Vec<DataCount>,
    },
    /// Another active plugin requires this one.
    Dependency {
        /// Plugin that cannot change.
        id: String,
        /// Active plugins that depend on it.
        dependents: Vec<String>,
    },
    /// Configuration does not match the schema.
    Schema(String),
    /// Writing the file failed.
    Io(String),
}

impl PluginError {
    /// Protocol error code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "E_NOT_FOUND",
            Self::Broken(_) => "E_CONFIG",
            Self::InUse { .. } | Self::DataCount { .. } | Self::Dependency { .. } => {
                "E_UNSUPPORTED"
            }
            Self::Schema(_) => "E_SCHEMA",
            Self::Io(_) => "E_JOB_FAILED",
        }
    }

    /// HTTP status.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Schema(_) => StatusCode::BAD_REQUEST,
            Self::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::CONFLICT,
        }
    }

    /// Structured detail for the error envelope.
    pub fn detail(&self) -> Value {
        match self {
            Self::NotFound(id) => json!({ "plugin": id }),
            Self::Broken(reason) => json!({ "reason": "broken", "message": reason }),
            Self::InUse { conflicts } => {
                json!({ "reason": "in_use", "conflicts": conflicts })
            }
            Self::DataCount { ids } => json!({ "reason": "data_count", "ids": ids }),
            Self::Dependency { id, dependents } => {
                json!({ "reason": "dependency", "plugin": id, "dependents": dependents })
            }
            Self::Schema(message) => json!({ "reason": "schema", "message": message }),
            Self::Io(message) => json!({ "reason": "io", "message": message }),
        }
    }

    /// Human-readable message.
    pub fn message(&self) -> String {
        match self {
            Self::NotFound(id) => format!("unknown plugin: {id}"),
            Self::Broken(reason) => format!("plugins.toml is broken: {reason}"),
            Self::InUse { conflicts } => {
                format!("{} plugin(s) still have live sessions", conflicts.len())
            }
            Self::DataCount { ids } => {
                format!("{} plugin(s) have historical data", ids.len())
            }
            Self::Dependency { id, dependents } => {
                format!("{id} is required by {}", dependents.join(", "))
            }
            Self::Schema(message) => message.clone(),
            Self::Io(message) => message.clone(),
        }
    }
}

/// One blocked plugin.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Conflict {
    /// Plugin id.
    pub id: String,
    /// Total live sessions.
    pub in_use: u64,
    /// Breakdown.
    pub detail: InUseDetail,
}

/// One plugin with historical data.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataCount {
    /// Plugin id.
    pub id: String,
    /// Rows owned by the plugin.
    pub data_count: u64,
}

/// Live session breakdown.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct InUseDetail {
    /// WebSocket trace subscriptions.
    pub ws: u64,
    /// Desktop leases.
    pub leases: u64,
}

/// One plugin in the administration response.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginView {
    /// Plugin id.
    pub id: String,
    /// English display name.
    pub name: String,
    /// One-line description.
    pub sub: String,
    /// Translation key for the name.
    pub name_key: Option<String>,
    /// Display version.
    pub version: String,
    /// Declared API range.
    pub api: String,
    /// `builtin` or `external`.
    pub kind: &'static str,
    /// Long description.
    pub description: String,
    /// `on`, `off`, `pending`, `incompatible`, `invalid` or `broken`.
    pub state: String,
    /// Whether the plugin is active right now.
    pub active: bool,
    /// The user's or preset's choice before dependencies.
    pub chosen: bool,
    /// `preset` or `user`.
    pub source: String,
    /// Plugin ids this one requires.
    pub requires: Vec<String>,
    /// Host services this one requires.
    pub requires_services: Vec<String>,
    /// Active plugins that require this one.
    pub required_by: Vec<String>,
    /// Active sessions plus leases.
    pub in_use: u64,
    /// Breakdown.
    pub in_use_detail: InUseDetail,
    /// Rows owned by the plugin.
    pub data_count: u64,
    /// Usage writer counters for the records plugin.
    pub usage_dropped: Option<UsageDroppedView>,
    /// User configuration, when present.
    pub config: Option<Value>,
    /// Configuration schema, when the plugin has one.
    pub schema: Option<Value>,
    /// Declared permissions, with bridge keys in wire camelCase.
    pub permissions: Value,
    /// Why the state is not `on`.
    pub why: Option<String>,
    /// UI contributions.
    pub ui: Vec<UiView>,
}

/// Usage writer counters shown for the records plugin.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageDroppedView {
    /// Rows offered after the writer closed.
    pub dropped_after_close: u64,
    /// Rows lost because the bounded queue was full or the writer died.
    pub dropped_full: u64,
}

/// One UI contribution in the list response.
#[derive(Debug, Clone, Serialize)]
pub struct UiView {
    /// Window label.
    pub window: String,
    /// Page id.
    pub page: String,
    /// Localized titles.
    pub titles: BTreeMap<String, String>,
    /// External entry path.
    pub entry: Option<String>,
}

fn kind_name(spec: &BuiltinSpec) -> &'static str {
    match spec.manifest.kind {
        printkit_plugin_api::Kind::Builtin => "builtin",
        printkit_plugin_api::Kind::External => "external",
    }
}

fn permissions_view(permissions: &printkit_plugin_api::Permissions) -> Value {
    let filesystem = match permissions.filesystem {
        printkit_plugin_api::FileAccess::None => "none",
        printkit_plugin_api::FileAccess::PluginData => "pluginData",
    };
    json!({
        "filesystem": filesystem,
        "network": permissions.network,
        "process": permissions.process,
        "printer": permissions.printer,
        "raw": permissions.raw,
        "bridge": {
            "selfConfig": permissions.bridge.self_config,
            "printers": permissions.bridge.printers,
            "recordsSummary": permissions.bridge.records_summary,
        }
    })
}

fn required_by(id: &str, specs: &[BuiltinSpec]) -> Vec<String> {
    specs
        .iter()
        .filter(|spec| {
            spec.manifest
                .requires
                .plugins
                .iter()
                .any(|dependency| dependency.as_str() == id)
        })
        .map(|spec| spec.id().to_owned())
        .collect()
}

fn active_dependents(id: &str, specs: &[BuiltinSpec], registry: &Registry) -> Vec<String> {
    specs
        .iter()
        .filter(|spec| {
            registry.active.active(spec.id())
                && spec
                    .manifest
                    .requires
                    .plugins
                    .iter()
                    .any(|dependency| dependency.as_str() == id)
        })
        .map(|spec| spec.id().to_owned())
        .collect()
}

fn validate_plugin_config(spec: &BuiltinSpec, config: &toml::Value) -> Result<(), PluginError> {
    let value = serde_json::to_value(config)
        .map_err(|error| PluginError::Schema(format!("cannot encode config: {error}")))?;
    match &spec.manifest.config {
        Some(schema) => {
            validate_config(schema, &value).map_err(|error| PluginError::Schema(error.to_string()))
        }
        None => {
            if value.as_object().is_some_and(|object| object.is_empty()) {
                Ok(())
            } else {
                Err(PluginError::Schema(format!(
                    "plugin {} has no configuration schema",
                    spec.id()
                )))
            }
        }
    }
}

fn upsert_row(
    file: &mut PluginsFile,
    id: &str,
    on: Option<bool>,
    config: Option<toml::Value>,
) -> Result<(), PluginError> {
    let plugin_id = PluginId::new(id)
        .map_err(|error| PluginError::Schema(format!("invalid plugin id: {error}")))?;
    if let Some(row) = file.plugins.iter_mut().find(|row| row.id.as_str() == id) {
        if let Some(on) = on {
            row.enabled = on;
        }
        if config.is_some() {
            row.config = config;
        }
        return Ok(());
    }
    if on.is_none() && config.is_none() {
        return Ok(());
    }
    file.plugins.push(PluginRow {
        id: plugin_id,
        enabled: on.unwrap_or_else(|| file.preset.enables_without_row()),
        version: None,
        source: None,
        integrity: None,
        config,
    });
    Ok(())
}

/// Build the records domain through the host service registry.
fn start_records(store: &Arc<Store>, recorder: &printkit_facade::UsageRecorder) -> RecordsDomain {
    let record_store: Arc<dyn printkit_plugin_api::RecordStore> =
        Arc::new(super::records::StoreRecords::new(Arc::clone(store)));
    let usage: Arc<dyn printkit_plugin_api::UsageSink> =
        Arc::new(super::records::StoreUsage::new(recorder.clone()));
    let registry = ServiceRegistry::new()
        .with_record_store(record_store)
        .with_usage(usage);
    let services = registry
        .resolve(&[RECORD_STORE.to_owned(), RECORDS_USAGE.to_owned()])
        .expect("the host installed both records services");
    printkit_plugin_records::bind(
        services.record_store().expect("resolved record store"),
        services.usage_sink().expect("resolved usage sink"),
    )
}

/// Count rows in the table a builtin plugin owns.
pub fn data_count(store: &Store, id: &str) -> u64 {
    let count = match id {
        "records" => store
            .print_record_count()
            .ok()
            .map(|count| u64::try_from(count.count).unwrap_or(0)),
        _ => Some(0),
    };
    count.unwrap_or(0)
}

/// All data counts in built-in order.
pub fn data_counts(store: &Store) -> BTreeMap<String, u64> {
    builtins()
        .iter()
        .map(|spec| (spec.id().to_owned(), data_count(store, spec.id())))
        .collect()
}

/// A sink that drops every event while its plugin is off.
pub struct GatedSink {
    inner: Arc<dyn printkit_facade::Sink>,
    gate: Arc<AtomicBool>,
}

impl GatedSink {
    /// Wrap the real sink.
    pub fn new(inner: Arc<dyn printkit_facade::Sink>, gate: Arc<AtomicBool>) -> Self {
        Self { inner, gate }
    }
}

impl printkit_facade::Sink for GatedSink {
    fn emit(&self, event: &printkit_facade::TraceEvent) {
        if self.gate.load(Ordering::Relaxed) {
            self.inner.emit(event);
        } else {
            self.inner.note_dropped();
        }
    }

    fn note_dropped(&self) {
        self.inner.note_dropped();
    }

    fn note_dropped_field(&self) {
        self.inner.note_dropped_field();
    }
}

/// Plugin that owns an HTTP route, if any.
pub fn route_owner(method: &str, path: &str) -> Option<&'static str> {
    let path = super::gate_path(path)?;
    builtins().iter().find_map(|spec| {
        spec.admin_routes
            .iter()
            .find(|(route_method, pattern)| {
                route_method.eq_ignore_ascii_case(method) && route_matches(pattern, &path)
            })
            .map(|_| spec.id())
    })
}

fn route_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim_start_matches('/').trim_end_matches('/');
    let path = path.trim_start_matches('/').trim_end_matches('/');
    match pattern.split_once('{') {
        Some((prefix, _)) => path.starts_with(prefix.trim_end_matches('/')),
        None => pattern == path,
    }
}

/// Gate owned routes by plugin state.
///
/// An authenticated caller learns that the plugin is disabled; an
/// unauthenticated one sees the ordinary not-found body, so the route set is
/// not a probe oracle.
#[handler]
pub async fn plugin_gate(
    req: &mut Request,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
    depot: &mut Depot,
) {
    let Some(state) = depot.obtain::<Arc<AppState>>().ok().cloned() else {
        return;
    };
    let method = req.method().as_str();
    let raw_path = req.uri().path();
    let Some(plugin) = route_owner(method, raw_path) else {
        if super::gate_path(raw_path).is_none() {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(Json(json!({
                "ok": false,
                "error": { "code": "E_NOT_FOUND", "message": "Not Found" }
            })));
            ctrl.skip_rest();
        }
        return;
    };
    if state.plugins.active(plugin) {
        return;
    }
    res.status_code(StatusCode::NOT_FOUND);
    if admin::admin_session(&state, req) {
        res.render(Json(json!({
            "ok": false,
            "error": {
                "code": "E_UNSUPPORTED",
                "message": format!("plugin {plugin} is disabled"),
                "detail": { "plugin": plugin, "enable": format!("printkit plugin enable {plugin}") },
            }
        })));
    } else {
        res.render(Json(json!({
            "ok": false,
            "error": { "code": "E_NOT_FOUND", "message": "Not Found" }
        })));
    }
    ctrl.skip_rest();
}

fn obtain(depot: &Depot) -> Option<Arc<AppState>> {
    depot.obtain::<Arc<AppState>>().ok().cloned()
}

fn ok(res: &mut Response, body: Value) {
    res.status_code(StatusCode::OK);
    res.render(Json(body));
}

fn fail(res: &mut Response, status: StatusCode, code: &str, message: &str, detail: Value) {
    res.status_code(status);
    res.render(Json(json!({
        "ok": false,
        "error": { "code": code, "message": message, "detail": detail }
    })));
}

fn deny(res: &mut Response, denial: WriteDenied) {
    let status = match denial {
        WriteDenied::Session => StatusCode::UNAUTHORIZED,
        WriteDenied::Origin | WriteDenied::Csrf => StatusCode::FORBIDDEN,
    };
    let code = match denial {
        WriteDenied::Session => "E_AUTH",
        WriteDenied::Origin => "E_ORIGIN",
        WriteDenied::Csrf => "E_CSRF",
    };
    fail(
        res,
        status,
        code,
        "the administrator request was refused",
        Value::Null,
    );
}

#[handler]
pub async fn list(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = obtain(depot) else {
        return fail(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_JOB_FAILED",
            "service state is missing",
            Value::Null,
        );
    };
    if !admin::admin_session(&state, req) {
        return fail(
            res,
            StatusCode::UNAUTHORIZED,
            "E_AUTH",
            "an administrator session is required",
            Value::Null,
        );
    }
    let store = Arc::clone(&state.store);
    let counts = match tokio::task::spawn_blocking(move || data_counts(&store)).await {
        Ok(counts) => counts,
        Err(error) => {
            return fail(
                res,
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_JOB_FAILED",
                &format!("cannot count plugin data: {error}"),
                Value::Null,
            );
        }
    };
    ok(res, state.plugins.views(&counts));
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    preset: Option<Preset>,
    #[serde(default, alias = "enabled")]
    on: Option<bool>,
    #[serde(default)]
    config: Option<Value>,
    #[serde(default)]
    confirm_data_count: bool,
    /// Accepted for UI compatibility; `inUse` is always a hard rejection.
    #[serde(default)]
    confirm_in_use: bool,
}

#[handler]
pub async fn set(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = obtain(depot) else {
        return fail(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_JOB_FAILED",
            "service state is missing",
            Value::Null,
        );
    };
    if let Err(denial) = admin::check_write(&state, req, true) {
        return deny(res, denial);
    }
    let body: SetBody = match req.parse_json().await {
        Ok(body) => body,
        Err(error) => {
            return fail(
                res,
                StatusCode::BAD_REQUEST,
                "E_SCHEMA",
                &format!("invalid plugin body: {error}"),
                Value::Null,
            );
        }
    };
    let _confirm_in_use = body.confirm_in_use;
    let change = match (body.id, body.preset) {
        (Some(id), None) => {
            let config = match body.config {
                Some(value) => match toml::Value::try_from(value) {
                    Ok(value) => Some(value),
                    Err(error) => {
                        return fail(
                            res,
                            StatusCode::BAD_REQUEST,
                            "E_SCHEMA",
                            &format!("invalid plugin config: {error}"),
                            Value::Null,
                        );
                    }
                },
                None => None,
            };
            Change::One {
                id,
                on: body.on,
                config,
            }
        }
        (None, Some(preset)) if body.on.is_none() && body.config.is_none() => {
            Change::Preset(preset)
        }
        _ => {
            return fail(
                res,
                StatusCode::BAD_REQUEST,
                "E_SCHEMA",
                "send either {id,on,config} or {preset}",
                Value::Null,
            );
        }
    };
    let store = Arc::clone(&state.store);
    let counts = match tokio::task::spawn_blocking(move || data_counts(&store)).await {
        Ok(counts) => counts,
        Err(error) => {
            return fail(
                res,
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_JOB_FAILED",
                &format!("cannot count plugin data: {error}"),
                Value::Null,
            );
        }
    };
    if let Err(error) = state.plugins.change(change, body.confirm_data_count, |id| {
        counts.get(id).copied().unwrap_or(0)
    }) {
        return fail(
            res,
            error.status(),
            error.code(),
            &error.message(),
            error.detail(),
        );
    }
    let store = Arc::clone(&state.store);
    let counts: BTreeMap<String, u64> = tokio::task::spawn_blocking(move || data_counts(&store))
        .await
        .unwrap_or_default();
    ok(res, state.plugins.views(&counts));
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LeaseBody {
    id: String,
    op: String,
    lease_id: String,
}

#[handler]
pub async fn lease(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = obtain(depot) else {
        return fail(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_JOB_FAILED",
            "service state is missing",
            Value::Null,
        );
    };
    if let Err(denial) = admin::check_write(&state, req, true) {
        return deny(res, denial);
    }
    let body: LeaseBody = match req.parse_json().await {
        Ok(body) => body,
        Err(error) => {
            return fail(
                res,
                StatusCode::BAD_REQUEST,
                "E_SCHEMA",
                &format!("invalid lease body: {error}"),
                Value::Null,
            );
        }
    };
    match state.plugins.lease(&body.id, &body.op, &body.lease_id) {
        Ok(in_use) => ok(res, json!({ "ok": true, "inUse": in_use })),
        Err(error) => fail(
            res,
            error.status(),
            error.code(),
            &error.message(),
            error.detail(),
        ),
    }
}

/// The periodic lease sweep.
pub fn spawn_lease_sweep(runtime: &tokio::runtime::Runtime, state: Arc<AppState>) {
    runtime.spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            state.plugins.sweep();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(dir: &Path) -> PluginsState {
        PluginsState::load_only(dir)
    }

    #[test]
    fn route_ownership_matches_the_router_shapes() {
        assert_eq!(
            route_owner("GET", "/admin/logs/events"),
            Some("observability")
        );
        assert_eq!(route_owner("GET", "/records/list"), Some("records"));
        assert_eq!(route_owner("GET", "/admin/plugins"), None);
        assert_eq!(route_owner("POST", "/admin/plugin-lease"), None);
    }

    #[test]
    fn a_live_subscription_or_lease_blocks_disable() {
        let temp = tempfile::tempdir().unwrap();
        let plugins = state(temp.path());
        plugins
            .change(
                Change::One {
                    id: "observability".to_owned(),
                    on: Some(true),
                    config: None,
                },
                true,
                |_| 0,
            )
            .unwrap();
        plugins.subscribe_begin();
        let error = plugins
            .change(
                Change::One {
                    id: "observability".to_owned(),
                    on: Some(false),
                    config: None,
                },
                true,
                |_| 0,
            )
            .unwrap_err();
        assert!(matches!(error, PluginError::InUse { .. }), "{error:?}");
        plugins.subscribe_end();

        plugins.lease("observability", "acquire", "desk").unwrap();
        assert_eq!(plugins.in_use("observability"), 1);
        let error = plugins
            .change(
                Change::One {
                    id: "observability".to_owned(),
                    on: Some(false),
                    config: None,
                },
                true,
                |_| 0,
            )
            .unwrap_err();
        assert!(matches!(error, PluginError::InUse { .. }), "{error:?}");
        plugins.lease("observability", "release", "desk").unwrap();
        plugins
            .change(
                Change::One {
                    id: "observability".to_owned(),
                    on: Some(false),
                    config: None,
                },
                true,
                |_| 0,
            )
            .unwrap();
        assert!(!plugins.active("observability"));
    }

    #[test]
    fn data_count_requires_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let plugins = state(temp.path());
        plugins
            .change(
                Change::One {
                    id: "observability".to_owned(),
                    on: Some(true),
                    config: None,
                },
                true,
                |_| 0,
            )
            .unwrap();
        let error = plugins
            .change(
                Change::One {
                    id: "observability".to_owned(),
                    on: Some(false),
                    config: None,
                },
                false,
                |_| 3,
            )
            .unwrap_err();
        assert!(matches!(error, PluginError::DataCount { .. }), "{error:?}");
        plugins
            .change(
                Change::One {
                    id: "observability".to_owned(),
                    on: Some(false),
                    config: None,
                },
                true,
                |_| 3,
            )
            .unwrap();
    }

    #[test]
    fn preset_core_off_disables_rowless_plugins() {
        let temp = tempfile::tempdir().unwrap();
        let plugins = state(temp.path());
        plugins
            .change(Change::Preset(Preset::Core), true, |_| 0)
            .unwrap();
        assert!(!plugins.active("records"));
        assert!(!plugins.active("observability"));
    }
    fn write_one(recorder: &printkit_facade::UsageRecorder, trace: &str, tags: &[String]) {
        let at = "2026-11-13T00:00:00Z";
        let record = printkit_store::NewPrintRecord {
            trace_id: trace,
            seq: 1,
            at,
            origin: "https://plugins.test",
            template: None,
            tags,
            printer: None,
            pages: 1,
            copies: 1,
            status: "done",
            dur_ms: 0,
            error_code: None,
        };
        recorder.record(&record);
    }

    fn row_count(store: &Store) -> u64 {
        store
            .print_record_count()
            .map(|count| u64::try_from(count.count).unwrap_or(0))
            .unwrap_or(0)
    }

    #[test]
    fn a_startup_writer_failure_leaves_records_invalid_and_inactive() {
        let temp = tempfile::tempdir().unwrap();
        let plugins = PluginsState::all_for_tests_with_writer_failure(temp.path());
        assert!(!plugins.active("records"));
        assert!(plugins.usage_recorder("https://plugins.test").is_none());
        assert!(plugins.records_domain().is_none());
        let views = plugins.views(&BTreeMap::new());
        let records = views["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|plugin| plugin["id"] == "records")
            .unwrap();
        assert_eq!(records["state"], "invalid");
        assert_eq!(records["active"], false);
        assert!(
            records["why"]
                .as_str()
                .unwrap_or_default()
                .contains("records writer"),
            "{records}"
        );
        assert!(plugins.file().chosen("records"));
        plugins
            .change(
                Change::One {
                    id: "records".to_owned(),
                    on: Some(true),
                    config: None,
                },
                true,
                |_| 0,
            )
            .unwrap();
        assert!(plugins.active("records"));
        assert!(plugins.usage_recorder("https://plugins.test").is_some());
    }

    #[test]
    fn an_unrelated_change_does_not_retry_failed_records() {
        let temp = tempfile::tempdir().unwrap();
        let plugins = PluginsState::all_for_tests_with_writer_failure(temp.path());
        assert!(!plugins.active("records"));
        plugins
            .change(
                Change::One {
                    id: "observability".to_owned(),
                    on: Some(false),
                    config: None,
                },
                true,
                |_| 0,
            )
            .unwrap();
        assert!(!plugins.active("observability"));
        assert!(!plugins.active("records"));
        assert!(plugins.usage_recorder("https://plugins.test").is_none());
        assert!(plugins.records_domain().is_none());
        assert!(plugins.file().chosen("records"));
        let views = plugins.views(&BTreeMap::new());
        let records = views["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|plugin| plugin["id"] == "records")
            .unwrap();
        assert_eq!(records["state"], "invalid");
        assert!(
            records["why"]
                .as_str()
                .unwrap_or_default()
                .contains("records writer"),
            "{records}"
        );
        plugins
            .change(
                Change::One {
                    id: "records".to_owned(),
                    on: Some(true),
                    config: None,
                },
                true,
                |_| 0,
            )
            .unwrap();
        assert!(plugins.active("records"));
        assert!(plugins.usage_recorder("https://plugins.test").is_some());
        assert!(plugins.records_domain().is_some());
    }

    #[test]
    fn a_failed_enable_keeps_records_off_and_marks_the_reason() {
        let temp = tempfile::tempdir().unwrap();
        let plugins = PluginsState::all_for_tests(temp.path());
        plugins
            .change(
                Change::One {
                    id: "records".to_owned(),
                    on: Some(false),
                    config: None,
                },
                true,
                |_| 0,
            )
            .unwrap();
        plugins.fail_records_writer.store(true, Ordering::SeqCst);
        let error = plugins
            .change(
                Change::One {
                    id: "records".to_owned(),
                    on: Some(true),
                    config: None,
                },
                true,
                |_| 0,
            )
            .unwrap_err();
        assert!(matches!(error, PluginError::Io(_)), "{error:?}");
        assert!(!plugins.active("records"));
        assert!(plugins.usage_recorder("https://plugins.test").is_none());
        assert!(plugins.records_domain().is_none());
        assert!(!plugins.file().chosen("records"));
        let views = plugins.views(&BTreeMap::new());
        let records = views["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|plugin| plugin["id"] == "records")
            .unwrap();
        assert_eq!(records["state"], "invalid");
        assert_eq!(records["active"], false);
        assert!(
            records["why"]
                .as_str()
                .unwrap_or_default()
                .contains("records writer"),
            "{records}"
        );
        plugins
            .change(
                Change::One {
                    id: "records".to_owned(),
                    on: Some(true),
                    config: None,
                },
                true,
                |_| 0,
            )
            .unwrap();
        assert!(plugins.active("records"));
        assert!(plugins.usage_recorder("https://plugins.test").is_some());
    }

    #[test]
    fn a_409_keeps_the_records_writer_open() {
        let temp = tempfile::tempdir().unwrap();
        let plugins = PluginsState::all_for_tests(temp.path());
        plugins.lease("records", "acquire", "desk").unwrap();
        let error = plugins
            .change(
                Change::One {
                    id: "records".to_owned(),
                    on: Some(false),
                    config: None,
                },
                true,
                |_| 0,
            )
            .unwrap_err();
        assert!(matches!(error, PluginError::InUse { .. }), "{error:?}");
        assert!(plugins.active("records"));
        let recorder = plugins.usage_recorder("https://plugins.test").unwrap();
        let tags = Vec::new();
        write_one(&recorder, "before-409", &tags);
        let store = recorder.store();
        for _ in 0..100 {
            if row_count(&store) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(row_count(&store), 1);
        let views = plugins.views(&BTreeMap::new());
        let records = views["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|plugin| plugin["id"] == "records")
            .unwrap();
        assert_eq!(
            records["requiresServices"],
            serde_json::json!(["records.store", "records.usage"])
        );
        assert!(records["usageDropped"]["droppedAfterClose"].is_number());
        plugins.lease("records", "release", "desk").unwrap();
    }

    #[test]
    fn a_save_failure_keeps_the_records_writer_open() {
        let temp = tempfile::tempdir().unwrap();
        let plugins = PluginsState::all_for_tests(temp.path());
        std::fs::create_dir(temp.path().join("plugins.toml.tmp")).unwrap();
        let error = plugins
            .change(
                Change::One {
                    id: "records".to_owned(),
                    on: Some(false),
                    config: None,
                },
                true,
                |_| 0,
            )
            .unwrap_err();
        assert!(matches!(error, PluginError::Io(_)), "{error:?}");
        assert!(plugins.active("records"));
        let recorder = plugins.usage_recorder("https://plugins.test").unwrap();
        let tags = Vec::new();
        write_one(&recorder, "after-save-failure", &tags);
        let store = recorder.store();
        for _ in 0..100 {
            if row_count(&store) == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(row_count(&store), 1);
    }

    #[test]
    fn disabling_records_closes_the_writer_and_re_enabling_creates_a_new_one() {
        let temp = tempfile::tempdir().unwrap();
        let plugins = PluginsState::all_for_tests(temp.path());
        let old = plugins.usage_recorder("https://plugins.test").unwrap();
        plugins
            .change(
                Change::One {
                    id: "records".to_owned(),
                    on: Some(false),
                    config: None,
                },
                true,
                |_| 0,
            )
            .unwrap();
        assert!(!plugins.active("records"));
        assert!(plugins.usage_recorder("https://plugins.test").is_none());
        let tags = Vec::new();
        write_one(&old, "after-close", &tags);
        assert!(old.dropped_after_close() >= 1);
        plugins
            .change(
                Change::One {
                    id: "records".to_owned(),
                    on: Some(true),
                    config: None,
                },
                true,
                |_| 0,
            )
            .unwrap();
        assert!(plugins.active("records"));
        let fresh = plugins.usage_recorder("https://plugins.test").unwrap();
        write_one(&fresh, "after-enable", &tags);
        let store = fresh.store();
        for _ in 0..100 {
            if row_count(&store) >= 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(row_count(&store) >= 1);
    }

    #[test]
    fn broken_state_refuses_every_write() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("plugins.toml"), "version = 2").unwrap();
        let plugins = state(temp.path());
        let error = plugins
            .change(
                Change::One {
                    id: "observability".to_owned(),
                    on: Some(true),
                    config: None,
                },
                true,
                |_| 0,
            )
            .unwrap_err();
        assert!(matches!(error, PluginError::Broken(_)), "{error:?}");
        assert!(!plugins.active("observability"));
    }
}
