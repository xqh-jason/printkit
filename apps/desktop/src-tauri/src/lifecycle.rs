//! Service lifecycle for the desktop shell.
//! Plugin state is cached here, never probed from `snapshot`: a synchronous
//! IPC call reads [`PluginCache`] only, so five windows do not each hit the
//! loopback API. The cache is dropped when the service instance changes, and
//! a failed refresh turns the last known state into `unknown` instead of
//! keeping a stale "on".
//!
//! The only source of truth for "is serve running" is `<data_dir>/serve.lock`.
//! The shell owns a process only when the lock pid equals the pid it spawned;
//! anything else is an external service attached read-only.
//!
//! Stopping an owned service goes through `POST /admin/shutdown` from the
//! Rust control client (the admin token never enters the WebView), so the
//! service drains and removes its own lock. The
//! graceful request has a short budget; if it does not exit within
//! [`STOP_TIMEOUT`] + [`STOP_EXIT_WAIT`], the shell kills the process. A killed
//! process cannot run its destructor, so an unowned stale lock is left for the
//! next `printkit serve`, which reclaims it after checking the pid.

use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::Mutex as AsyncMutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_shell::ShellExt;
use tauri_plugin_shell::process::{CommandChild, CommandEvent};

pub const LOCK_FILE: &str = "serve.lock";
const SIDECAR: &str = "printkit";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const START_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// Time allowed for a service that acknowledged shutdown to exit.
const STOP_EXIT_WAIT: Duration = Duration::from_secs(1);
/// Time allowed for a killed child to disappear.
const KILL_WAIT: Duration = Duration::from_secs(2);
/// `version` must answer quickly; `config` may read a large file.
const SIDECAR_VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const SIDECAR_CONFIG_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SidecarInfo {
    pub version: String,
    pub target: String,
    pub compatible: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigView {
    pub data_dir: String,
    pub port: u16,
    pub json: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ServiceState {
    Stopped,
    RunningOwned,
    RunningExternal,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    pub state: ServiceState,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub reachable: bool,
    pub lock_path: String,
    pub stale_lock: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AutoStartState {
    Idle,
    Attempting,
    Started,
    Failed,
    SkippedExternal,
    SkippedIncompatible,
    SkippedConfig,
    Disabled,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoStartStatus {
    pub state: AutoStartState,
    pub error: Option<String>,
}

impl Default for AutoStartStatus {
    fn default() -> Self {
        Self {
            state: AutoStartState::Idle,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginView {
    /// One-line description supplied by the backend.
    #[serde(default)]
    pub sub: String,
    #[serde(default)]
    pub description: String,
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub name_key: Option<String>,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub kind: String,
    /// Backend's boolean for "active right now" (`state === "on"`).
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub chosen: bool,
    /// The backend calls this `why`; normalize to `reason` for the UI.
    #[serde(default, alias = "why")]
    pub reason: Option<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub required_by: Vec<String>,
    #[serde(default)]
    pub in_use: u64,
    #[serde(default)]
    pub in_use_detail: Option<serde_json::Value>,
    #[serde(default)]
    pub data_count: Option<u64>,
    #[serde(default)]
    pub capabilities: Option<serde_json::Value>,
    #[serde(default)]
    pub ui: Option<serde_json::Value>,
    #[serde(default)]
    pub permissions: Option<serde_json::Value>,
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    #[serde(default)]
    pub schema: Option<serde_json::Value>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginAvailability {
    On,
    Off,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginsChanged {
    pub instance: String,
    pub plugins: Option<Vec<PluginView>>,
    pub preset: Option<String>,
    pub broken: Option<String>,
    pub stale: bool,
    pub reachable: bool,
    pub error: Option<String>,
    pub at: u64,
}

#[derive(Debug, Clone, Default)]
struct PluginCache {
    instance: String,
    fetched_at: Option<Instant>,
    plugins: Option<Vec<PluginView>>,
    preset: Option<String>,
    broken: Option<String>,
    error: Option<String>,
    stale: bool,
    reachable: bool,
}

impl PluginCache {
    #[cfg(test)]
    fn availability(&self, id: &str) -> PluginAvailability {
        if self.stale || !self.reachable {
            return PluginAvailability::Unknown;
        }
        let Some(plugins) = &self.plugins else {
            return PluginAvailability::Unknown;
        };
        match plugins.iter().find(|plugin| plugin.id == id) {
            Some(plugin) if plugin.state == "on" => PluginAvailability::On,
            Some(_) => PluginAvailability::Off,
            None => PluginAvailability::Unknown,
        }
    }
}

pub fn start_attempt() -> AutoStartStatus {
    AutoStartStatus {
        state: AutoStartState::Attempting,
        error: None,
    }
}

pub fn start_succeeded() -> AutoStartStatus {
    AutoStartStatus {
        state: AutoStartState::Started,
        error: None,
    }
}

pub fn start_failed(error: impl Into<String>) -> AutoStartStatus {
    AutoStartStatus {
        state: AutoStartState::Failed,
        error: Some(error.into()),
    }
}

pub fn stopped_by_user() -> AutoStartStatus {
    AutoStartStatus {
        state: AutoStartState::Disabled,
        error: None,
    }
}

pub fn retry_status(current: &AutoStartStatus) -> AutoStartStatus {
    if current.state == AutoStartState::Failed {
        start_attempt()
    } else {
        current.clone()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub sidecar: SidecarInfo,
    pub config: Option<ConfigView>,
    pub config_error: Option<String>,
    pub status: ServiceStatus,
    pub auto_start: AutoStartStatus,
    /// `None` means unknown (never fetched, service stopped, or no plugin API
    /// in this build). `stale` keeps the last known list for the plugin page,
    /// but every business/nav consumer must treat it as unknown.
    pub plugins: Option<Vec<PluginView>>,
    pub plugins_error: Option<String>,
    pub plugins_stale: bool,
    pub plugins_reachable: bool,
    pub plugins_preset: Option<String>,
    pub plugins_broken: Option<String>,
    /// Absolute path of the bundled CLI, used by recovery commands in the
    /// business window when `printkit` is not on PATH.
    pub cli_path: Option<String>,
}

pub struct ManagedChild {
    pid: u32,
    /// Per-run id read from this child's `serve.lock`; `None` until the lock
    /// appears. A later pid match without this instance is not our service.
    instance: Option<String>,
    child: CommandChild,
}

pub struct DesktopState {
    sidecar: Mutex<SidecarInfo>,
    config: Mutex<Option<ConfigView>>,
    config_error: Mutex<Option<String>>,
    probe_done: AtomicBool,
    child: Mutex<Option<ManagedChild>>,
    /// Pid the shell is stopping on purpose. Its exit is not a failure and
    /// must not raise a notice in the UI.
    expected_exit: Mutex<Option<u32>>,
    output: Mutex<Vec<String>>,
    /// One start/stop transition at a time, so double clicks cannot spawn two
    /// sidecars before the first lock is visible.
    start_gate: AsyncMutex<()>,
    auto_start: Mutex<AutoStartStatus>,
    auto_start_attempted: AtomicBool,
    control: Mutex<Option<(u16, Arc<crate::control::ControlClient>)>>,
    pub quitting: AtomicBool,
    /// Quit without stopping the owned sidecar (tray "keep service").
    pub keep_service: AtomicBool,
    plugins: Mutex<PluginCache>,
    /// Serialises `/admin/plugins` fetches from the event listener, the 2 s
    /// loop and the business page's manual refresh.
    plugin_fetch_gate: AsyncMutex<()>,
}

impl DesktopState {
    pub fn new() -> Self {
        Self {
            sidecar: Mutex::new(SidecarInfo {
                version: String::new(),
                target: String::new(),
                compatible: false,
                message: "The printkit sidecar is being checked.".to_owned(),
            }),
            config: Mutex::new(None),
            config_error: Mutex::new(Some("The printkit sidecar is being checked.".to_owned())),
            probe_done: AtomicBool::new(false),
            child: Mutex::new(None),
            expected_exit: Mutex::new(None),
            output: Mutex::new(Vec::new()),
            auto_start: Mutex::new(AutoStartStatus::default()),
            start_gate: AsyncMutex::new(()),
            auto_start_attempted: AtomicBool::new(false),
            control: Mutex::new(None),
            quitting: AtomicBool::new(false),
            keep_service: AtomicBool::new(false),
            plugins: Mutex::new(PluginCache::default()),
            plugin_fetch_gate: AsyncMutex::new(()),
        }
    }

    pub fn sidecar(&self) -> SidecarInfo {
        guard(&self.sidecar).clone()
    }

    pub fn config(&self) -> Option<ConfigView> {
        guard(&self.config).clone()
    }

    pub fn config_error(&self) -> Option<String> {
        guard(&self.config_error).clone()
    }

    pub fn probe_done(&self) -> bool {
        self.probe_done.load(Ordering::SeqCst)
    }
    pub fn auto_start_status(&self) -> AutoStartStatus {
        guard(&self.auto_start).clone()
    }

    pub fn set_auto_start(&self, state: AutoStartState, error: Option<String>) {
        *guard(&self.auto_start) = AutoStartStatus { state, error };
    }

    pub fn take_auto_start_attempt(&self) -> bool {
        self.auto_start_attempted.swap(true, Ordering::SeqCst)
    }

    pub fn reset_auto_start_attempt(&self) {
        self.auto_start_attempted.store(false, Ordering::SeqCst);
    }

    pub fn control(&self) -> Option<Arc<crate::control::ControlClient>> {
        guard(&self.control)
            .as_ref()
            .map(|(_, client)| Arc::clone(client))
    }

    pub fn set_control(&self, port: u16, client: Arc<crate::control::ControlClient>) {
        *guard(&self.control) = Some((port, client));
    }

    pub fn cached_control_port(&self) -> Option<u16> {
        guard(&self.control).as_ref().map(|(port, _)| *port)
    }

    pub fn plugins_view(&self) -> PluginsChanged {
        let cache = guard(&self.plugins);
        PluginsChanged {
            instance: cache.instance.clone(),
            plugins: cache.plugins.clone(),
            preset: cache.preset.clone(),
            broken: cache.broken.clone(),
            stale: cache.stale,
            reachable: cache.reachable,
            error: cache.error.clone(),
            at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|now| now.as_secs())
                .unwrap_or_default(),
        }
    }
}

impl Default for DesktopState {
    fn default() -> Self {
        Self::new()
    }
}

fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

pub fn snapshot(app: &AppHandle) -> Snapshot {
    let state = app.state::<DesktopState>();
    let view = state.plugins_view();
    Snapshot {
        sidecar: state.sidecar(),
        config: state.config(),
        config_error: state.config_error(),
        status: status(app),
        auto_start: state.auto_start_status(),
        plugins: view.plugins,
        plugins_error: view.error,
        plugins_stale: view.stale,
        plugins_reachable: view.reachable,
        plugins_preset: view.preset,
        plugins_broken: view.broken,
        cli_path: sidecar_program()
            .ok()
            .map(|path| path.display().to_string()),
    }
}

/// Probe the sidecar and the effective configuration. Runs once after setup;
/// the UI can refresh the configuration later without respawning anything.
pub async fn probe(app: &AppHandle) {
    let sidecar = match sidecar_version(app).await {
        Ok(info) => info,
        Err(error) => SidecarInfo {
            version: String::new(),
            target: String::new(),
            compatible: false,
            message: error,
        },
    };
    let (config, config_error) = match read_config(app).await {
        Ok(view) => (Some(view), None),
        Err(error) => (None, Some(error)),
    };

    let state = app.state::<DesktopState>();
    *guard(&state.sidecar) = sidecar.clone();
    *guard(&state.config) = config;
    *guard(&state.config_error) = config_error;
    state.probe_done.store(true, Ordering::SeqCst);
    let _ = app.emit("service-changed", ());
    if std::env::var_os("PRINTKIT_DESKTOP_REQUIRE_SIDECAR").is_some() {
        let problem = if !sidecar.compatible {
            Some(sidecar.message.clone())
        } else {
            state.config_error()
        };
        if let Some(problem) = problem {
            eprintln!("printkit-desktop: require-sidecar failed: {problem}");
            std::process::exit(2);
        }
    }
    if !sidecar.compatible {
        let _ = app.emit("desktop-notice", sidecar.message.clone());
    }
}

/// Pull `/admin/plugins` off the IPC thread. `force` is honoured for events
/// and write commands, but a fetch less than 750 ms old is reused so a burst
/// of `service-changed` events cannot stampede the loopback API.
pub async fn refresh_plugins(app: &AppHandle, force: bool) {
    let service = status(app);
    let state = app.state::<DesktopState>();
    let _gate = state.plugin_fetch_gate.lock().await;
    let config = state.config();
    let port = crate::control::port_for(&service, config.as_ref());
    let data_dir = config.as_ref().map(|view| PathBuf::from(&view.data_dir));
    let instance = match (service.reachable, port, data_dir.as_ref()) {
        (true, Some(port), Some(data_dir)) => Some(format!(
            "{}|{port}|{}|{}",
            data_dir.display(),
            state_label(service.state),
            lock_instance(app).unwrap_or_default()
        )),
        _ => None,
    };

    let mut fetch = false;
    let mut publish = false;
    {
        let mut cache = guard(&state.plugins);
        match instance {
            None => {
                if cache.reachable || cache.instance.is_empty() {
                    publish = true;
                }
                cache.reachable = false;
                cache.instance.clear();
                if cache.plugins.is_some() && !cache.stale {
                    cache.stale = true;
                    publish = true;
                }
                let error = "the printkit service is not reachable".to_owned();
                if cache.error.as_deref() != Some(error.as_str()) {
                    cache.error = Some(error);
                    publish = true;
                }
            }
            Some(next) => {
                if cache.instance != next {
                    cache.instance = next;
                    cache.plugins = None;
                    cache.preset = None;
                    cache.broken = None;
                    cache.error = None;
                    cache.stale = false;
                    cache.fetched_at = None;
                    cache.reachable = true;
                    fetch = true;
                    publish = true;
                } else {
                    cache.reachable = true;
                    let recent = cache
                        .fetched_at
                        .is_some_and(|instant| instant.elapsed() < Duration::from_millis(750));
                    let due = cache
                        .fetched_at
                        .is_none_or(|instant| instant.elapsed() >= Duration::from_secs(10));
                    fetch = !recent && (force || due);
                }
            }
        }
        if fetch {
            // Reserve the slot before the blocking call so concurrent events
            // see a fresh timestamp.
            cache.fetched_at = Some(Instant::now());
        }
    }

    if fetch {
        if port.is_none() || data_dir.is_none() {
            return;
        }
        let handle = app.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            // Reuse the shell's logged-in client. A fresh connect logs in on
            // every 2 s poll and accumulates server sessions; the cached client
            // re-authenticates only when its port changes or a request 401s.
            let client = crate::commands::control_client(&handle)
                .map_err(|error| error.to_string())?;
            client
                .get_json("/admin/plugins")
                .map_err(|error| error.to_string())
        })
        .await
        .unwrap_or_else(|error| Err(format!("plugin refresh task failed: {error}")));
        let mut cache = guard(&state.plugins);
        cache.fetched_at = Some(Instant::now());
        match outcome {
            Ok(value) => {
                if let Err(error) = store_plugins(&mut cache, &value) {
                    cache.error = Some(error);
                    cache.stale = cache.plugins.is_some();
                }
            }
            Err(error) => {
                // A build without the plugin API answers 404; that is an
                // unknown state, not a stale "everything is on".
                if error.contains("(HTTP 404)") {
                    cache.plugins = None;
                    cache.preset = None;
                    cache.broken = None;
                }
                cache.error = Some(error);
                cache.stale = cache.plugins.is_some();
            }
        }
        publish = true;
    }

    if publish {
        let _ = app.emit("plugins-changed", state.plugins_view());
    }
}

fn store_plugins(
    cache: &mut PluginCache,
    value: &serde_json::Value,
) -> Result<Vec<PluginView>, String> {
    let raw = value
        .get("plugins")
        .cloned()
        .ok_or_else(|| "the plugin API did not return a plugins list".to_owned())?;
    let plugins: Vec<PluginView> = serde_json::from_value(raw)
        .map_err(|error| format!("the plugin list has an unexpected shape: {error}"))?;
    cache.plugins = Some(plugins.clone());
    cache.preset = value
        .get("preset")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    cache.broken = value
        .get("broken")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    cache.error = None;
    cache.stale = false;
    cache.reachable = true;
    cache.fetched_at = Some(Instant::now());
    Ok(plugins)
}

/// Apply a `POST /admin/plugins` response immediately, so every window sees
/// the new state without waiting for the next poll.
pub fn apply_plugins(
    app: &AppHandle,
    value: &serde_json::Value,
) -> Result<Vec<PluginView>, String> {
    let state = app.state::<DesktopState>();
    let plugins = store_plugins(&mut guard(&state.plugins), value)?;
    let _ = app.emit("plugins-changed", state.plugins_view());
    Ok(plugins)
}

/// Keep the cache in step with the service: `service-changed` refreshes
/// immediately, the 2 s loop covers a silently vanishing sidecar, and the
/// 10 s due-check inside [`refresh_plugins`] bounds background traffic.
pub fn install_plugin_refresh(app: &AppHandle) {
    use tauri::Listener;

    let handle = app.clone();
    app.listen("service-changed", move |_| {
        let handle = handle.clone();
        tauri::async_runtime::spawn(async move {
            refresh_plugins(&handle, true).await;
        });
    });
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            refresh_plugins(&handle, false).await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

/// Start an owned service once after the first probe. External instances are
/// attached, never started; incompatible sidecars and bad config are reported
/// without a retry loop.
/// The pure precondition half of auto-start, so the state machine can be
/// tested without spawning a service.
pub fn auto_start_decision(
    status: &ServiceStatus,
    sidecar: &SidecarInfo,
    config_error: Option<&str>,
    current: AutoStartState,
) -> (AutoStartState, Option<String>) {
    if current == AutoStartState::Disabled {
        return (AutoStartState::Disabled, None);
    }
    match status.state {
        ServiceState::RunningOwned => (AutoStartState::Started, None),
        ServiceState::RunningExternal => (AutoStartState::SkippedExternal, None),
        ServiceState::Stopped => {
            if !sidecar.compatible {
                (
                    AutoStartState::SkippedIncompatible,
                    Some(sidecar.message.clone()),
                )
            } else if let Some(error) = config_error {
                (AutoStartState::SkippedConfig, Some(error.to_owned()))
            } else {
                (AutoStartState::Attempting, None)
            }
        }
    }
}

/// Reclaiming an orphan is allowed only when the shell would otherwise start
/// an owned service: the lock is external/stopped, the sidecar is compatible
/// and the config is readable. An incompatible build or a broken config keeps
/// the running external service.
pub fn should_take_over_orphan(
    decision: AutoStartState,
    sidecar_compatible: bool,
    config_error: Option<&str>,
) -> bool {
    matches!(decision, AutoStartState::SkippedExternal | AutoStartState::Attempting)
        && sidecar_compatible
        && config_error.is_none()
}

pub async fn auto_start(app: &AppHandle) {
    let state = app.state::<DesktopState>();
    if state.take_auto_start_attempt() {
        return;
    }
    let mut current = status(app);
    let info = state.sidecar();
    let config_error = state.config_error();
    let (mut decision, mut error) = auto_start_decision(
        &current,
        &info,
        config_error.as_deref(),
        state.auto_start_status().state,
    );
    // An orphan may be reclaimed only on the way to starting an owned service.
    // An incompatible sidecar or a broken config keeps the external service
    // and reports why, instead of shutting it down first.
    if should_take_over_orphan(decision, info.compatible, config_error.as_deref())
        && take_over_orphan(app, &current).await.unwrap_or(false)
    {
        current = status(app);
        (decision, error) = auto_start_decision(
            &current,
            &info,
            config_error.as_deref(),
            state.auto_start_status().state,
        );
    }
    let outcome = if decision == AutoStartState::Attempting {
        state.set_auto_start(AutoStartState::Attempting, None);
        let _ = app.emit("service-changed", ());
        match start(app).await {
            Ok(_) => (AutoStartState::Started, None),
            Err(error) => (AutoStartState::Failed, Some(error)),
        }
    } else {
        (decision, error)
    };
    state.set_auto_start(outcome.0, outcome.1);
    let _ = app.emit("service-changed", ());
}

pub async fn retry_start(app: &AppHandle) -> Result<ServiceStatus, String> {
    let state = app.state::<DesktopState>();
    let next = retry_status(&state.auto_start_status());
    if next.state != AutoStartState::Attempting {
        return Err("retry is only available after a failed start".to_owned());
    }
    state.reset_auto_start_attempt();
    state.set_auto_start(next.state, next.error);
    let _ = app.emit("service-changed", ());
    match start(app).await {
        Ok(status) => {
            state.set_auto_start(AutoStartState::Started, None);
            let _ = app.emit("service-changed", ());
            Ok(status)
        }
        Err(error) => {
            state.set_auto_start(AutoStartState::Failed, Some(error.clone()));
            let _ = app.emit("service-changed", ());
            Err(error)
        }
    }
}

pub async fn refresh_config(app: &AppHandle) -> Result<ConfigView, String> {
    let state = app.state::<DesktopState>();
    match read_config(app).await {
        Ok(view) => {
            *guard(&state.config) = Some(view.clone());
            *guard(&state.config_error) = None;
            let _ = app.emit("service-changed", ());
            Ok(view)
        }
        Err(error) => {
            *guard(&state.config_error) = Some(error.clone());
            let _ = app.emit("service-changed", ());
            Err(error)
        }
    }
}

async fn sidecar_version(app: &AppHandle) -> Result<SidecarInfo, String> {
    let raw = run_sidecar(app, &["version", "--json"]).await?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|error| format!("printkit version returned invalid JSON: {error}"))?;
    let name = value
        .get("name")
        .and_then(|item| item.as_str())
        .unwrap_or("");
    let version = value
        .get("version")
        .and_then(|item| item.as_str())
        .unwrap_or("");
    let target = value
        .get("target")
        .and_then(|item| item.as_str())
        .unwrap_or("");
    let expected_target = std::env::consts::OS;
    let compatible = name == "printkit" && version == APP_VERSION && target == expected_target;
    let message = if compatible {
        format!("printkit {version} ({target})")
    } else {
        format!(
            "Expected printkit {APP_VERSION} for {expected_target}, found {name} {version} ({target})"
        )
    };
    Ok(SidecarInfo {
        version: version.to_owned(),
        target: target.to_owned(),
        compatible,
        message,
    })
}

async fn read_config(app: &AppHandle) -> Result<ConfigView, String> {
    let raw = run_sidecar(app, &["config", "--json"]).await?;
    let json: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|error| format!("printkit config returned invalid JSON: {error}"))?;
    let data_dir = json
        .pointer("/data_dir/value")
        .and_then(|item| item.as_str())
        .ok_or_else(|| "printkit config did not report data_dir".to_owned())?;
    let port = json
        .pointer("/server/port/value")
        .and_then(|item| item.as_u64())
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| "printkit config did not report a usable server.port".to_owned())?;
    Ok(ConfigView {
        data_dir: data_dir.to_owned(),
        port,
        json,
    })
}

pub async fn run_sidecar(app: &AppHandle, args: &[&str]) -> Result<String, String> {
    run_sidecar_owned(app, args.iter().map(|arg| (*arg).to_owned()).collect()).await
}

/// Resolve the sidecar next to this executable. Tauri's `sidecar()` helper
/// rejects paths whose `current_exe()` contains a symlink (macOS), which makes
/// a worktree under `/tmp` unable to probe the bundled binary. An explicit
/// `PRINTKIT_SIDECAR_PATH` wins for development and tests.
fn sidecar_program() -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os("PRINTKIT_SIDECAR_PATH") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "PRINTKIT_SIDECAR_PATH does not name a file: {}",
            path.display()
        ));
    }
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot locate the desktop executable: {error}"))?;
    let directory = exe
        .parent()
        .ok_or_else(|| "the desktop executable has no parent directory".to_owned())?;
    let path = directory.join(format!("{SIDECAR}{}", std::env::consts::EXE_SUFFIX));
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "the bundled printkit sidecar is missing at {}",
            path.display()
        ))
    }
}

