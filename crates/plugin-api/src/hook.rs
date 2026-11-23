//! Job lifecycle hooks.
//!
//! A hook observes one phase of a print job. The host runs it in isolation and
//! converts every failure into a warning: a plugin can never fail a print.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Default time a host waits for one hook.
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_millis(50);

/// One point in a job's life at which a hook may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookPhase {
    /// Before rendering starts.
    BeforeRender,
    /// After a rendered page/document is available.
    AfterRender,
    /// Before the job is handed to the print system.
    BeforeSubmit,
    /// After the print system accepted the job.
    AfterSubmit,
    /// After a usage record was written.
    AfterRecord,
}

/// The only facts a hook receives; it never sees a document body or token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookCtx {
    /// Job identifier.
    pub job_id: String,
    /// Trace identifier shared with the JSONL events.
    pub trace_id: String,
    /// Printer name, when one was chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub printer: Option<String>,
    /// Final page count, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
}

/// What a hook asks the host to do with the observation.
///
/// In S1 the runner only records outcomes; `Skip` is reserved for a later
/// phase and does not stop other hooks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookOutcome {
    /// The hook completed and has nothing to report.
    Continue,
    /// The hook completed and wants to surface a non-fatal warning.
    Warn(String),
    /// The hook asks the host to skip the optional work for this phase.
    Skip,
}

/// Why a hook failed. The host turns every variant into a warning.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HookError {
    /// The hook exceeded the host timeout.
    #[error("hook {hook} timed out after {timeout_ms} ms")]
    Timeout {
        /// Hook id.
        hook: &'static str,
        /// Timeout used by the runner.
        timeout_ms: u64,
    },
    /// The hook panicked; the runner caught it.
    #[error("hook {hook} panicked")]
    Panic {
        /// Hook id.
        hook: &'static str,
    },
    /// The hook reported its own failure.
    #[error("hook {hook} failed: {message}")]
    Failed {
        /// Hook id.
        hook: &'static str,
        /// Hook-supplied explanation.
        message: String,
    },
}

/// A plugin's optional job observer.
///
/// The trait is object-safe (`&dyn JobHook`) and `Send + Sync`; the host can
/// therefore keep one static instance and run it from a worker thread.
pub trait JobHook: Send + Sync {
    /// Stable id used in warnings and counters.
    fn id(&self) -> &'static str;

    /// Phases this hook wants to observe.
    fn phases(&self) -> &'static [HookPhase];

    /// Observe one phase. Returning [`HookError`] is a warning, not a print
    /// failure; the host never propagates it to the caller.
    fn run(&self, ctx: &HookCtx) -> Result<HookOutcome, HookError>;
}
