//! `printkit pdf`: document in, PDF file out. No printer is touched.

use std::fs;
use std::io::Read;
use std::path::PathBuf;

use clap::Args;
use printkit_facade as facade;
use serde_json::json;

use crate::config::{AppConfig, CliOverrides};
use crate::error::Failure;

#[derive(Debug, Args)]
pub struct PdfArgs {
    /// Input document JSON file, or `-` for stdin.
    #[arg(short = 'i', long = "input", value_name = "FILE")]
    pub input: PathBuf,

    /// Output PDF file, or `-` for stdout.
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    pub output: PathBuf,

    /// Machine-readable result on stdout.
    #[arg(long)]
    pub json: bool,

    /// Also write the intermediate HTML to this file.
    #[arg(long = "dump-html", value_name = "FILE")]
    pub dump_html: Option<PathBuf>,
}

pub fn run(args: PdfArgs, overrides: CliOverrides) -> Result<(), Failure> {
    let config = AppConfig::load(overrides).map_err(Failure::from_config)?;

    let webview = facade::webview_info();
    if !webview.available {
        return Err(Failure::environment(format!(
            "{} renderer is not available",
            webview.engine
        )));
    }

    let fetch = facade::FetchPolicy {
        timeout_ms: config.fetch.timeout_ms.value,
        max_bytes: config.fetch.max_bytes.value,
        max_redirects: config.fetch.max_redirects.value,
        allow_private_networks: super::allow_private_fetch(),
    };
    let input = read_input(&args.input)?;
    let sink = super::sink_for(&config, super::plugin_active(&config, "observability"));
    #[cfg(debug_assertions)]
    super::test_hook_before_render();
    let result = facade::export_pdf(&input, sink.as_ref(), &fetch).map_err(Failure::from_engine)?;

    if let Some(path) = &args.dump_html {
        fs::write(path, &result.html)
            .map_err(|error| Failure::io(format!("cannot write {}: {error}", path.display())))?;
    }
    write_output(&args.output, &result.pdf)?;

    for warning in &result.warnings {
        eprintln!("warning: {} {}", warning.code, warning.message);
    }

    // `-o -` puts PDF bytes on stdout; the summary must not corrupt them.
    let summary_to_stderr = args.output == std::path::Path::new("-");
    let summary = if args.json {
        let warnings: Vec<_> = result
            .warnings
            .iter()
            .map(|warning| json!({ "code": warning.code, "message": warning.message }))
            .collect();
        json!({
            "traceId": result.trace_id.as_str(),
            "output": args.output.display().to_string(),
            "pages": result.pages,
            "warnings": warnings,
        })
        .to_string()
    } else {
        format!(
            "wrote {} ({} page, trace {})",
            args.output.display(),
            result.pages,
            result.trace_id
        )
    };
    if summary_to_stderr {
        eprintln!("{summary}");
    } else {
        println!("{summary}");
    }
    Ok(())
}

fn read_input(path: &std::path::Path) -> Result<Vec<u8>, Failure> {
    if path == std::path::Path::new("-") {
        let mut buffer = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buffer)
            .map_err(|error| Failure::io(format!("cannot read stdin: {error}")))?;
        return Ok(buffer);
    }
    fs::read(path).map_err(|error| Failure::io(format!("cannot read {}: {error}", path.display())))
}

fn write_output(path: &std::path::Path, pdf: &[u8]) -> Result<(), Failure> {
    if path == std::path::Path::new("-") {
        use std::io::Write as _;
        std::io::stdout()
            .write_all(pdf)
            .map_err(|error| Failure::io(format!("cannot write stdout: {error}")))?;
        return Ok(());
    }
    fs::write(path, pdf)
        .map_err(|error| Failure::io(format!("cannot write {}: {error}", path.display())))
}
