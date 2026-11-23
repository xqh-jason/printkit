//! The single configuration entry point.
//!
//! Every source is parsed here and nowhere else: CLI flags, environment
//! variables and `config.toml`. Each value keeps the source that won, so
//! `printkit config --json` and `printkit doctor --json` can explain why a value
//! is what it is.
//!
//! Startup has two phases because the data directory decides where
//! `config.toml` lives:
//!
//! 1. resolve `data_dir`: `--data-dir` > `PRINTKIT_DATA_DIR` > `portable.flag`
//!    next to the executable > platform default;
//! 2. read `<data_dir>/config.toml`, then merge every key: CLI > env > file >
//!    default.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use printkit_facade::LogLevel;
use serde::{Deserialize, Serialize};

/// Environment variables read by the configuration layer only.
const ENV_DATA_DIR: &str = "PRINTKIT_DATA_DIR";
const ENV_LOG: &str = "PRINTKIT_LOG";
const ENV_PORT: &str = "PRINTKIT_PORT";
const ENV_RENDER_WORKERS: &str = "PRINTKIT_RENDER_WORKERS";
const ENV_QUEUE_MAX: &str = "PRINTKIT_QUEUE_MAX";
const ENV_QUEUE_TIMEOUT: &str = "PRINTKIT_QUEUE_TIMEOUT_MS";
const ENV_FETCH_TIMEOUT: &str = "PRINTKIT_FETCH_TIMEOUT_MS";
const ENV_FETCH_MAX_BYTES: &str = "PRINTKIT_FETCH_MAX_BYTES";

/// Where a value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigSource {
    Cli,
    Env,
    File,
    Db,
    Default,
}

impl ConfigSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Env => "env",
            Self::File => "file",
            Self::Db => "db",
            Self::Default => "default",
        }
    }
}

/// A value plus the source that produced it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sourced<T> {
    pub value: T,
    pub source: ConfigSource,
}