pub async fn run_sidecar_owned(app: &AppHandle, args: Vec<String>) -> Result<String, String> {
    let budget = sidecar_budget(&args);
    let command = app.shell().command(sidecar_program()?).args(args);
    let output = tokio::time::timeout(budget, command.output())
        .await
        .map_err(|_| format!("printkit did not answer within {}s", budget.as_secs()))?
        .map_err(|error| format!("cannot run printkit: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        return Err(if stderr.is_empty() { stdout } else { stderr });
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("printkit wrote non-UTF-8 output: {error}"))
}

pub fn status(app: &AppHandle) -> ServiceStatus {
    let state = app.state::<DesktopState>();
    let config = state.config();
    let owned = guard(&state.child)
        .as_ref()
        .map(|child| (child.pid, child.instance.clone()));

    let Some(config) = config else {
        return ServiceStatus {
            state: ServiceState::Stopped,
            pid: None,
            port: None,
            reachable: false,
            lock_path: "unknown".to_owned(),
            stale_lock: false,
            detail: Some(
                state
                    .config_error()
                    .unwrap_or_else(|| "The data directory is not known yet.".to_owned()),
            ),
        };
    };

    let data_dir = PathBuf::from(&config.data_dir);
    let lock_path = data_dir.join(LOCK_FILE);
    let lock = read_lock(&lock_path);
    classify(
        lock,
        owned,
        pid_alive,
        port_open,
        lock_identity,
        lock_path.display().to_string(),
    )
}

#[derive(Debug, Deserialize)]
struct LockInfo {
    pid: u32,
    port: u16,
    /// Per-run id written by the service. A reused pid alone never matches.
    #[serde(default)]
    instance: Option<String>,
    /// Canonical service spelling is `executable`; `exe` is the older alias.
    #[serde(default, alias = "exe")]
    executable: Option<String>,
    /// Unix start seconds as written by the service (a string); `started_at`
    /// was the desktop's earlier numeric spelling.
    #[serde(default, alias = "started_at")]
    started: Option<String>,
}

#[derive(Debug)]
enum LockRead {
    Missing,
    Invalid(String),
    Present(LockInfo),
}

fn read_lock(path: &Path) -> LockRead {
    match fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<LockInfo>(&text) {
            Ok(info) => LockRead::Present(info),
            Err(error) => LockRead::Invalid(format!("serve.lock is unreadable: {error}")),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LockRead::Missing,
        Err(error) => LockRead::Invalid(format!("cannot read serve.lock: {error}")),
    }
}

#[allow(clippy::too_many_arguments)]
fn classify(
    lock: LockRead,
    owned: Option<(u32, Option<String>)>,
    alive: impl Fn(u32) -> bool,
    reachable: impl Fn(u16) -> bool,
    identity: impl Fn(&LockInfo) -> bool,
    lock_path: String,
) -> ServiceStatus {
    let owned_pid = owned.as_ref().map(|(pid, _)| *pid);
    match lock {
        LockRead::Missing => ServiceStatus {
            state: ServiceState::Stopped,
            pid: None,
            port: None,
            reachable: false,
            lock_path,
            stale_lock: false,
            detail: None,
        },
        LockRead::Invalid(detail) => ServiceStatus {
            state: ServiceState::Stopped,
            pid: None,
            port: None,
            reachable: false,
            lock_path,
            stale_lock: true,
            detail: Some(detail),
        },
        LockRead::Present(info) if !alive(info.pid) => ServiceStatus {
            state: ServiceState::Stopped,
            pid: Some(info.pid),
            port: Some(info.port),
            reachable: false,
            lock_path,
            stale_lock: true,
            detail: Some(
                "serve.lock names a process that is no longer running; the next serve reclaims it."
                    .to_owned(),
            ),
        },
        LockRead::Present(info)
            if owned_pid == Some(info.pid) && !owned_instance_matches(&owned, &info) =>
        {
            ServiceStatus {
                state: ServiceState::Stopped,
                pid: Some(info.pid),
                port: Some(info.port),
                reachable: false,
                lock_path,
                stale_lock: true,
                detail: Some(
                    "serve.lock reuses our pid with a different instance; refusing to treat it as ours."
                        .to_owned(),
                ),
            }
        }
        LockRead::Present(info) if owned_pid != Some(info.pid) && !identity(&info) => {
            ServiceStatus {
                state: ServiceState::Stopped,
                pid: Some(info.pid),
                port: Some(info.port),
                reachable: false,
                lock_path,
                stale_lock: true,
                detail: Some(
                    "serve.lock names a process that is not printkit; refusing to attach or send credentials."
                        .to_owned(),
                ),
            }
        }
        LockRead::Present(info) => {
            // An owned child must present the instance captured from its own
            // lock. A pid that matches but an instance that does not means the
            // pid was reused, so this is not our service.
            let owned = owned_pid == Some(info.pid) && owned_instance_matches(&owned, &info);
            ServiceStatus {
                state: if owned {
                    ServiceState::RunningOwned
                } else {
                    ServiceState::RunningExternal
                },
                pid: Some(info.pid),
                port: Some(info.port),
                reachable: reachable(info.port),
                lock_path,
                stale_lock: false,
                detail: if reachable(info.port) {
                    None
                } else {
                    Some(
                        "The process is alive but the port is not accepting connections."
                            .to_owned(),
                    )
                },
            }
        }
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 performs the permission and existence check without
    // delivering a signal.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Windows has no `kill -0`; `OpenProcess` answers the same question.
///
/// "Cannot open" is not "gone": a process owned by another user still holds
/// its pid, so access denied must keep the lock. Only a missing pid frees it.
#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::core::HRESULT;

    // SAFETY: takes plain values and returns an owned handle or an error.
    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => handle,
        Err(error) => return error.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0),
    };
    let mut exit_code = 0u32;
    // SAFETY: the handle is live and `exit_code` is a writable local.
    let queried = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    // SAFETY: the handle came from OpenProcess and is closed exactly once.
    unsafe {
        let _ = CloseHandle(handle);
    }
    queried.is_ok() && exit_code == STILL_ACTIVE.0 as u32
}

