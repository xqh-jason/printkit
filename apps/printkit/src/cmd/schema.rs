//! `printkit schema`: write the two published JSON Schema contracts.

use std::path::{Path, PathBuf};

use clap::Args;
use printkit_schema as schema;

use crate::error::Failure;

#[derive(Debug, Args)]
pub struct SchemaArgs {
    /// Directory to write `document-v1.json` and `protocol-v1.json` into.
    #[arg(long, value_name = "DIR", conflicts_with = "path")]
    pub out: Option<PathBuf>,

    /// Print the directory that supplies the published contracts.
    #[arg(long)]
    pub path: bool,
}

pub fn run(args: SchemaArgs) -> Result<(), Failure> {
    if args.path {
        let directory =
            schema::published_dir().map_err(|error| Failure::usage(error.to_string()))?;
        println!("{}", directory.display());
        return Ok(());
    }
    let Some(out) = args.out else {
        return Err(Failure::usage("schema expects --out DIR or --path"));
    };
    let written = write(&out)?;
    for path in written {
        println!("{}", path.display());
    }
    Ok(())
}

fn write(dir: &Path) -> Result<Vec<PathBuf>, Failure> {
    schema::write_to(dir).map_err(|error| Failure::io(error.to_string()))
}