impl<T> Sourced<T> {
    fn new(value: T, source: ConfigSource) -> Self {
        Self { value, source }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerConfig {
    pub port: Sourced<u16>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogConfig {
    pub level: Sourced<LogLevel>,
    pub retention_days: Sourced<u16>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RenderConfig {
    pub workers: Sourced<u8>,
    pub queue_max: Sourced<u16>,
    pub queue_timeout_ms: Sourced<u32>,
    pub warmup: Sourced<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FetchConfig {
    pub timeout_ms: Sourced<u32>,
    pub max_bytes: Sourced<u64>,
    pub max_redirects: Sourced<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordsConfig {
    pub retention_days: Sourced<u16>,
    pub max_rows: Sourced<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreviewConfig {
    pub max_windows: Sourced<u8>,
    pub png_dpi: Sourced<u16>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticsConfig {
    pub export_ttl_hours: Sourced<u16>,
}

/// `<data_dir>/config.toml`. Derived from `data_dir`, never configured on its
/// own; the source is the source of `data_dir`.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigPath {
    pub path: Sourced<PathBuf>,
}

/// Parsed and validated configuration. `serve` reads it once at startup and
/// never re-reads the environment.
#[derive(Debug, Clone, Serialize)]
pub struct AppConfig {
    pub data_dir: Sourced<PathBuf>,
    pub config: ConfigPath,
    pub server: ServerConfig,
    pub log: LogConfig,
    pub render: RenderConfig,
    pub fetch: FetchConfig,
    pub records: RecordsConfig,
    pub preview: PreviewConfig,
    pub diagnostics: DiagnosticsConfig,
}

/// Configuration failures are local CLI errors, code `E_CONFIG`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub key: String,
    pub message: String,
}

impl ConfigError {
    fn new(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.key, self.message)
    }
}

impl std::error::Error for ConfigError {}

/// Values that only exist on the command line.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub data_dir: Option<PathBuf>,
    pub log_level: Option<String>,
    pub port: Option<u16>,
}

/// Environment snapshot. Injecting it keeps tests free of process-global state.
#[derive(Debug, Clone, Default)]
pub struct ConfigEnv {
    values: BTreeMap<String, String>,
}

impl ConfigEnv {
    /// Read the configuration variables from the current process.
    pub fn from_process() -> Self {
        let keys = [
            ENV_DATA_DIR,
            ENV_LOG,
            ENV_PORT,
            ENV_RENDER_WORKERS,
            ENV_QUEUE_MAX,
            ENV_QUEUE_TIMEOUT,
            ENV_FETCH_TIMEOUT,
            ENV_FETCH_MAX_BYTES,
        ];
        let mut values = BTreeMap::new();
        for key in keys {
            if let Ok(value) = std::env::var(key) {
                values.insert(key.to_owned(), value);
            }
        }
        Self { values }
    }

    #[cfg(test)]
    pub fn from_pairs(
        pairs: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        Self {
            values: pairs
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    fn parse<T>(&self, env_key: &str, config_key: &str) -> Result<Option<T>, ConfigError>
    where
        T: std::str::FromStr,
    {
        let Some(raw) = self.get(env_key) else {
            return Ok(None);
        };
        raw.parse()
            .map(Some)
            .map_err(|_| ConfigError::new(config_key, format!("invalid {env_key} value: {raw}")))
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    /// Never allowed in the file: the data directory decides where the file is.
    data_dir: Option<toml::Value>,
    /// Origins are managed through the control plane so they cannot bypass CSRF.
    origins: Option<toml::Value>,
    server: Option<ServerFile>,
    log: Option<LogFile>,
    retention: Option<RetentionFile>,
    render: Option<RenderFile>,
    fetch: Option<FetchFile>,
    records: Option<RecordsFile>,
    preview: Option<PreviewFile>,
    diagnostics: Option<DiagnosticsFile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerFile {
    port: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogFile {
    level: Option<LogLevel>,
    retention_days: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetentionFile {
    days: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderFile {
    workers: Option<u8>,
    queue_max: Option<u16>,
    queue_timeout_ms: Option<u32>,
    warmup: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchFile {
    timeout_ms: Option<u32>,
    max_bytes: Option<u64>,
    max_redirects: Option<u8>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordsFile {
    retention_days: Option<u16>,
    max_rows: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewFile {
    max_windows: Option<u8>,
    png_dpi: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticsFile {
    export_ttl_hours: Option<u16>,
}

impl AppConfig {
    /// Load from the real process environment and the real filesystem.
    pub fn load(cli: CliOverrides) -> Result<Self, ConfigError> {
        Self::load_with(cli, ConfigEnv::from_process())
    }

    /// Load with an injected environment, for tests.
    pub fn load_with(cli: CliOverrides, env: ConfigEnv) -> Result<Self, ConfigError> {
        let (data_dir, data_dir_source) = resolve_data_dir(&cli, &env)?;
        let config_path = data_dir.join("config.toml");
        let file = read_file(&config_path)?;
        let db = db_settings(&data_dir)?;

        let cli_log = match cli.log_level.as_deref() {
            Some(raw) => Some(raw.parse::<LogLevel>().map_err(|_| {
                ConfigError::new("log.level", format!("invalid --log value: {raw}"))
            })?),
            None => None,
        };

        let port = resolve(
            cli.port,
            env.parse::<u16>(ENV_PORT, "server.port")?,
            file.server.as_ref().and_then(|server| server.port),
            None,
            19080,
        )?;
        validate_range("server.port", port.value, 1024, 65535)?;

        let level = resolve(
            cli_log,
            env.parse::<LogLevel>(ENV_LOG, "log.level")?,
            file.log.as_ref().and_then(|log| log.level),
            db_setting::<LogLevel>(&db, "log_level", "log.level")?,
            LogLevel::Info,
        )?;

        let file_retention = file.retention.as_ref().and_then(|retention| retention.days);
        if let Some(days) = file_retention {
            validate_range("retention.days", days, 1, 3650)?;
        }
        let retention_days = resolve(
            None,
            None,
            file_retention.or_else(|| file.log.as_ref().and_then(|log| log.retention_days)),
            db_setting::<u16>(&db, "log_retention_days", "log.retention_days")?,
            90,
        )?;

        validate_range("log.retention_days", retention_days.value, 1, 3650)?;

        let default_workers = default_worker_count();
        let workers = resolve(
            None,
            env.parse::<u8>(ENV_RENDER_WORKERS, "render.workers")?,
            file.render.as_ref().and_then(|render| render.workers),
            None,
            default_workers,
        )?;
        validate_workers(workers.value)?;

        let default_queue_max = u16::from(workers.value).saturating_mul(16).max(32);
        let queue_max = resolve(
            None,
            env.parse::<u16>(ENV_QUEUE_MAX, "render.queue_max")?,
            file.render.as_ref().and_then(|render| render.queue_max),
            db_setting::<u16>(&db, "queue_max", "render.queue_max")?,
            default_queue_max,
        )?;
        validate_range("render.queue_max", queue_max.value, 1, 1024)?;

        let queue_timeout_ms = resolve(
            None,
            env.parse::<u32>(ENV_QUEUE_TIMEOUT, "render.queue_timeout_ms")?,
            file.render
                .as_ref()
                .and_then(|render| render.queue_timeout_ms),
            db_setting::<u32>(&db, "queue_timeout_ms", "render.queue_timeout_ms")?,
            10_000,
        )?;
        validate_range(
            "render.queue_timeout_ms",
            queue_timeout_ms.value,
            100,
            60_000,
        )?;

        let warmup = resolve(
            None,
            None,
            file.render.as_ref().and_then(|render| render.warmup),
            None,
            true,
        )?;

        let fetch_timeout_ms = resolve(
            None,
            env.parse::<u32>(ENV_FETCH_TIMEOUT, "fetch.timeout_ms")?,
            file.fetch.as_ref().and_then(|fetch| fetch.timeout_ms),
            None,
            10_000,
        )?;
        validate_range("fetch.timeout_ms", fetch_timeout_ms.value, 100, 60_000)?;

        let max_bytes = resolve(
            None,
            env.parse::<u64>(ENV_FETCH_MAX_BYTES, "fetch.max_bytes")?,
            file.fetch.as_ref().and_then(|fetch| fetch.max_bytes),
            None,
            printkit_facade::MAX_FETCH_BYTES as u64,
        )?;
        validate_range("fetch.max_bytes", max_bytes.value, 1, 134_217_728)?;

        let max_redirects = resolve(
            None,
            None,
            file.fetch.as_ref().and_then(|fetch| fetch.max_redirects),
            None,
            5,
        )?;
        validate_range("fetch.max_redirects", max_redirects.value, 0, 20)?;

        let records_days = resolve(
            None,
            None,
            file_retention.or_else(|| {
                file.records
                    .as_ref()
                    .and_then(|records| records.retention_days)
            }),
            db_setting::<u16>(&db, "records_retention_days", "records.retention_days")?,
            90,
        )?;
        validate_range("records.retention_days", records_days.value, 1, 3650)?;

        let max_rows = resolve(
            None,
            None,
            file.records.as_ref().and_then(|records| records.max_rows),
            db_setting::<u32>(&db, "records_max_rows", "records.max_rows")?,
            5000,
        )?;
        validate_range("records.max_rows", max_rows.value, 1, 100_000)?;

        let max_windows = resolve(
            None,
            None,
            file.preview
                .as_ref()
                .and_then(|preview| preview.max_windows),
            None,
            8,
        )?;
        validate_range("preview.max_windows", max_windows.value, 1, 8)?;

        let png_dpi = resolve(
            None,
            None,
            file.preview.as_ref().and_then(|preview| preview.png_dpi),
            None,
            150,
        )?;
        validate_range("preview.png_dpi", png_dpi.value, 72, 300)?;

        let export_ttl_hours = resolve(
            None,
            None,
            file.diagnostics
                .as_ref()
                .and_then(|diagnostics| diagnostics.export_ttl_hours),
            None,
            24,
        )?;

        validate_range(
            "diagnostics.export_ttl_hours",
            export_ttl_hours.value,
            1,
            168,
        )?;

        Ok(Self {
            data_dir: Sourced::new(data_dir, data_dir_source),
            config: ConfigPath {
                path: Sourced::new(config_path, data_dir_source),
            },
            server: ServerConfig { port },
            log: LogConfig {
                level,
                retention_days,
            },
            render: RenderConfig {
                workers,
                queue_max,
                queue_timeout_ms,
                warmup,
            },
            fetch: FetchConfig {
                timeout_ms: fetch_timeout_ms,
                max_bytes,
                max_redirects,
            },
            records: RecordsConfig {
                retention_days: records_days,
                max_rows,
            },
            preview: PreviewConfig {
                max_windows,
                png_dpi,
            },
            diagnostics: DiagnosticsConfig { export_ttl_hours },
        })
    }

    /// Flat `(key, value, source)` list for `printkit config`.
    pub fn entries(&self) -> Vec<(String, String, ConfigSource)> {
        let mut entries = Vec::new();
        entries.push((
            "data_dir".to_owned(),
            self.data_dir.value.display().to_string(),
            self.data_dir.source,
        ));
        entries.push((
            "config.path".to_owned(),
            self.config.path.value.display().to_string(),
            self.config.path.source,
        ));
        let mut push = |key: &str, value: String, source: ConfigSource| {
            entries.push((key.to_owned(), value, source));
        };
        push(
            "server.port",
            self.server.port.value.to_string(),
            self.server.port.source,
        );
        push(
            "log.level",
            level_name(self.log.level.value),
            self.log.level.source,
        );
        push(
            "log.retention_days",
            self.log.retention_days.value.to_string(),
            self.log.retention_days.source,
        );
        push(
            "render.workers",
            self.render.workers.value.to_string(),
            self.render.workers.source,
        );
        push(
            "render.queue_max",
            self.render.queue_max.value.to_string(),
            self.render.queue_max.source,
        );
        push(
            "render.queue_timeout_ms",
            self.render.queue_timeout_ms.value.to_string(),
            self.render.queue_timeout_ms.source,
        );
        push(
            "render.warmup",
            self.render.warmup.value.to_string(),
            self.render.warmup.source,
        );
        push(
            "fetch.timeout_ms",
            self.fetch.timeout_ms.value.to_string(),
            self.fetch.timeout_ms.source,
        );
        push(
            "fetch.max_bytes",
            self.fetch.max_bytes.value.to_string(),
            self.fetch.max_bytes.source,
        );
        push(
            "fetch.max_redirects",
            self.fetch.max_redirects.value.to_string(),
            self.fetch.max_redirects.source,
        );
        push(
            "records.retention_days",
            self.records.retention_days.value.to_string(),
            self.records.retention_days.source,
        );
        push(
            "records.max_rows",
            self.records.max_rows.value.to_string(),
            self.records.max_rows.source,
        );
        push(
            "preview.max_windows",
            self.preview.max_windows.value.to_string(),
            self.preview.max_windows.source,
        );
        push(
            "preview.png_dpi",
            self.preview.png_dpi.value.to_string(),
            self.preview.png_dpi.source,
        );
        push(
            "diagnostics.export_ttl_hours",
            self.diagnostics.export_ttl_hours.value.to_string(),
            self.diagnostics.export_ttl_hours.source,
        );
        entries
    }
}

fn level_name(level: LogLevel) -> String {
    match level {
        LogLevel::Off => "off",
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::UsageOnly => "usage-only",
    }
    .to_owned()
}

/// First source in priority order wins: CLI > env > file > DB > default.
fn resolve<T>(
    cli: Option<T>,
    env: Option<T>,
    file: Option<T>,
    db: Option<T>,
    default: T,
) -> Result<Sourced<T>, ConfigError> {
    if let Some(value) = cli {
        return Ok(Sourced::new(value, ConfigSource::Cli));
    }
    if let Some(value) = env {
        return Ok(Sourced::new(value, ConfigSource::Env));
    }
    if let Some(value) = file {
        return Ok(Sourced::new(value, ConfigSource::File));
    }
    if let Some(value) = db {
        return Ok(Sourced::new(value, ConfigSource::Db));
    }
    Ok(Sourced::new(default, ConfigSource::Default))
}

/// Run-time settings written by the control plane. A missing database means "no
/// DB source", not an error: a fresh install has not started `serve` yet.
#[cfg(feature = "serve")]
fn db_settings(data_dir: &Path) -> Result<BTreeMap<String, String>, ConfigError> {
    let path = data_dir.join(printkit_store::DB_FILE);
    match printkit_store::Store::read_settings(&path) {
        Ok(settings) => Ok(settings.unwrap_or_default()),
        Err(error) => Err(ConfigError::new("setting", error.to_string())),
    }
}

#[cfg(not(feature = "serve"))]
fn db_settings(_data_dir: &Path) -> Result<BTreeMap<String, String>, ConfigError> {
    Ok(BTreeMap::new())
}

fn db_setting<T>(
    settings: &BTreeMap<String, String>,
    key: &str,
    config_key: &str,
) -> Result<Option<T>, ConfigError>
where
    T: std::str::FromStr,
{
    let Some(raw) = settings.get(key) else {
        return Ok(None);
    };
    raw.parse()
        .map(Some)
        .map_err(|_| ConfigError::new(config_key, format!("invalid setting.{key} value: {raw}")))
}

/// The render pool holds one worker on macOS: `WKWebView.createPDF` runs on
/// the main thread, so a larger pool would only queue work that cannot run in
/// parallel. Windows sizes the pool from the core count up to four WebView2
/// instances; other hosts keep the wider bound.
const MAX_WORKERS: u8 = if cfg!(target_os = "macos") {
    1
} else if cfg!(windows) {
    4
} else {
    16
};

fn validate_workers(workers: u8) -> Result<(), ConfigError> {
    if workers > MAX_WORKERS {
        return Err(ConfigError::new(
            "render.workers",
            format!("render.workers must be at most {MAX_WORKERS} on this host"),
        ));
    }
    validate_range("render.workers", workers, 1, MAX_WORKERS)
}

fn validate_range<T>(key: &str, value: T, min: T, max: T) -> Result<(), ConfigError>
where
    T: PartialOrd + Copy + fmt::Display,
{
    if value < min || value > max {
        return Err(ConfigError::new(
            key,
            format!("value {value} is outside {min}..={max}"),
        ));
    }
    Ok(())
}

fn resolve_data_dir(
    cli: &CliOverrides,
    env: &ConfigEnv,
) -> Result<(PathBuf, ConfigSource), ConfigError> {
    if let Some(dir) = &cli.data_dir {
        return Ok((dir.clone(), ConfigSource::Cli));
    }
    if let Some(dir) = env.get(ENV_DATA_DIR) {
        return Ok((PathBuf::from(dir), ConfigSource::Env));
    }
    if let Some(dir) = portable_data_dir() {
        return Ok((dir, ConfigSource::File));
    }
    let dir = platform_data_dir().ok_or_else(|| {
        ConfigError::new(
            "data_dir",
            "cannot determine a data directory: set --data-dir or PRINTKIT_DATA_DIR",
        )
    })?;
    Ok((dir, ConfigSource::Default))
}

/// `portable.flag` next to the executable makes the install self-contained:
/// data lives under `<executable dir>/data`, never in the install directory.
fn portable_data_dir() -> Option<PathBuf> {
    portable_data_dir_for(&std::env::current_exe().ok()?)
}

fn portable_data_dir_for(executable: &Path) -> Option<PathBuf> {
    let directory = executable.parent()?;
    directory
        .join("portable.flag")
        .is_file()
        .then(|| directory.join("data"))
}

fn platform_data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        Some(
            home.join("Library")
                .join("Application Support")
                .join("printkit"),
        )
    }
    #[cfg(windows)]
    {
        // `%LOCALAPPDATA%` is per-user and never roamed; secrets and logs
        // belong there rather than in the roaming profile.
        Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("printkit"))
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local").join("share"));
        Some(base.join("printkit"))
    }
}

fn read_file(config_path: &Path) -> Result<FileConfig, ConfigError> {
    if !config_path.is_file() {
        return Ok(FileConfig::default());
    }
    let contents = std::fs::read_to_string(config_path).map_err(|error| {
        ConfigError::new(
            "config.toml",
            format!("cannot read {}: {error}", config_path.display()),
        )
    })?;
    let file: FileConfig = toml::from_str(&contents).map_err(|error| {
        ConfigError::new("config.toml", format!("{}: {error}", config_path.display()))
    })?;
    if file.data_dir.is_some() {
        return Err(ConfigError::new(
            "data_dir",
            "data_dir cannot be set in config.toml; use --data-dir or PRINTKIT_DATA_DIR",
        ));
    }
    if file.origins.is_some() {
        return Err(ConfigError::new(
            "origins",
            "origins are managed through /admin, not config.toml",
        ));
    }
    Ok(file)
}

fn default_worker_count() -> u8 {
    if cfg!(target_os = "macos") {
        return MAX_WORKERS;
    }
    let cores = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(2);
    #[cfg(windows)]
    {
        printkit_platform_win::render_workers(cores)
    }
    #[cfg(not(windows))]
    {
        u8::try_from((cores / 2).clamp(1, 4)).unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn load(dir: &Path, env: ConfigEnv) -> Result<AppConfig, ConfigError> {
        AppConfig::load_with(
            CliOverrides {
                data_dir: Some(dir.to_path_buf()),
                log_level: None,
                port: None,
            },
            env,
        )
    }

    #[test]
    fn defaults_are_labelled() {
        let temp = tempfile::tempdir().unwrap();
        let config = load(temp.path(), ConfigEnv::default()).unwrap();
        assert_eq!(config.data_dir.source, ConfigSource::Cli);
        assert_eq!(config.log.level.source, ConfigSource::Default);
        assert_eq!(config.log.level.value, LogLevel::Info);
        assert_eq!(config.log.retention_days.value, 90);
        assert_eq!(config.log.retention_days.source, ConfigSource::Default);
        assert_eq!(config.records.retention_days.value, 90);
        assert_eq!(config.records.retention_days.source, ConfigSource::Default);
        assert_eq!(config.render.workers.source, ConfigSource::Default);
        assert_eq!(config.server.port.value, 19080);
        assert_eq!(
            config.render.queue_max.value,
            32.max(16 * u16::from(config.render.workers.value))
        );
    }

    #[test]
    fn legacy_retention_days_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("config.toml"),
            "[log]\nretention_days = 30\n[records]\nretention_days = 120\n",
        )
        .unwrap();
        let config = load(temp.path(), ConfigEnv::default()).unwrap();
        assert_eq!(config.log.retention_days.value, 30);
        assert_eq!(config.log.retention_days.source, ConfigSource::File);
        assert_eq!(config.records.retention_days.value, 120);
        assert_eq!(config.records.retention_days.source, ConfigSource::File);
    }

    #[test]
    fn one_retention_setting_controls_both_streams() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("config.toml"),
            "[log]\nretention_days = 30\n[records]\nretention_days = 120\n[retention]\ndays = 180\n",
        )
        .unwrap();
        let config = load(temp.path(), ConfigEnv::default()).unwrap();
        assert_eq!(config.log.retention_days.value, 180);
        assert_eq!(config.log.retention_days.source, ConfigSource::File);
        assert_eq!(config.records.retention_days.value, 180);
        assert_eq!(config.records.retention_days.source, ConfigSource::File);
    }

    #[test]
    fn retention_range_rejects_zero_and_more_than_3650() {
        for days in [0, 3651] {
            let temp = tempfile::tempdir().unwrap();
            fs::write(
                temp.path().join("config.toml"),
                format!("[retention]\ndays = {days}\n"),
            )
            .unwrap();
            let error = load(temp.path(), ConfigEnv::default()).unwrap_err();
            assert_eq!(error.key, "retention.days", "{days}");
        }
    }

    #[test]
    fn environment_beats_the_file_and_reports_env() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("config.toml"),
            "[log]\nlevel = \"debug\"\n",
        )
        .unwrap();
        let config = load(temp.path(), ConfigEnv::from_pairs([(ENV_LOG, "off")])).unwrap();
        assert_eq!(config.log.level.value, LogLevel::Off);
        assert_eq!(config.log.level.source, ConfigSource::Env);
        assert_eq!(config.log.retention_days.value, 90);
    }

    #[test]
    fn file_values_are_used_when_env_is_absent() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("config.toml"),
            "[render]\nqueue_max = 40\n[server]\nport = 19081\n",
        )
        .unwrap();
        let config = load(temp.path(), ConfigEnv::default()).unwrap();
        assert_eq!(config.render.queue_max.value, 40);
        assert_eq!(config.render.queue_max.source, ConfigSource::File);
        assert_eq!(config.server.port.value, 19081);
        assert_eq!(config.server.port.source, ConfigSource::File);
    }

    #[test]
    fn workers_are_capped_at_the_host_maximum() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("config.toml"),
            format!("[render]\nworkers = {}\n", MAX_WORKERS + 1),
        )
        .unwrap();
        let error = load(temp.path(), ConfigEnv::default()).unwrap_err();
        assert_eq!(error.key, "render.workers");
    }

    #[test]
    fn cli_log_level_wins() {
        let temp = tempfile::tempdir().unwrap();
        let config = AppConfig::load_with(
            CliOverrides {
                data_dir: Some(temp.path().to_path_buf()),
                log_level: Some("debug".to_owned()),
                port: None,
            },
            ConfigEnv::from_pairs([(ENV_LOG, "off")]),
        )
        .unwrap();
        assert_eq!(config.log.level.value, LogLevel::Debug);
        assert_eq!(config.log.level.source, ConfigSource::Cli);
    }

    #[test]
    fn cli_port_wins_over_every_other_source() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("config.toml"), "[server]\nport = 19082\n").unwrap();
        let config = AppConfig::load_with(
            CliOverrides {
                data_dir: Some(temp.path().to_path_buf()),
                log_level: None,
                port: Some(19081),
            },
            ConfigEnv::from_pairs([(ENV_PORT, "19083")]),
        )
        .unwrap();
        assert_eq!(config.server.port.value, 19081);
        assert_eq!(config.server.port.source, ConfigSource::Cli);
    }

    #[cfg(feature = "serve")]
    #[test]
    fn database_setting_is_reported_as_db_source() {
        let temp = tempfile::tempdir().unwrap();
        let store = printkit_store::Store::open(temp.path()).unwrap();
        store.set_setting("log_level", "debug").unwrap();
        store.set_setting("queue_max", "48").unwrap();
        let config = load(temp.path(), ConfigEnv::default()).unwrap();
        assert_eq!(config.log.level.value, LogLevel::Debug);
        assert_eq!(config.log.level.source, ConfigSource::Db);
        assert_eq!(config.render.queue_max.value, 48);
        assert_eq!(config.render.queue_max.source, ConfigSource::Db);
    }

    #[cfg(feature = "serve")]
    #[test]
    fn file_beats_the_database() {
        let temp = tempfile::tempdir().unwrap();
        let store = printkit_store::Store::open(temp.path()).unwrap();
        store.set_setting("log_level", "debug").unwrap();
        fs::write(temp.path().join("config.toml"), "[log]\nlevel = \"off\"\n").unwrap();
        let config = load(temp.path(), ConfigEnv::default()).unwrap();
        assert_eq!(config.log.level.value, LogLevel::Off);
        assert_eq!(config.log.level.source, ConfigSource::File);
    }

    #[test]
    fn out_of_range_values_are_rejected_with_the_key() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("config.toml"), "[render]\nworkers = 32\n").unwrap();
        let error = load(temp.path(), ConfigEnv::default()).unwrap_err();
        assert_eq!(error.key, "render.workers");
    }

    #[test]
    fn data_dir_in_config_toml_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("config.toml"), "data_dir = \"/tmp/x\"\n").unwrap();
        let error = load(temp.path(), ConfigEnv::default()).unwrap_err();
        assert_eq!(error.key, "data_dir");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("config.toml"), "[render]\nunknown = 1\n").unwrap();
        let error = load(temp.path(), ConfigEnv::default()).unwrap_err();
        assert_eq!(error.key, "config.toml");
    }

    #[test]
    fn invalid_env_value_is_rejected_with_the_config_key() {
        let temp = tempfile::tempdir().unwrap();
        let error = load(
            temp.path(),
            ConfigEnv::from_pairs([(ENV_RENDER_WORKERS, "many")]),
        )
        .unwrap_err();
        assert_eq!(error.key, "render.workers");
    }

    #[test]
    fn portable_flag_places_data_in_a_data_subdirectory() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("printkit");
        fs::write(&executable, "").unwrap();
        assert_eq!(portable_data_dir_for(&executable), None);
        fs::write(temp.path().join("portable.flag"), "").unwrap();
        assert_eq!(
            portable_data_dir_for(&executable),
            Some(temp.path().join("data"))
        );
    }
}