/// No other host builds the desktop shell today. Answering "not alive" is
/// safer than the old unconditional `true`: a stale lock is reclaimed instead
/// of pinning a service that does not exist.
#[cfg(all(not(unix), not(windows)))]
fn pid_alive(_pid: u32) -> bool {
    false
}


fn is_printkit_name(path: &str) -> bool {
    let Some(name) = Path::new(path.trim())
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    if name.starts_with("printkit-desktop") {
        return false;
    }
    name == "printkit" || name == "printkit.exe" || name.starts_with("printkit-")
}

/// An external lock is trusted only when it carries a per-run instance, names
/// a printkit executable, and its pid's real start time matches the recorded
/// one. A copied data dir or a recycled pid must not receive the admin token.
fn lock_identity(info: &LockInfo) -> bool {
    lock_fields_identity(info)
        && process_name(info.pid).is_some_and(|name| is_printkit_name(&name))
        && start_time_matches(info.pid, info.started.as_deref())
}

/// The parts of the identity a lock must carry, independent of the OS
/// (testable): a concrete instance and a printkit executable name.
fn lock_fields_identity(info: &LockInfo) -> bool {
    let instance_ok = info
        .instance
        .as_deref()
        .is_some_and(|value| !value.is_empty() && value != "unknown");
    let exe_ok = info
        .executable
        .as_deref()
        .is_some_and(is_printkit_name);
    instance_ok && exe_ok
}

