//! `printkit version`.

use clap::Args;
use serde_json::json;

use crate::error::Failure;

#[derive(Debug, Args)]
pub struct VersionArgs {
    /// Machine-readable version on stdout.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: VersionArgs) -> Result<(), Failure> {
    if args.json {
        println!(
            "{}",
            json!({
                "name": "printkit",
                "version": env!("CARGO_PKG_VERSION"),
                "target": std::env::consts::OS,
            })
        );
    } else {
        println!("printkit {}", env!("CARGO_PKG_VERSION"));
    }
    Ok(())
}
