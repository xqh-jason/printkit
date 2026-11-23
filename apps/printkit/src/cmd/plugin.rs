//! `printkit plugin`: list and change built-in plugin state.
//!
//! The service is the only writer. `enable`, `disable` and `config` forward to
//! the loopback administrator API; when the service is not running the command
//! fails with `E_CONNECT` and never rewrites `plugins.toml` itself. `repair` is
//! the single offline exception: it archives a broken file and rebuilds the
//! defaults while holding `plugins.lock`.

use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, Subcommand};
use printkit_plugin_host::{PluginLock, PluginsFile, Preset, builtins, load, save};
use serde_json::Value;

use super::preview::http;
use crate::config::{AppConfig, CliOverrides};
use crate::error::Failure;

#[derive(Debug, Args)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub command: PluginCommand,
}

impl PluginArgs {
    /// Whether this invocation asked for machine-readable output.
    pub fn wants_json(&self) -> bool {
        match &self.command {
            PluginCommand::List { json }
            | PluginCommand::Enable { json, .. }
            | PluginCommand::Disable { json, .. }
            | PluginCommand::Config { json, .. }
            | PluginCommand::Repair { json, .. } => *json,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// List every built-in plugin and its state.
    List {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Enable one plugin through the running service.
    Enable {
        /// Plugin id.
        id: String,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Disable one plugin through the running service.
    Disable {
        /// Plugin id.
        id: String,
        /// Confirm losing the page while historical rows stay on disk.
        #[arg(long)]
        confirm_data_count: bool,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Replace one plugin's configuration.
    Config {
        /// Plugin id.
        id: String,
        /// JSON object.
        #[arg(long, value_name = "JSON")]
        config: String,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Archive a broken plugins.toml and rebuild the defaults (service stopped).
    Repair {
        /// Required: repair discards the current file.
        #[arg(long)]
        force: bool,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(args: PluginArgs, overrides: CliOverrides) -> Result<(), Failure> {
    let config = AppConfig::load(overrides).map_err(Failure::from_config)?;
    match args.command {
        PluginCommand::List { json } => list(&config, json),
        PluginCommand::Enable { id, json } => forward(&config, &id, Some(true), None, false, json),
        PluginCommand::Disable {
            id,
            confirm_data_count,
            json,
        } => forward(&config, &id, Some(false), None, confirm_data_count, json),
        PluginCommand::Config {
            id,
            config: raw,
            json,
        } => {
            let value: Value = serde_json::from_str(&raw)
                .map_err(|error| Failure::usage(format!("invalid JSON config: {error}")))?;
            forward(&config, &id, None, Some(value), false, json)
        }
        PluginCommand::Repair { force, json } => repair(&config, force, json),
    }
}

fn list(config: &AppConfig, json: bool) -> Result<(), Failure> {
    let loaded = load(&config.data_dir.value);
    let registry = loaded.registry(builtins());
    if json {
        let plugins: Vec<Value> = registry
            .statuses
            .iter()
            .map(|status| {
                serde_json::json!({
                    "id": status.id.as_str(),
                    "state": status.state.as_str(),
                    "chosen": status.chosen,
                    "source": status.source.as_str(),
                    "why": status.reason,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "preset": if loaded.file.preset == Preset::All { "all" } else { "core" },
                "broken": loaded.broken,
                "plugins": plugins,
            }))
            .map_err(|error| Failure::io(format!("cannot encode plugins: {error}")))?
        );
        return Ok(());
    }
    if let Some(reason) = &loaded.broken {
        eprintln!("warning: plugins.toml is broken: {reason}");
    }
    println!(
        "{:<16} {:<12} {:<8} {:<8}",
        "id", "state", "source", "chosen"
    );
    for status in &registry.statuses {
        let source = status.source.as_str();
        println!(
            "{:<16} {:<12} {:<8} {:<8}",
            status.id.as_str(),
            status.state.as_str(),
            source,
            if status.chosen { "yes" } else { "no" }
        );
        if let Some(reason) = &status.reason {
            println!("  {reason}");
        }
    }
    Ok(())
}

fn forward(
    config: &AppConfig,
    id: &str,
    on: Option<bool>,
    config_value: Option<Value>,
    confirm_data_count: bool,
    json: bool,
) -> Result<(), Failure> {
    let body = serde_json::json!({
        "id": id,
        "on": on,
        "config": config_value,
        "confirmDataCount": confirm_data_count,
    })
    .to_string();
    let response = admin_post(config, "/admin/plugins", &body).map_err(connect_failure)?;
    if response.status != 200 {
        return Err(remote_failure(&response));
    }
    let value: Value = serde_json::from_slice(&response.body)
        .map_err(|error| Failure::io(format!("invalid plugin response: {error}")))?;
    print_result(value, json)
}

fn repair(config: &AppConfig, force: bool, json: bool) -> Result<(), Failure> {
    if !force {
        return Err(Failure::usage(
            "repair discards the current file; pass --force",
        ));
    }
    let data_dir = &config.data_dir.value;
    let lock = PluginLock::try_acquire(data_dir)
        .map_err(|error| Failure::io(format!("cannot acquire plugins.lock: {error}")))?;
    let Some(_lock) = lock else {
        return Err(Failure::runtime(
            "E_ALREADY_RUNNING",
            "the service holds plugins.lock; stop it before repairing",
            3,
        ));
    };
    let path = data_dir.join(printkit_plugin_host::PLUGINS_FILE);
    if path.exists() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let archive = data_dir.join(format!("plugins.toml.broken.{stamp}"));
        std::fs::rename(&path, &archive)
            .map_err(|error| Failure::io(format!("cannot archive plugins.toml: {error}")))?;
    }
    save(data_dir, &PluginsFile::default())
        .map_err(|error| Failure::io(format!("cannot write plugins.toml: {error}")))?;
    if json {
        println!("{}", serde_json::json!({ "ok": true, "preset": "all" }));
    } else {
        println!("plugins.toml rebuilt with preset all");
    }
    Ok(())
}

fn print_result(value: Value, json: bool) -> Result<(), Failure> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&value)
                .map_err(|error| Failure::io(format!("cannot encode plugins: {error}")))?
        );
        return Ok(());
    }
    for plugin in value["plugins"].as_array().into_iter().flatten() {
        let id = plugin["id"].as_str().unwrap_or("?");
        let state = plugin["state"].as_str().unwrap_or("?");
        let in_use = plugin["inUse"].as_u64().unwrap_or(0);
        let data = plugin["dataCount"].as_u64().unwrap_or(0);
        println!("{id}: {state} (inUse {in_use}, data {data})");
        if let Some(why) = plugin["why"].as_str() {
            println!("  {why}");
        }
    }
    Ok(())
}

pub(crate) fn connect_failure(message: String) -> Failure {
    Failure::runtime(
        "E_CONNECT",
        format!("cannot forward to a running serve: {message}; start `printkit serve` first"),
        3,
    )
}

pub(crate) fn remote_failure(response: &super::preview::HttpResponse) -> Failure {
    let value: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    let code = value["error"]["code"].as_str().unwrap_or("E_CONNECT");
    let message = value["error"]["message"]
        .as_str()
        .unwrap_or("the service refused the plugin change")
        .to_owned();
    let detail = value["error"]["detail"].clone();
    let exit_code = match code {
        "E_CONNECT" => 3,
        "E_CONFIG" => 1,
        "E_SCHEMA" => 2,
        "E_UNSUPPORTED" => 4,
        _ => 3,
    };
    Failure {
        code: match code {
            "E_NOT_FOUND" => "E_NOT_FOUND",
            "E_CONFIG" => "E_CONFIG",
            "E_SCHEMA" => "E_SCHEMA",
            "E_UNSUPPORTED" => "E_UNSUPPORTED",
            _ => "E_JOB_FAILED",
        },
        message,
        detail: Some(detail),
        exit_code,
    }
}

/// The running service's lock, as written by `printkit serve`.
#[derive(Debug, serde::Deserialize)]
struct ServeLockInfo {
    pid: u32,
    port: u16,
    #[serde(default)]
    instance: String,
    #[serde(default)]
    executable: String,
    #[serde(default)]
    started: String,
}

/// Read `serve.lock` and accept it only when it names a live printkit process
/// with a real instance, a printkit executable name and a matching pid start
/// time. The configured `server.port` is a preference, not proof that a
/// service is listening; sending the admin token there could reach another
/// process.
pub(crate) fn live_lock_port(data_dir: &std::path::Path) -> Result<u16, String> {
    let path = data_dir.join("serve.lock");
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let info: ServeLockInfo = serde_json::from_str(&text)
        .map_err(|error| format!("{} is not a valid serve.lock: {error}", path.display()))?;
    if info.instance.is_empty() || info.instance == "unknown" {
        return Err("serve.lock has no instance id; refusing to send credentials".to_owned());
    }
    if !is_printkit_name(&info.executable) {
        return Err(format!(
            "serve.lock names a non-printkit executable: {}",
            info.executable
        ));
    }
    if !process_name_matches(info.pid) {
        return Err(format!(
            "serve.lock pid {} is not a running printkit process",
            info.pid
        ));
    }
    if !start_time_matches(info.pid, &info.started) {
        return Err(format!(
            "serve.lock pid {} start time does not match; the pid may have been reused",
            info.pid
        ));
    }
    Ok(info.port)
}

fn is_printkit_name(path: &str) -> bool {
    let Some(name) = std::path::Path::new(path.trim())
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

fn process_name_matches(pid: u32) -> bool {
    #[cfg(unix)]
    let command = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output();
    #[cfg(windows)]
    let command = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output();
    #[cfg(all(not(unix), not(windows)))]
    let command: Result<std::process::Output, std::io::Error> =
        Err(std::io::Error::other("unsupported host"));

    let Ok(output) = command else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let name = text
        .lines()
        .next()
        .unwrap_or_default()
        .split(',')
        .next()
        .unwrap_or_default()
        .trim()
        .trim_matches('"');
    is_printkit_name(name)
}

fn start_time_matches(pid: u32, started: &str) -> bool {
    let Ok(started) = started.trim().parse::<u64>() else {
        return false;
    };
    let Some(actual) = process_start_secs(pid) else {
        return false;
    };
    started.abs_diff(actual) <= 120
}

#[cfg(unix)]
fn process_start_secs(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "etime="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let elapsed = elapsed_from_etime(&String::from_utf8_lossy(&output.stdout))?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(now.saturating_sub(elapsed))
}

/// BSD and GNU `ps etime` share `[[days-]hh:]mm:ss`; GNU's `etimes` is not
/// available on macOS, so parse the portable spelling.
#[cfg(unix)]
fn elapsed_from_etime(text: &str) -> Option<u64> {
    let text = text.trim();
    let (days, rest) = match text.split_once('-') {
        Some((days, rest)) => (days.trim().parse::<u64>().ok()?, rest),
        None => (0, text),
    };
    let parts = rest
        .split(':')
        .map(|part| part.trim().parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    let seconds = match parts.as_slice() {
        [minutes, seconds] => minutes * 60 + seconds,
        [hours, minutes, seconds] => hours * 3600 + minutes * 60 + seconds,
        _ => return None,
    };
    Some(days * 86_400 + seconds)
}

#[cfg(windows)]
fn process_start_secs(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "$p=Get-Process -Id {pid} -ErrorAction Stop; [int64](($p.StartTime.ToUniversalTime() - [datetime]'1970-01-01').TotalSeconds)"
            ),
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(all(not(unix), not(windows)))]
fn process_start_secs(_pid: u32) -> Option<u64> {
    None
}

/// GET `/admin/session`, login, then POST one JSON body.
fn admin_post(
    config: &AppConfig,
    path: &str,
    body: &str,
) -> Result<super::preview::HttpResponse, String> {
    admin_request(config, "POST", path, Some(body))
}

/// Validate the live `serve.lock`, log in with the data-directory token, then
/// send one authenticated request. The token never leaves this process.
pub(crate) fn admin_request(
    config: &AppConfig,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<super::preview::HttpResponse, String> {
    let port = live_lock_port(&config.data_dir.value)?;
    let admin = std::fs::read_to_string(config.data_dir.value.join("admin-token"))
        .map_err(|error| format!("cannot read the administrator token: {error}"))?;
    let origin = format!("http://127.0.0.1:{port}");
    let page = http(port, "GET", "/admin/session", &[], None)?;
    let csrf = page
        .cookie("pk_csrf")
        .ok_or_else(|| "the admin session set no CSRF cookie".to_owned())?;
    let login_body = serde_json::json!({ "token": admin.trim() }).to_string();
    let csrf_cookie = format!("pk_csrf={csrf}");
    let login = http(
        port,
        "POST",
        "/admin/login",
        &[
            ("Origin", &origin),
            ("Cookie", &csrf_cookie),
            ("X-CSRF-Token", &csrf),
        ],
        Some(&login_body),
    )?;
    if login.status != 200 {
        return Err(format!("the administrator login returned {}", login.status));
    }
    let session = login
        .cookie("pk_admin")
        .ok_or_else(|| "the administrator login set no session cookie".to_owned())?;
    let cookies = format!("pk_admin={session}; pk_csrf={csrf}");
    http(
        port,
        method,
        path,
        &[
            ("Origin", &origin),
            ("Cookie", &cookies),
            ("X-CSRF-Token", &csrf),
        ],
        body,
    )
}

#[cfg(test)]
mod lock_tests {
    use std::fs;

    use super::*;

    fn write_lock(dir: &std::path::Path, instance: &str, executable: &str, started: u64) {
        let text = serde_json::json!({
            "pid": std::process::id(),
            "port": 19080,
            "instance": instance,
            "executable": executable,
            "started": started.to_string(),
        })
        .to_string();
        fs::write(dir.join("serve.lock"), text).unwrap();
    }

    fn own_started() -> u64 {
        process_start_secs(std::process::id()).expect("own start time")
    }

    fn own_exe() -> String {
        std::env::current_exe().unwrap().display().to_string()
    }

    #[test]
    fn accepts_a_live_matching_lock() {
        let dir = tempfile::tempdir().unwrap();
        write_lock(dir.path(), "pk_run", &own_exe(), own_started());
        assert_eq!(live_lock_port(dir.path()), Ok(19080));
    }

    #[test]
    fn rejects_a_missing_or_unknown_instance() {
        let dir = tempfile::tempdir().unwrap();
        write_lock(dir.path(), "", &own_exe(), own_started());
        assert!(live_lock_port(dir.path()).is_err());
        write_lock(dir.path(), "unknown", &own_exe(), own_started());
        assert!(live_lock_port(dir.path()).is_err());
    }

    #[test]
    fn rejects_a_foreign_executable() {
        let dir = tempfile::tempdir().unwrap();
        write_lock(dir.path(), "pk_run", "/bin/sh", own_started());
        assert!(live_lock_port(dir.path()).is_err());
    }

    #[test]
    fn rejects_a_reused_pid_start_time() {
        let dir = tempfile::tempdir().unwrap();
        write_lock(dir.path(), "pk_run", &own_exe(), own_started() + 3600);
        assert!(live_lock_port(dir.path()).is_err());
    }

    #[test]
    fn rejects_a_missing_lock() {
        let dir = tempfile::tempdir().unwrap();
        assert!(live_lock_port(dir.path()).is_err());
    }
}
