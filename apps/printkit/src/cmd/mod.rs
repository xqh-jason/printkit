//! Subcommand implementations.

pub mod config_cmd;
pub mod doctor;
pub mod logs;
#[cfg(feature = "serve")]
pub mod origin;
pub mod papers;
pub mod pdf;
#[cfg(feature = "serve")]
pub mod plugin;
#[cfg(feature = "serve")]
pub mod preview;
pub mod print;
pub mod printers;
pub mod raw;
pub mod schema;
#[cfg(feature = "serve")]
pub mod serve;
#[cfg(feature = "serve")]
pub mod stats;
pub mod version;

use printkit_facade as facade;

use crate::Command;
use crate::config::{AppConfig, CliOverrides};
use crate::error::Failure;

/// Open the statistics database for a one-shot print or raw submission.
///
/// The CLI writes the same `print_record` rows as the service. Statistics are
/// a side channel, so an unusable database must warn rather than fail the
/// print that already reached the spooler.
pub(crate) fn usage_recorder(config: &AppConfig) -> Option<facade::UsageRecorder> {
    if !config.log.level.value.writes_usage() || !plugin_active(config, "records") {
        return None;
    }
    match facade::UsageRecorder::open(
        &config.data_dir.value,
        "cli".to_owned(),
        config.records.retention_days.value,
        config.records.max_rows.value,
    ) {
        Ok(usage) => Some(usage),
        Err(error) => {
            eprintln!("warning: cannot open the statistics database: {error}");
            None
        }
    }
}

/// The sink a one-shot CLI command writes through.
///
/// Observability off means `NullSink`: no JSONL, no seed, no rotation. A
/// one-shot process has no log continuation to protect.
pub(crate) fn sink_for(config: &AppConfig, observability: bool) -> Box<dyn facade::Sink> {
    if observability && config.log.level.value.writes_events() {
        Box::new(facade::JsonlSink::with_retention(
            facade::logs_dir(&config.data_dir.value),
            config.log.level.value,
            config.log.retention_days.value,
        ))
    } else {
        Box::new(facade::NullSink)
    }
}

/// Whether a plugin is active in the on-disk `plugins.toml`.
///
/// A broken file means the core-only fallback, matching the service.
pub(crate) fn plugin_active(config: &AppConfig, id: &str) -> bool {
    let loaded = printkit_plugin_host::load(&config.data_dir.value);
    loaded
        .registry(printkit_plugin_host::builtins())
        .active
        .active(id)
}

/// Debug-only seam that runs a test hook before the CLI renders/prints.
///
/// Release builds do not compile this function, so production has no hook
/// call at all. It exists solely for `plugins_fail_open_e2e.sh`.
#[cfg(debug_assertions)]
pub(crate) fn test_hook_before_render() {
    let Some(hook) = printkit_plugin_host::test_hook_from_env() else {
        return;
    };
    let runner = printkit_plugin_host::HookRunner::new(printkit_plugin_host::DEFAULT_HOOK_TIMEOUT);
    runner.register(hook);
    let ctx = printkit_plugin_host::HookCtx {
        job_id: "cli".to_owned(),
        trace_id: "cli".to_owned(),
        printer: None,
        pages: None,
    };
    let _ = runner.run(printkit_plugin_host::HookPhase::BeforeRender, &ctx);
}

/// Debug-only switch that lets URL items reach loopback and private hosts.
///
/// The URL fixtures under `tests/fixtures` are served from 127.0.0.1 by
/// `tests/scripts/serve_url_fixture.sh`, and only a debug build honours
/// `PRINTKIT_ALLOW_PRIVATE_FETCH=1`: a release binary always keeps the SSRF
/// guard on, so neither an environment nor a shipped script can open it.
pub(crate) fn allow_private_fetch() -> bool {
    allow_private_fetch_from(std::env::var_os("PRINTKIT_ALLOW_PRIVATE_FETCH").as_deref())
}

/// The build-and-value rule behind [`allow_private_fetch`], split out so a
/// test can check both the debug and release answer.
fn allow_private_fetch_from(value: Option<&std::ffi::OsStr>) -> bool {
    cfg!(debug_assertions) && value.is_some_and(|value| value == "1")
}

pub fn run(command: Command, overrides: CliOverrides) -> Result<(), Failure> {
    match command {
        Command::Pdf(args) => pdf::run(args, overrides),
        Command::Print(args) => print::run(args, overrides),
        Command::Printers(args) => printers::run(args, overrides),
        Command::Raw(args) => raw::run(args, overrides),
        Command::Papers(args) => papers::run(args, overrides),
        #[cfg(feature = "serve")]
        Command::Origin(args) => origin::run(args, overrides),
        #[cfg(feature = "serve")]
        Command::Plugin(args) => plugin::run(args, overrides),
        Command::Schema(args) => schema::run(args),
        Command::Doctor(args) => doctor::run(args, overrides),
        Command::Logs(args) => logs::run(args, overrides),
        #[cfg(feature = "serve")]
        Command::Stats(args) => stats::run(args, overrides),
        Command::Config(args) => config_cmd::run(args, overrides),
        #[cfg(feature = "serve")]
        Command::Preview(args) => preview::run(args, overrides),
        #[cfg(feature = "serve")]
        Command::Serve(args) => serve::run(args, overrides),
        Command::Version(args) => version::run(args),
    }
}
#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::allow_private_fetch_from;

    #[test]
    fn the_private_fetch_switch_only_works_in_a_debug_build() {
        let enabled = allow_private_fetch_from(Some(OsStr::new("1")));
        assert_eq!(enabled, cfg!(debug_assertions));
        assert!(!allow_private_fetch_from(Some(OsStr::new("0"))));
        assert!(!allow_private_fetch_from(None));
    }
}