/// Unix start seconds recorded by the service must be within two minutes of
/// the process's real start; otherwise the pid was reused.
const START_TIME_TOLERANCE_SECS: u64 = 120;

fn start_time_matches(pid: u32, started: Option<&str>) -> bool {
    let Some(started) = started.and_then(|value| value.trim().parse::<u64>().ok()) else {
        return false;
    };
    let Some(actual) = process_start_secs(pid) else {
        return false;
    };
    started.abs_diff(actual) <= START_TIME_TOLERANCE_SECS
}

#[cfg(unix)]
fn process_start_secs(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "etimes="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let elapsed = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(now.saturating_sub(elapsed))
}

#[cfg(windows)]
fn process_start_secs(pid: u32) -> Option<u64> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: opens a query handle for the pid and closes it once.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all four pointers are writable locals on a live handle.
    let queried =
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    // SAFETY: the handle came from OpenProcess.
    unsafe {
        let _ = CloseHandle(handle);
    }
    if queried.is_err() {
        return None;
    }
    let ticks = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    // FILETIME counts 100 ns ticks since 1601-01-01.
    Some(ticks / 10_000_000 - 11_644_473_600)
}

#[cfg(all(not(unix), not(windows)))]
fn process_start_secs(_pid: u32) -> Option<u64> {
    None
}

