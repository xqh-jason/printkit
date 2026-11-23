//! `printkit origin`: list, add and remove allowed browser origins.
//!
//! The service is the only writer. Every command validates the live
//! `serve.lock` (pid, instance and executable), logs in with the data-directory
//! administrator token, and forwards the change; the token is never sent to a
//! process that did not prove it owns the data directory.

use clap::{Args, Subcommand};
use serde_json::Value;

use crate::config::{AppConfig, CliOverrides};
use crate::error::Failure;

#[derive(Debug, Args)]
pub struct OriginArgs {
    #[command(subcommand)]
    pub command: OriginCommand,
}

impl OriginArgs {
    /// Whether this invocation asked for machine-readable output.
    pub fn wants_json(&self) -> bool {
        match &self.command {
            OriginCommand::List { json }
            | OriginCommand::Add { json, .. }
            | OriginCommand::Remove { json, .. } => *json,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum OriginCommand {
    /// List the allowlisted browser origins.
    List {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Add one origin to the allowlist.
    Add {
        /// Exact browser origin, for example `https://app.example`.
        origin: String,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Remove one origin from the allowlist.
    Remove {
        /// Exact browser origin.
        origin: String,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(args: OriginArgs, overrides: CliOverrides) -> Result<(), Failure> {
    let config = AppConfig::load(overrides).map_err(Failure::from_config)?;
    match args.command {
        OriginCommand::List { json } => {
            let response = super::plugin::admin_request(&config, "GET", "/admin/origins", None)
                .map_err(super::plugin::connect_failure)?;
            check(&response)?;
            print_origins(&response.body, json)
        }
        OriginCommand::Add { origin, json } => forward(&config, "POST", &origin, json),
        OriginCommand::Remove { origin, json } => forward(&config, "DELETE", &origin, json),
    }
}

fn forward(config: &AppConfig, method: &str, origin: &str, json: bool) -> Result<(), Failure> {
    let body = serde_json::json!({ "origin": origin }).to_string();
    let response = super::plugin::admin_request(config, method, "/admin/origins", Some(&body))
        .map_err(super::plugin::connect_failure)?;
    check(&response)?;
    print_origins(&response.body, json)
}

fn check(response: &super::preview::HttpResponse) -> Result<(), Failure> {
    if response.status == 200 {
        Ok(())
    } else {
        Err(super::plugin::remote_failure(response))
    }
}

fn print_origins(body: &[u8], json: bool) -> Result<(), Failure> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| Failure::io(format!("invalid origin response: {error}")))?;
    let origins = value["origins"]
        .as_array()
        .ok_or_else(|| Failure::io("the service did not return an origin list".to_owned()))?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "origins": origins }))
                .map_err(|error| Failure::io(format!("cannot encode origins: {error}")))?
        );
        return Ok(());
    }
    if origins.is_empty() {
        println!("no origins");
    }
    for origin in origins {
        if let Some(origin) = origin.as_str() {
            println!("{origin}");
        }
    }
    Ok(())
}
