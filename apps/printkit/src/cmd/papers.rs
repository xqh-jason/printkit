//! `printkit papers`: list the paper sizes one printer accepts.

use clap::Args;
use printkit_facade as facade;
use serde_json::json;

use crate::config::CliOverrides;
use crate::error::Failure;

#[derive(Debug, Args)]
pub struct PapersArgs {
    /// Printer name.
    #[arg(short = 'p', long = "printer", value_name = "NAME")]
    pub printer: String,

    /// Machine-readable result on stdout.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: PapersArgs, _overrides: CliOverrides) -> Result<(), Failure> {
    let backend = facade::new_printer_backend();
    let papers = backend
        .papers(&args.printer)
        .map_err(Failure::from_platform)?;
    if args.json {
        let listed: Vec<_> = papers
            .iter()
            .map(|paper| {
                json!({
                    "name": paper.name,
                    "widthMm": paper.width_mm,
                    "heightMm": paper.height_mm,
                    "kind": if paper.named { "named" } else { "custom" },
                })
            })
            .collect();
        println!("{}", json!(listed));
    } else if papers.is_empty() {
        println!("{} reports no paper sizes", args.printer);
    } else {
        for paper in &papers {
            println!(
                "{:<24} {:>7.1} x {:>7.1} mm  {}",
                paper.name,
                paper.width_mm,
                paper.height_mm,
                if paper.named { "named" } else { "custom" }
            );
        }
    }
    Ok(())
}