#[cfg(unix)]
fn process_name(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if name.is_empty() { None } else { Some(name) }
}

#[cfg(windows)]
fn process_name(pid: u32) -> Option<String> {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().next()?.split(',').next()?;
    Some(first.trim_matches('"').to_owned())
}

#[cfg(all(not(unix), not(windows)))]
fn process_name(_pid: u32) -> Option<String> {
    None
}

fn orphan_parent(ppid: u32) -> bool {
    ppid == 0 || ppid == 1
}

/// A sidecar whose parent is launchd/init was orphaned by a killed shell; it
/// may be shut down and reclaimed instead of staying read-only forever.
fn pid_is_orphan(pid: u32) -> bool {
    process_parent(pid).is_some_and(orphan_parent)
}

#[cfg(unix)]
fn process_parent(pid: u32) -> Option<u32> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "ppid="])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(windows)]
fn process_parent(_pid: u32) -> Option<u32> {
    // Windows has no init reparenting; a killed shell leaves no adoptable child.
    None
}

#[cfg(all(not(unix), not(windows)))]
fn process_parent(_pid: u32) -> Option<u32> {
    None
}

#[cfg(unix)]
fn kill_pid(pid: u32) -> bool {
    // SAFETY: a plain signal to a pid that was already identity-checked.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) == 0 }
}

#[cfg(windows)]
fn kill_pid(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    // SAFETY: opens a handle for the identity-checked pid and closes it once.
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, pid) else {
            return false;
        };
        let killed = TerminateProcess(handle, 1).is_ok();
        let _ = CloseHandle(handle);
        killed
    }
}

#[cfg(all(not(unix), not(windows)))]
fn kill_pid(_pid: u32) -> bool {
    false
}
fn port_open(port: u16) -> bool {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok()
}

/// A service is started only when the lock is ours *and* the port answers.
pub fn start_ready(status: &ServiceStatus) -> bool {
    status.state == ServiceState::RunningOwned && status.reachable
}

fn sidecar_budget(args: &[String]) -> Duration {
    if args.first().map(String::as_str) == Some("version") {
        SIDECAR_VERSION_TIMEOUT
    } else {
        SIDECAR_CONFIG_TIMEOUT
    }
}

/// An orphaned sidecar (parent launchd after the shell was SIGKILLed) can be
/// shut down gracefully and then reclaimed. A user-started external service is
/// left alone.
async fn take_over_orphan(app: &AppHandle, current: &ServiceStatus) -> Result<bool, String> {
    let Some(pid) = current.pid else {
        return Ok(false);
    };
    if !pid_is_orphan(pid) {
        return Ok(false);
    }
    let Some(config) = app.state::<DesktopState>().config() else {
        return Ok(false);
    };
    let port = current.port.unwrap_or(config.port);
    let data_dir = PathBuf::from(config.data_dir);
    let task = tokio::task::spawn_blocking(move || {
        crate::control::ControlClient::connect_with_timeout(
            data_dir,
            port,
            STOP_TIMEOUT + STOP_EXIT_WAIT,
        )
        .and_then(|client| client.shutdown())
        .is_ok()
    });
    let _ = tokio::time::timeout(STOP_TIMEOUT + STOP_EXIT_WAIT * 2, task).await;
    if !wait_for_exit(pid, STOP_EXIT_WAIT).await {
        let _ = kill_pid(pid);
        let _ = wait_for_exit(pid, KILL_WAIT).await;
    }
    cleanup_lock(app, pid);
    Ok(true)
}

/// A desktop-owned first start should show Jobs and Records without asking the
/// user to discover Plugin management. Existing `plugins.toml` is never
/// touched, and the CLI/HTTP service keeps its core-only default. Calibration
/// is deliberately not enabled.
const DESKTOP_PLUGINS: &str = "version = 1\nsettled = 2\npreset = \"core\"\n\
[[plugins]]\nid = \"observability\"\nenabled = true\n\
[[plugins]]\nid = \"records\"\nenabled = true\n";

fn ensure_desktop_plugins(data_dir: &Path) -> Result<bool, String> {
    fs::create_dir_all(data_dir)
        .map_err(|error| format!("cannot create {}: {error}", data_dir.display()))?;
    let path = data_dir.join("plugins.toml");
    if path.exists() {
        return Ok(false);
    }
    // Write a complete sibling first, then publish it with an atomic
    // no-replace link. A concurrent desktop or a user-created file wins
    // without being truncated, and no reader observes a partial TOML body.
    static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let sequence = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temporary = data_dir.join(format!(
        ".plugins.toml.tmp-{}-{stamp}-{sequence}",
        std::process::id()
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options
            .open(&temporary)
            .map_err(|error| format!("cannot create {}: {error}", temporary.display()))?;
        use std::io::Write as _;
        file.write_all(DESKTOP_PLUGINS.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("cannot write {}: {error}", temporary.display()))?;
        drop(file);
        match fs::hard_link(&temporary, &path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(format!("cannot publish {}: {error}", path.display())),
        }
    })();
    let _ = fs::remove_file(&temporary);
    result
}

