//! CLI failure reporting. Errors are English, as everything inside the repo.

use printkit_facade::{EngineError, PlatformError};
use serde_json::json;

use crate::config::ConfigError;

/// A user-facing failure with an error code and the process exit status.
#[derive(Debug)]
pub struct Failure {
    pub code: &'static str,
    pub message: String,
    pub detail: Option<serde_json::Value>,
    pub exit_code: u8,
}

impl Failure {
    pub fn from_config(error: ConfigError) -> Self {
        Self {
            code: "E_CONFIG",
            detail: Some(json!({ "key": error.key, "message": error.message })),
            message: error.to_string(),
            exit_code: 1,
        }
    }

    /// Usage or configuration mistake on the command line.
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            code: "E_CONFIG",
            message: message.into(),
            detail: None,
            exit_code: 1,
        }
    }

    /// Input cannot be used as requested: missing file, invalid PDF, bad
    /// document structure.
    pub fn schema(message: impl Into<String>) -> Self {
        Self {
            code: "E_SCHEMA",
            message: message.into(),
            detail: None,
            exit_code: 2,
        }
    }

    pub fn from_engine(error: EngineError) -> Self {
        let exit_code = match error.code() {
            "E_CONFIG" => 1,
            "E_SCHEMA" => 2,
            // Exit 4 means the host cannot render at all, as opposed to a
            // failure while rendering this job.
            "E_UNSUPPORTED" => 4,
            _ => 3,
        };
        // Schema failures carry the JSON Pointer; an unsupported PDF object
        // carries its object number. The engine decides which detail applies.
        let detail = error.detail();
        Self {
            code: error.code(),
            message: error.to_string(),
            detail,
            exit_code,
        }
    }

    /// A print-system failure, for commands that never render a document.
    pub fn from_platform(error: PlatformError) -> Self {
        let (code, exit_code) = match &error {
            PlatformError::Unsupported(_) | PlatformError::Unavailable(_) => ("E_UNSUPPORTED", 4),
            PlatformError::Timeout(_) => ("E_TIMEOUT", 3),
            PlatformError::PrinterNotFound(_) | PlatformError::NoDefaultPrinter => {
                ("E_PRINTER_NOT_FOUND", 3)
            }
            PlatformError::CancelUnsupported => ("E_CANCEL_UNSUPPORTED", 3),
            PlatformError::RawUnsupported(_) => ("E_RAW_UNSUPPORTED", 3),
            PlatformError::WindowNotFound(_) | PlatformError::JobNotFound { .. } => {
                ("E_NOT_FOUND", 3)
            }
            PlatformError::Print(_) | PlatformError::Render(_) => ("E_JOB_FAILED", 3),
        };
        Self {
            code,
            message: error.to_string(),
            detail: None,
            exit_code,
        }
    }

    /// A known failure code from a long-running command.
    #[cfg(feature = "serve")]
    pub fn runtime(code: &'static str, message: impl Into<String>, exit_code: u8) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
            exit_code,
        }
    }

    /// Local IO failure, for example an unwritable output path.
    pub fn io(message: impl Into<String>) -> Self {
        Self {
            code: "E_JOB_FAILED",
            message: message.into(),
            detail: None,
            exit_code: 3,
        }
    }

    /// The host cannot satisfy the request at all (renderer unavailable).
    pub fn environment(message: impl Into<String>) -> Self {
        Self {
            code: "E_UNSUPPORTED",
            message: message.into(),
            detail: None,
            exit_code: 4,
        }
    }

    /// `--json` writes one structured object to stderr; otherwise a readable
    /// line. stdout stays reserved for the command's own output.
    pub fn report(&self, json: bool) {
        if json {
            let payload = json!({
                "code": self.code,
                "message": self.message,
                "detail": self.detail,
            });
            eprintln!("{payload}");
        } else {
            eprintln!("printkit: {} [{}]", self.message, self.code);
        }
    }
}
