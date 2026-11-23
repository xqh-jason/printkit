//! `printkit config`: effective values, their sources and the config path.

use clap::Args;
use serde_json::json;

use crate::config::{AppConfig, CliOverrides};
use crate::error::Failure;

#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// Machine-readable configuration with a source per value.
    #[arg(long)]
    pub json: bool,

    /// Print only the path of config.toml.
    #[arg(long)]
    pub path: bool,

    /// Annotate every text line with its source.
    #[arg(long = "show-source")]
    pub show_source: bool,

    /// Validate the configuration and exit without starting anything.
    #[arg(long)]
    pub validate: bool,
}

pub fn run(args: ConfigArgs, overrides: CliOverrides) -> Result<(), Failure> {
    let config = AppConfig::load(overrides).map_err(Failure::from_config)?;

    if args.path {
        println!("{}", config.config.path.value.display());
        return Ok(());
    }
    if args.validate {
        if args.json {
            println!(
                "{}",
                json!({
                    "ok": true,
                    "config": config.config.path.value.display().to_string(),
                })
            );
        } else {
            println!("configuration ok: {}", config.config.path.value.display());
        }
        return Ok(());
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&config)
                .map_err(|error| Failure::io(format!("cannot encode configuration: {error}")))?
        );
        return Ok(());
    }

    for (key, value, source) in config.entries() {
        if args.show_source {
            println!("{key} = {value} ({})", source.as_str());
        } else {
            println!("{key} = {value}");
        }
    }
    Ok(())
}