pub async fn start(app: &AppHandle) -> Result<ServiceStatus, String> {
    let state = app.state::<DesktopState>();
    let _gate = state.start_gate.lock().await;
    if !state.probe_done() {
        return Err("The printkit sidecar is still being checked.".to_owned());
    }
    let info = state.sidecar();
    if !info.compatible {
        return Err(info.message);
    }
    let Some(config) = state.config() else {
        return Err(state
            .config_error()
            .unwrap_or_else(|| "The printkit configuration is unavailable.".to_owned()));
    };
    let mut current = status(app);
    if current.state == ServiceState::RunningExternal {
        if take_over_orphan(app, &current).await? {
            current = status(app);
        } else {
            return Err("The service is already running (external).".to_owned());
        }
    }
    if current.state != ServiceState::Stopped {
        return Err(format!(
            "The service is already {}.",
            state_label(current.state)
        ));
    }

    ensure_desktop_plugins(&PathBuf::from(&config.data_dir))?;
    guard(&state.output).clear();
    let port = config.port.to_string();
    let command = app
        .shell()
        .command(sidecar_program()?)
        .kill_on_drop(true)
        .args([
            "serve",
            "--data-dir",
            config.data_dir.as_str(),
            "--port",
            port.as_str(),
        ]);
    let (mut events, child) = command
        .spawn()
        .map_err(|error| format!("cannot start printkit serve: {error}"))?;
    let pid = child.pid();
    {
        let mut slot = guard(&state.child);
        if slot.is_some() {
            let _ = child.kill();
            return Err("Another sidecar process is already tracked.".to_owned());
        }
        *slot = Some(ManagedChild {
            pid,
            instance: None,
            child,
        });
    }

    let event_app = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                CommandEvent::Stdout(line) | CommandEvent::Stderr(line) => {
                    let text = String::from_utf8_lossy(&line).trim().to_owned();
                    if !text.is_empty() {
                        remember_output(&event_app, text);
                    }
                }
                CommandEvent::Error(error) => remember_output(&event_app, error),
                CommandEvent::Terminated(payload) => {
                    let state = event_app.state::<DesktopState>();
                    let ours = {
                        let mut slot = guard(&state.child);
                        if slot.as_ref().is_some_and(|child| child.pid == pid) {
                            slot.take();
                            true
                        } else {
                            false
                        }
                    };
                    let expected = {
                        let mut slot = guard(&state.expected_exit);
                        if *slot == Some(pid) {
                            *slot = None;
                            true
                        } else {
                            false
                        }
                    };
                    if ours && !expected {
                        let status = match payload.code {
                            Some(code) => format!("status {code}"),
                            None => "a signal".to_owned(),
                        };
                        let message = format!("printkit serve exited with {status}.");
                        let _ = event_app.emit("desktop-notice", message);
                    }
                    if ours {
                        let _ = event_app.emit("service-changed", ());
                    }
                }
                _ => {}
            }
        }
    });

    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        let now = status(app);
        if start_ready(&now) {
            if let Some(instance) = lock_instance(app) {
                if let Some(child) = guard(&state.child).as_mut() {
                    if child.pid == pid {
                        child.instance = Some(instance);
                    }
                }
            }
            return Ok(now);
        } else if !tracked_child_alive(&state, pid) {
            cleanup_lock(app, pid);
            return Err(format!(
                "printkit serve did not start: {}",
                recent_output(&state)
            ));
        }

        if Instant::now() >= deadline {
            // A lock without a reachable port is not a started service.
            let _ = kill_child(&state);
            cleanup_lock(app, pid);
            return Err(format!(
                "Timed out waiting for serve.lock; {}",
                recent_output(&state)
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub async fn stop(app: &AppHandle) -> Result<ServiceStatus, String> {
    let state = app.state::<DesktopState>();
    let _gate = state.start_gate.lock().await;
    let current = status(app);
    match current.state {
        ServiceState::Stopped => Ok(current),
        ServiceState::RunningExternal => Err(
            "The service was started outside this desktop shell; it is attached read-only."
                .to_owned(),
        ),
        ServiceState::RunningOwned => {
            let pid = current
                .pid
                .ok_or_else(|| "serve.lock does not name a process.".to_owned())?;
            let port = current.port.unwrap_or(0);
            let state = app.state::<DesktopState>();
            *guard(&state.expected_exit) = Some(pid);

            let graceful = if let Some(data_dir) = state
                .config()
                .map(|config| PathBuf::from(config.data_dir))
            {
                let wait = STOP_TIMEOUT + STOP_EXIT_WAIT;
                let task = tokio::task::spawn_blocking(move || {
                    crate::control::ControlClient::connect_with_timeout(
                        data_dir, port, STOP_TIMEOUT,
                    )
                    .and_then(|client| client.shutdown())
                    .is_ok()
                });
                matches!(tokio::time::timeout(wait, task).await, Ok(Ok(true)))
            } else {
                false
            };
            if graceful && wait_for_exit(pid, STOP_EXIT_WAIT).await {
                return Ok(status(app));
            }

            kill_child(&state)?;
            let deadline = Instant::now() + KILL_WAIT;
            while pid_alive(pid) && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            cleanup_lock(app, pid);
            Ok(status(app))
        }
    }
}

async fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while pid_alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    true
}


/// Explicit kill for `RunEvent::Exit`; `kill_on_drop` only covers clean drops.
/// A "quit and keep service" exit leaves the owned child running on purpose.
pub fn kill_owned_child(app: &AppHandle) {
    let state = app.state::<DesktopState>();
    if state.keep_service.load(Ordering::SeqCst) {
        return;
    }
    let child = guard(&state.child).take();
    if let Some(ManagedChild { child, .. }) = child {
        let _ = child.kill();
    }
}
fn kill_child(state: &DesktopState) -> Result<(), String> {
    let child = guard(&state.child).take();
    match child {
        Some(ManagedChild { child, .. }) => child
            .kill()
            .map_err(|error| format!("cannot stop printkit serve: {error}")),
        None => Ok(()),
    }
}

fn tracked_child_alive(state: &DesktopState, pid: u32) -> bool {
    guard(&state.child)
        .as_ref()
        .is_some_and(|child| child.pid == pid)
}

fn remember_output(app: &AppHandle, line: String) {
    let state = app.state::<DesktopState>();
    let mut output = guard(&state.output);
    output.push(line);
    let overflow = output.len().saturating_sub(20);
    if overflow > 0 {
        output.drain(..overflow);
    }
}

fn recent_output(state: &DesktopState) -> String {
    let output = guard(&state.output);
    if output.is_empty() {
        "no output from the sidecar".to_owned()
    } else {
        output.join(" | ")
    }
}

/// Remove a lock only when it names the process this shell just killed. Any
/// other lock belongs to a live or foreign service and must be left alone.
fn cleanup_lock(app: &AppHandle, pid: u32) {
    let Some(config) = app.state::<DesktopState>().config() else {
        return;
    };
    let path = PathBuf::from(config.data_dir).join(LOCK_FILE);
    cleanup_lock_file(&path, pid, pid_alive);
}

/// Remove a lock only when it names the process this shell just killed. A
/// concurrently acquired winner lock must survive the loser's cleanup.
fn cleanup_lock_file(path: &Path, pid: u32, alive: impl Fn(u32) -> bool) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let Ok(info) = serde_json::from_str::<LockInfo>(&text) else {
        return;
    };
    if info.pid == pid && !alive(pid) {
        let _ = fs::remove_file(path);
    }
}

/// Once the spawned child's instance is known, a pid match must carry that
/// same instance. Before the lock appears the instance is unknown, so the pid
/// alone is accepted for the short start window.
fn owned_instance_matches(owned: &Option<(u32, Option<String>)>, info: &LockInfo) -> bool {
    owned
        .as_ref()
        .and_then(|(_, instance)| instance.as_deref())
        .is_none_or(|instance| info.instance.as_deref() == Some(instance))
}

/// The per-run instance id from the current `serve.lock`, when readable.
fn lock_instance(app: &AppHandle) -> Option<String> {
    let config = app.state::<DesktopState>().config()?;
    let path = PathBuf::from(config.data_dir).join(LOCK_FILE);
    match read_lock(&path) {
        LockRead::Present(info) => info.instance,
        _ => None,
    }
}

fn state_label(state: ServiceState) -> &'static str {
    match state {
        ServiceState::Stopped => "stopped",
        ServiceState::RunningOwned => "running (owned)",
        ServiceState::RunningExternal => "running (external)",
    }
}

