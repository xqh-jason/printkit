//! Result types shared by several methods.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::code::ErrorCode;
use crate::envelope::{JobState, WarningBody};

/// Host platform reported by [`SysInfo`].
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    /// Apple macOS, wire name `macos`.
    Macos,
    /// Microsoft Windows, wire name `windows`.
    Windows,
    /// Linux, wire name `linux`.
    Linux,
}

/// What this build can actually do. Every bit is the implemented behaviour, not
/// the platform's potential, so a client hides an entry when its bit is false.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// The backend reports duplex printing; both current backends report
    /// `false` until a duplex device is verified.
    pub duplex: bool,
    /// The backend can select a paper source (tray); v1 reports `false`.
    pub paper_source: bool,
    /// Retracting an already submitted **graphic/PDF** job.
    pub cancel_submitted: bool,
    /// Retracting an already submitted **raw** job, which always has a
    /// spooler id on the supported platforms.
    pub cancel_raw: bool,
    /// Preview windows are implemented (`doc.preview` and `preview.*`).
    pub preview: bool,
    /// A raster fallback exists for platforms without a direct path; v1 reports
    /// `false`.
    pub raster_fallback: bool,
    /// The `raw.send` channel is implemented; per-printer support is
    /// [`Printer::raw`].
    pub raw: bool,
    /// `doc.printBatch` is implemented.
    pub batch: bool,
    /// A complete PDF reaches the driver without re-rendering (`doc.printPdf`
    /// and embedded PDFs).
    pub pdf_passthrough: bool,
    /// Actual parallelism of the render pool; macOS reports 1.
    pub render_workers: u32,
}

/// `sys.info` result: engine identity plus runtime capabilities.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SysInfo {
    /// Engine version string, currently the crate version.
    pub engine: String,
    /// Protocol major version; see [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION).
    pub protocol: u32,
    /// Host platform the engine runs on.
    pub platform: Platform,
    /// Capability bits of this build; see [`Capabilities`].
    pub capabilities: Capabilities,
}

/// One printer as reported by the driver.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Printer {
    /// Printer name as the system reports it; unique on the machine.
    pub name: String,
    /// `true` for the system default printer (`isDefault`).
    pub is_default: bool,
    /// The printer accepts raw bytes on `raw.send`.
    pub raw: bool,
    /// MIME types the driver advertises, for example `application/pdf`.
    pub mime_types: Vec<String>,
    /// The printer reports duplex support.
    pub duplex: bool,
    /// The printer reports paper-source selection (`paperSource`).
    pub paper_source: bool,
}

/// Whether a paper size is a named standard size or caller supplied.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaperKind {
    /// A standard size the driver knows by name (`named`).
    Named,
    /// Caller-supplied dimensions in millimetres (`custom`).
    Custom,
}

/// One paper size a printer accepts.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PaperSize {
    /// Size name, for example `A4` or the caller's label for a custom size.
    pub name: String,
    /// Width in millimetres (`widthMm`).
    pub width_mm: f64,
    /// Height in millimetres (`heightMm`).
    pub height_mm: f64,
    /// Whether `name` is a standard size or caller supplied.
    pub kind: PaperKind,
}

/// Failure attached to a job status.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct JobError {
    /// The [`ErrorCode`] spelling, for example `E_JOB_FAILED`.
    pub code: ErrorCode,
    /// Human-readable failure text suitable for logs and UI.
    pub message: String,
}

/// Current state of one job.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobStatus {
    /// Job handle returned when the job was submitted (`jobId`).
    pub job_id: String,
    /// Lifecycle phase; the wire words match [`JobState`].
    pub status: JobState,
    /// Remaining queue position while the job waits.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub queue_pos: Option<u32>,
    /// Pages in the submitted PDF, copies excluded.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub pages: Option<u32>,
    /// Warnings from a successful render; empty while the job runs or failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<WarningBody>,
    /// Failure details, present only once `status` is `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub error: Option<JobError>,
}
