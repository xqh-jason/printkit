#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used))]

//! printkit CLI.
//!
//! Command functions are thin: parse arguments, call the facade, print. All
//! configuration parsing lives in [`config`], all job logic behind the facade.

mod cmd;
mod config;
mod error;
#[cfg(feature = "serve")]
mod service;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use cmd::config_cmd::ConfigArgs;
use cmd::doctor::DoctorArgs;
use cmd::logs::LogsArgs;
use cmd::papers::PapersArgs;
use cmd::pdf::PdfArgs;
#[cfg(feature = "serve")]
use cmd::plugin::PluginArgs;
use cmd::print::PrintArgs;
use cmd::printers::PrintersArgs;
use cmd::raw::RawArgs;
use cmd::schema::SchemaArgs;
#[cfg(feature = "serve")]
use cmd::stats::StatsArgs;
use cmd::version::VersionArgs;

use crate::config::CliOverrides;

#[derive(Debug, Parser)]
#[command(
    name = "printkit",
    version,
    about = "Declarative documents to PDF, without a printer",
    long_about = None
)]
struct Cli {
    /// Data directory; beats PRINTKIT_DATA_DIR and the platform default.
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,
    /// TCP port of the local service; beats PRINTKIT_PORT and config.toml.
    #[arg(long, global = true, value_name = "PORT")]
    port: Option<u16>,

    /// Log level: off, debug, info or usage-only.
    #[arg(long, global = true, value_name = "LEVEL")]
    log: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Render a document to a PDF file without a printer.
    Pdf(PdfArgs),
    /// Check the environment: renderer, data directory, configuration, logs.
    Doctor(DoctorArgs),
    /// Print a document or an existing PDF through the operating-system print system.
    Print(PrintArgs),
    /// List configured printers.
    Printers(PrintersArgs),
    /// Send device bytes (ESC/POS, TSPL, ZPL, CPCL) to one printer unchanged.
    Raw(RawArgs),
    /// List the paper sizes one printer accepts.
    Papers(PapersArgs),
    /// Read local JSONL trace events.
    Logs(LogsArgs),
    /// Aggregate the local usage table.
    #[cfg(feature = "serve")]
    Stats(StatsArgs),
    /// Print the effective configuration and where every value came from.
    Config(ConfigArgs),
    /// Write the published JSON Schema contracts.
    Schema(SchemaArgs),
    /// Open one document in a native preview window.
    #[cfg(feature = "serve")]
    Preview(cmd::preview::PreviewArgs),
    /// Run the local HTTP/WebSocket service.
    #[cfg(feature = "serve")]
    Serve(cmd::serve::ServeArgs),
    /// List and change the allowlisted browser origins.
    #[cfg(feature = "serve")]
    Origin(cmd::origin::OriginArgs),
    /// List and change built-in plugins.
    #[cfg(feature = "serve")]
    #[command(name = "plugin", alias = "plugins")]
    Plugin(PluginArgs),
    /// Print the version.
    Version(VersionArgs),
}

impl Command {
    fn wants_json(&self) -> bool {
        match self {
            Self::Pdf(args) => args.json,
            Self::Print(args) => args.json,
            Self::Printers(args) => args.json,
            Self::Raw(args) => args.json,
            Self::Papers(args) => args.json,
            Self::Doctor(args) => args.json,
            Self::Logs(args) => args.json,
            #[cfg(feature = "serve")]
            Self::Stats(args) => args.json,
            Self::Config(args) => args.json,
            Self::Schema(_) => false,
            #[cfg(feature = "serve")]
            Self::Preview(args) => args.json,
            #[cfg(feature = "serve")]
            Self::Serve(_) => false,
            #[cfg(feature = "serve")]
            Self::Origin(args) => args.wants_json(),
            #[cfg(feature = "serve")]
            Self::Plugin(args) => args.wants_json(),
            Self::Version(args) => args.json,
        }
    }
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            use clap::error::ErrorKind;
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                let _ = error.print();
                return ExitCode::SUCCESS;
            }
            // Parsing failed before a command exists, so `--json` can only be
            // read from the raw arguments.
            let json = std::env::args().any(|arg| arg == "--json");
            if json {
                error::Failure::usage(error.to_string().trim()).report(true);
            } else {
                let _ = error.print();
            }
            return ExitCode::from(1);
        }
    };
    let wants_json = cli.command.wants_json();
    let overrides = CliOverrides {
        data_dir: cli.data_dir,
        log_level: cli.log,
        port: cli.port,
    };
    match cmd::run(cli.command, overrides) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            failure.report(wants_json);
            ExitCode::from(failure.exit_code)
        }
    }
}