#[cfg(test)]
mod plugin_state_tests {
    use super::*;

    fn view(id: &str, state: &str) -> PluginView {
        PluginView {
            id: id.to_owned(),
            state: state.to_owned(),
            ..PluginView::default()
        }
    }

    #[test]
    fn availability_is_unknown_until_a_fresh_list_arrives() {
        let mut cache = PluginCache::default();
        assert_eq!(
            cache.availability("observability"),
            PluginAvailability::Unknown
        );

        cache.reachable = true;
        cache.plugins = Some(vec![
            view("observability", "on"),
            view("records", "off"),
        ]);
        assert_eq!(cache.availability("observability"), PluginAvailability::On);
        assert_eq!(cache.availability("records"), PluginAvailability::Off);
        assert_eq!(cache.availability("unknown-plugin"), PluginAvailability::Unknown);

        cache.stale = true;
        assert_eq!(
            cache.availability("observability"),
            PluginAvailability::Unknown
        );

        cache.stale = false;
        cache.reachable = false;
        assert_eq!(
            cache.availability("observability"),
            PluginAvailability::Unknown
        );
    }

    #[test]
    fn plugin_payloads_parse_without_optional_fields() {
        let value = serde_json::json!({
            "ok": true,
            "preset": "all",
            "settled": false,
            "broken": null,
            "plugins": [
                { "id": "observability", "state": "on", "active": true },
                { "id": "records", "state": "broken", "why": "dependency cycle" }
            ]
        });
        let mut cache = PluginCache::default();
        let plugins = store_plugins(&mut cache, &value).unwrap();
        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[0].state, "on");
        assert_eq!(plugins[1].state, "broken");
        assert_eq!(plugins[1].reason.as_deref(), Some("dependency cycle"));
        assert_eq!(cache.preset.as_deref(), Some("all"));
        assert!(cache.broken.is_none());
        assert!(!cache.stale);
        assert!(cache.reachable);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alive(_: u32) -> bool {
        true
    }

    fn dead(_: u32) -> bool {
        false
    }

    fn open(_: u16) -> bool {
        true
    }

    fn closed(_: u16) -> bool {
        false
    }

    fn lock(pid: u32, port: u16) -> LockRead {
        LockRead::Present(LockInfo {
            pid,
            port,
            instance: None,
            executable: None,
            started: None,
        })
    }

    fn printkit(_: &LockInfo) -> bool {
        true
    }

    fn other_process(_: &LockInfo) -> bool {
        false
    }

    #[test]
    fn lock_fields_require_instance_and_executable() {
        let mut info = LockInfo {
            pid: 42,
            port: 19080,
            instance: Some("pk_run".to_owned()),
            executable: Some("/Applications/printkit".to_owned()),
            started: Some("1700000000".to_owned()),
        };
        assert!(lock_fields_identity(&info));
        info.instance = None;
        assert!(!lock_fields_identity(&info));
        info.instance = Some("unknown".to_owned());
        assert!(!lock_fields_identity(&info));
        info.instance = Some("pk_run".to_owned());
        info.executable = Some("/bin/sh".to_owned());
        assert!(!lock_fields_identity(&info));
    }

    #[test]
    fn lock_fields_accept_service_and_legacy_spellings() {
        let service: LockInfo = serde_json::from_str(
            r#"{"pid":42,"port":19080,"instance":"pk_run","started":"1700000000","executable":"/opt/printkit"}"#,
        )
        .unwrap();
        assert_eq!(service.instance.as_deref(), Some("pk_run"));
        assert_eq!(service.executable.as_deref(), Some("/opt/printkit"));
        assert_eq!(service.started.as_deref(), Some("1700000000"));

        let legacy: LockInfo = serde_json::from_str(
            r#"{"pid":42,"port":19080,"instance":"pk_run","started_at":"1700000000","exe":"/opt/printkit"}"#,
        )
        .unwrap();
        assert_eq!(legacy.executable.as_deref(), Some("/opt/printkit"));
        assert_eq!(legacy.started.as_deref(), Some("1700000000"));
    }

    #[test]
    fn an_owned_pid_with_a_different_instance_is_not_ours() {
        let status = classify(
            LockRead::Present(LockInfo {
                pid: 42,
                port: 19080,
                instance: Some("pk_new".to_owned()),
                executable: Some("/opt/printkit".to_owned()),
                started: Some("1700000000".to_owned()),
            }),
            Some((42, Some("pk_old".to_owned()))),
            alive,
            open,
            printkit,
            "lock".to_owned(),
        );
        assert_eq!(status.state, ServiceState::Stopped);
    }

    #[test]
    fn missing_lock_is_stopped() {
        let status = classify(LockRead::Missing, None, alive, open, printkit, "lock".to_owned());
        assert_eq!(status.state, ServiceState::Stopped);
        assert!(!status.stale_lock);
        assert_eq!(status.pid, None);
    }

    #[test]
    fn own_child_is_owned() {
        let status = classify(lock(42, 19080), Some((42, None)), alive, open, printkit, "lock".to_owned());
        assert_eq!(status.state, ServiceState::RunningOwned);
        assert_eq!(status.port, Some(19080));
        assert!(status.reachable);
    }

    #[test]
    fn foreign_child_is_external() {
        let status = classify(lock(42, 19080), Some((7, None)), alive, open, printkit, "lock".to_owned());
        assert_eq!(status.state, ServiceState::RunningExternal);
    }

    #[test]
    fn a_non_printkit_process_is_never_attached() {
        let status = classify(
            lock(42, 19080),
            Some((7, None)),
            alive,
            open,
            other_process,
            "lock".to_owned(),
        );
        assert_eq!(status.state, ServiceState::Stopped);
        assert!(status.stale_lock);
        assert!(status.detail.is_some());
    }

    #[test]
    fn dead_pid_is_a_stale_stopped_lock() {
        let status = classify(lock(42, 19080), Some((42, None)), dead, open, printkit, "lock".to_owned());
        assert_eq!(status.state, ServiceState::Stopped);
        assert!(status.stale_lock);
        assert_eq!(status.port, Some(19080));
    }

    #[test]
    fn unreadable_lock_is_reported() {
        let status = classify(
            LockRead::Invalid("bad".to_owned()),
            None,
            alive,
            open,
            printkit,
            "lock".to_owned(),
        );
        assert_eq!(status.state, ServiceState::Stopped);
        assert!(status.stale_lock);
        assert_eq!(status.detail.as_deref(), Some("bad"));
    }

