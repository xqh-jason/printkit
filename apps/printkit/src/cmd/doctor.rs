//! `printkit doctor`: the first command to run when nothing prints.

use std::fs;
use std::path::Path;

use clap::Args;
use printkit_facade as facade;
#[cfg(feature = "serve")]
use printkit_store::Store;
use serde_json::{Map, Value, json};

use crate::config::{AppConfig, CliOverrides};
use crate::error::Failure;

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Machine-readable report on stdout.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: DoctorArgs, overrides: CliOverrides) -> Result<(), Failure> {
    let config = AppConfig::load(overrides).map_err(Failure::from_config)?;
    let report = report(&config)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|error| Failure::io(format!("cannot encode report: {error}")))?
        );
    } else {
        let data_dir = &config.data_dir.value;
        let data_dir_writable = report["data_dir"]["writable"].as_bool().unwrap_or(false);
        let logs_dir = report["logs"]["dir"].as_str().unwrap_or("");
        let logs_writable = report["logs"]["writable"].as_bool().unwrap_or(false);
        println!("printkit {}", env!("CARGO_PKG_VERSION"));
        println!(
            "webview        {} {}",
            report["webview"]["engine"].as_str().unwrap_or("unknown"),
            report["webview"]["version"]
                .as_str()
                .unwrap_or("unknown version")
        );
        println!(
            "data_dir       {} ({})",
            data_dir.display(),
            if data_dir_writable {
                "writable"
            } else {
                "NOT writable"
            }
        );
        println!(
            "config.path    {}",
            report["config"]["path"].as_str().unwrap_or("")
        );
        println!(
            "logs           {logs_dir} ({}, {})",
            config.log.level.value,
            if logs_writable {
                "writable"
            } else {
                "NOT writable"
            }
        );
        println!("network        http, https (outbound only for data.url)");
        println!(
            "records        {} row(s), oldest {}",
            report["records"]["count"].as_i64().unwrap_or(0),
            report["records"]["earliest"].as_str().unwrap_or("none")
        );
    }

    let data_dir_writable = report["data_dir"]["writable"].as_bool().unwrap_or(false);
    let logs_writable = report["logs"]["writable"].as_bool().unwrap_or(false);
    if !data_dir_writable || !logs_writable {
        return Err(Failure::environment(
            "the data directory is not writable; printing cannot be traced",
        ));
    }
    Ok(())
}

/// The single doctor report. `logs.export` stores the same document next to
/// the events it exports, and the text output is only a summary of it.
pub fn report(config: &AppConfig) -> Result<Value, Failure> {
    let data_dir = &config.data_dir.value;
    let webview = facade::webview_info();
    let logs_dir = facade::logs_dir(data_dir);
    let logs_writable = probe_writable(&logs_dir);
    let data_dir_writable = probe_writable(data_dir);
    let events = facade::read_events(&logs_dir).map(|events| events.len());

    let mut sources = Map::new();
    for (key, _value, source) in config.entries() {
        sources.insert(key, Value::String(source.as_str().to_owned()));
    }

    let records = records_report(data_dir);

    Ok(json!({
        "webview": {
            "engine": webview.engine,
            "available": webview.available,
            "version": webview.version,
        },
        "data_dir": {
            "path": data_dir.display().to_string(),
            "writable": data_dir_writable,
        },
        "config": {
            "path": config.config.path.value.display().to_string(),
            "sources": sources,
        },
        "logs": {
            "dir": logs_dir.display().to_string(),
            "level": config.log.level.value,
            "writable": logs_writable,
            "events": events.ok(),
        },
        "network": {
            "allowlist": ["http", "https"],
        },
        "records": records,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[cfg(feature = "serve")]
fn records_report(data_dir: &Path) -> Value {
    let path = data_dir.join(printkit_store::DB_FILE);
    match Store::open_existing(&path) {
        Ok(Some(store)) => match store.print_record_count() {
            Ok(count) => json!({ "count": count.count, "earliest": count.earliest }),
            Err(_) => json!({ "count": null, "earliest": null }),
        },
        Ok(None) => json!({ "count": 0, "earliest": null }),
        Err(_) => json!({ "count": null, "earliest": null }),
    }
}

#[cfg(not(feature = "serve"))]
fn records_report(_data_dir: &Path) -> Value {
    json!({ "count": null, "earliest": null })
}

/// Create the directory and write a probe file into it, removing it again.
fn probe_writable(directory: &Path) -> bool {
    if fs::create_dir_all(directory).is_err() {
        return false;
    }
    let probe = directory.join(".printkit-write-probe");
    match fs::write(&probe, b"") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}
