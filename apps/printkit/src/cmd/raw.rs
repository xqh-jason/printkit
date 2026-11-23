//! `printkit raw`: send device bytes to one printer without rendering.
//!
//! The payload is the device's own command language (ESC/POS, TSPL, ZPL,
//! CPCL). The command never needs a renderer: bytes go from the file to the
//! spooler unchanged.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use clap::Args;
use printkit_facade as facade;
use serde_json::json;

use crate::config::{AppConfig, CliOverrides};
use crate::error::Failure;

#[derive(Debug, Args)]
pub struct RawArgs {
    /// Raw device bytes, or `-` for stdin.
    #[arg(short = 'f', long = "file", value_name = "FILE", required = true)]
    pub file: PathBuf,

    /// Printer name; omitted means the system default.
    #[arg(short = 'p', long = "printer", value_name = "NAME")]
    pub printer: Option<String>,

    /// Title shown in the spooler queue.
    #[arg(long = "title", value_name = "TEXT", default_value = "printkit raw")]
    pub title: String,

    /// Machine-readable result on stdout.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: RawArgs, overrides: CliOverrides) -> Result<(), Failure> {
    let config = AppConfig::load(overrides).map_err(Failure::from_config)?;
    let sink = super::sink_for(&config, super::plugin_active(&config, "observability"));
    let backend = facade::new_printer_backend();
    let usage = super::usage_recorder(&config);
    let bytes = read_bytes(&args.file)?;
    let done = facade::raw_send_now(
        bytes.into(),
        backend.as_ref(),
        sink.as_ref(),
        args.printer,
        args.title,
        usage,
    )
    .map_err(Failure::from_engine)?;

    if args.json {
        println!(
            "{}",
            json!({
                "traceId": done.trace_id.as_str(),
                "jobId": done.submitted.job_id,
                "printer": done.submitted.printer,
                "bytes": done.bytes,
            })
        );
    } else if let Some(job_id) = done.submitted.job_id {
        println!(
            "submitted raw job {job_id} to {} ({} bytes, trace {})",
            done.submitted.printer, done.bytes, done.trace_id
        );
    } else {
        println!(
            "submitted raw to {} ({} bytes, trace {}; no spooler job id)",
            done.submitted.printer, done.bytes, done.trace_id
        );
    }
    Ok(())
}

/// Read exactly the bytes to send. A device file is usually binary, so this
/// never decodes text.
fn read_bytes(path: &Path) -> Result<Vec<u8>, Failure> {
    let mut bytes = Vec::new();
    if path == Path::new("-") {
        std::io::stdin()
            .read_to_end(&mut bytes)
            .map_err(|error| Failure::io(format!("cannot read stdin: {error}")))?;
    } else {
        bytes = std::fs::read(path)
            .map_err(|error| Failure::io(format!("cannot read {}: {error}", path.display())))?;
    }
    if bytes.is_empty() {
        return Err(Failure::usage("the raw payload is empty"));
    }
    Ok(bytes)
}