    #[test]
    fn closed_port_is_flagged() {
        let status = classify(
            lock(42, 19080),
            Some((42, None)),
            alive,
            closed,
            printkit,
            "lock".to_owned(),
        );
        assert_eq!(status.state, ServiceState::RunningOwned);
        assert!(!status.reachable);
        assert!(status.detail.is_some());
    }

    fn auto_status(state: ServiceState) -> ServiceStatus {
        ServiceStatus {
            state,
            pid: None,
            port: Some(19080),
            reachable: state != ServiceState::Stopped,
            lock_path: "serve.lock".to_owned(),
            stale_lock: false,
            detail: None,
        }
    }

    fn auto_sidecar(compatible: bool) -> SidecarInfo {
        SidecarInfo {
            version: "0.1.0".to_owned(),
            target: "macos".to_owned(),
            compatible,
            message: if compatible {
                "printkit 0.1.0 (macos)".to_owned()
            } else {
                "incompatible sidecar".to_owned()
            },
        }
    }

    #[test]
    fn orphan_takeover_requires_a_healthy_sidecar_and_config() {
        assert!(should_take_over_orphan(
            AutoStartState::SkippedExternal,
            true,
            None
        ));
        assert!(should_take_over_orphan(AutoStartState::Attempting, true, None));
        assert!(!should_take_over_orphan(
            AutoStartState::SkippedIncompatible,
            false,
            None
        ));
        assert!(!should_take_over_orphan(
            AutoStartState::SkippedConfig,
            true,
            Some("bad config")
        ));
        assert!(!should_take_over_orphan(
            AutoStartState::SkippedExternal,
            false,
            None
        ));
    }

    #[test]
    fn desktop_first_start_enables_observability_and_records_once() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ensure_desktop_plugins(dir.path()).unwrap());
        let text = std::fs::read_to_string(dir.path().join("plugins.toml")).unwrap();
        assert!(text.contains("id = \"observability\""));
        assert!(text.contains("id = \"records\""));
        assert!(!text.contains("calibration"));
        assert!(!ensure_desktop_plugins(dir.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("plugins.toml")).unwrap(),
            text
        );
    }

    #[test]
    fn desktop_plugin_default_never_overwrites_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugins.toml");
        std::fs::write(&path, "# user file\nversion = 1\n").unwrap();
        assert!(!ensure_desktop_plugins(dir.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# user file\nversion = 1\n"
        );
    }

    #[test]
    fn desktop_plugin_default_is_published_once_under_concurrency() {
        let dir = tempfile::tempdir().unwrap();
        let mut winners = 0;
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| ensure_desktop_plugins(dir.path()).unwrap()))
                .collect();
            for handle in handles {
                winners += usize::from(handle.join().unwrap());
            }
        });
        assert_eq!(winners, 1, "exactly one writer may publish the default");
        let text = std::fs::read_to_string(dir.path().join("plugins.toml")).unwrap();
        assert!(text.contains("id = \"observability\""));
        assert!(text.contains("id = \"records\""));
        assert!(!text.contains("calibration"));
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".plugins.toml.tmp-")
            })
            .collect();
        assert!(leftovers.is_empty(), "temporary files must be removed");
    }

    #[test]
    fn auto_start_decision_covers_the_state_machine() {
        assert_eq!(
            auto_start_decision(&auto_status(ServiceState::RunningOwned), &auto_sidecar(true), None, AutoStartState::Idle).0,
            AutoStartState::Started
        );
        assert_eq!(
            auto_start_decision(&auto_status(ServiceState::RunningExternal), &auto_sidecar(true), None, AutoStartState::Idle).0,
            AutoStartState::SkippedExternal
        );
        assert_eq!(
            auto_start_decision(&auto_status(ServiceState::Stopped), &auto_sidecar(false), None, AutoStartState::Idle).0,
            AutoStartState::SkippedIncompatible
        );
        assert_eq!(
            auto_start_decision(&auto_status(ServiceState::Stopped), &auto_sidecar(true), Some("config"), AutoStartState::Idle).0,
            AutoStartState::SkippedConfig
        );
        assert_eq!(
            auto_start_decision(&auto_status(ServiceState::Stopped), &auto_sidecar(true), None, AutoStartState::Idle).0,
            AutoStartState::Attempting
        );
    }

    #[test]
    fn retry_resets_failed_and_keeps_disabled() {
        let failed = start_failed("boom");
        let retry = retry_status(&failed);
        assert_eq!(retry.state, AutoStartState::Attempting);
        assert!(retry.error.is_none());

        let disabled = stopped_by_user();
        assert_eq!(retry_status(&disabled).state, AutoStartState::Disabled);
    }

    #[test]
    fn disabled_is_not_auto_started() {
        let (state, error) = auto_start_decision(
            &auto_status(ServiceState::Stopped),
            &auto_sidecar(true),
            None,
            AutoStartState::Disabled,
        );
        assert_eq!(state, AutoStartState::Disabled);
        assert!(error.is_none());
    }

    #[test]
    fn tray_and_command_start_results_use_the_same_states() {
        assert_eq!(start_attempt().state, AutoStartState::Attempting);
        assert_eq!(start_succeeded().state, AutoStartState::Started);
        let failed = start_failed("detail");
        assert_eq!(failed.state, AutoStartState::Failed);
        assert_eq!(failed.error.as_deref(), Some("detail"));
        assert_eq!(stopped_by_user().state, AutoStartState::Disabled);
    }

    fn test_lock_path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "printkit-lifecycle-{}-{}-{name}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        fs::write(&path, r#"{"pid":42,"port":19080}"#).unwrap();
        path
    }

    #[test]
    fn only_a_reachable_owned_service_is_a_started_service() {
        let mut status = auto_status(ServiceState::RunningOwned);
        status.reachable = true;
        assert!(start_ready(&status));
        status.reachable = false;
        assert!(!start_ready(&status));
        status.state = ServiceState::RunningExternal;
        status.reachable = true;
        assert!(!start_ready(&status));
    }

    #[test]
    fn only_launchd_parented_pids_are_orphans() {
        assert!(orphan_parent(1));
        assert!(orphan_parent(0));
        assert!(!orphan_parent(42));
    }

    #[test]
    fn bundled_sidecar_names_match_the_identity_check() {
        assert!(is_printkit_name("/x/printkit"));
        assert!(is_printkit_name("/x/printkit.exe"));
        assert!(is_printkit_name("/x/binaries/printkit-aarch64-apple-darwin"));
        assert!(!is_printkit_name("/x/printkit-desktop"));
        assert!(!is_printkit_name("/x/python3"));
    }

    #[test]
    fn sidecar_subcommands_have_bounded_budgets() {
        assert_eq!(
            sidecar_budget(&["version".to_owned(), "--json".to_owned()]),
            SIDECAR_VERSION_TIMEOUT
        );
        assert_eq!(
            sidecar_budget(&["config".to_owned(), "--json".to_owned()]),
            SIDECAR_CONFIG_TIMEOUT
        );
    }

    #[test]
    fn a_loser_never_deletes_the_winner_lock() {
        let path = test_lock_path("owner");
        cleanup_lock_file(&path, 42, |_| true);
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"pid":42,"port":19080}"#);

        cleanup_lock_file(&path, 7, |_| false);
        assert!(path.exists(), "another pid must not be removed");

        cleanup_lock_file(&path, 42, |_| false);
        assert!(!path.exists(), "the dead owner is cleaned up");
        let _ = fs::remove_file(path);
    }
}
