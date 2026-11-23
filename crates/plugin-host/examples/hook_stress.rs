//! Exercise the fail-open hook runner without wiring the print path.
//!
//! `PRINTKIT_TEST_HOOK=sleep|panic|warn` selects one test hook. The process
//! always exits successfully and prints `print-ok`, mirroring what a caller
//! must observe when a hook misbehaves.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use printkit_plugin_host::{HookCtx, HookPhase, HookRunner};

fn main() {
    let runner = HookRunner::new(Duration::from_millis(20));
    #[cfg(debug_assertions)]
    if let Some(hook) = printkit_plugin_host::test_hook_from_env() {
        runner.register(hook);
    }
    let ctx = HookCtx {
        job_id: "harness-job".to_owned(),
        trace_id: "harness-trace".to_owned(),
        printer: Some("fake-printer".to_owned()),
        pages: Some(1),
    };
    let outcomes = runner.run(HookPhase::BeforeRender, &ctx);
    println!("outcomes={outcomes:?} warnings={}", runner.warnings());
    println!("print-ok");
}
