//! `printkit printers`: list the printers the operating system reports.

use clap::Args;
use printkit_facade as facade;
use serde_json::json;

use crate::config::CliOverrides;
use crate::error::Failure;

#[derive(Debug, Args)]
pub struct PrintersArgs {
    /// Machine-readable result on stdout.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: PrintersArgs, _overrides: CliOverrides) -> Result<(), Failure> {
    let backend = facade::new_printer_backend();
    let printers = backend.printers().map_err(Failure::from_platform)?;
    if args.json {
        let listed: Vec<_> = printers
            .iter()
            .map(|printer| {
                json!({
                    "name": printer.name,
                    "isDefault": printer.is_default,
                    "raw": printer.raw,
                    "mimeTypes": printer.mime_types,
                    "duplex": printer.duplex,
                    "paperSource": printer.paper_source,
                })
            })
            .collect();
        println!("{}", json!(listed));
    } else if printers.is_empty() {
        println!("no printers are configured");
    } else {
        for printer in &printers {
            println!(
                "{}{}",
                if printer.is_default { "* " } else { "  " },
                printer.name
            );
        }
    }
    Ok(())
}
