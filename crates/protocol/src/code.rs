//! The error and warning vocabulary of the protocol.
//!
//! These enums are the single type behind `Violation.code`, trace codes and
//! generated SDK types. The strings are part of the public contract: they are
//! what a client switches on, so [`ErrorCode::as_str`] and the serde spelling
//! must stay identical.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A request that failed. Every failure has exactly one code.
#[non_exhaustive]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum ErrorCode {
    /// The service is not running or the port is unreachable.
    #[serde(rename = "E_CONNECT")]
    Connect,
    /// The token is invalid or lacks the required scope.
    #[serde(rename = "E_AUTH")]
    Auth,
    /// The origin is not whitelisted, or differs from the handshake header.
    #[serde(rename = "E_ORIGIN")]
    Origin,
    /// The administrator session is valid but CSRF cookie/header differ.
    #[serde(rename = "E_CSRF")]
    Csrf,
    /// The document failed the structural or the semantic layer.
    #[serde(rename = "E_SCHEMA")]
    Schema,
    /// No printer carries that name.
    #[serde(rename = "E_PRINTER_NOT_FOUND")]
    PrinterNotFound,
    /// The driver does not accept that paper size.
    #[serde(rename = "E_PAPER_UNSUPPORTED")]
    PaperUnsupported,
    /// Printing failed; `detail` carries the driver status.
    #[serde(rename = "E_JOB_FAILED")]
    JobFailed,
    /// The render queue or the job itself timed out.
    #[serde(rename = "E_TIMEOUT")]
    Timeout,
    /// The protocol major version is incompatible, or `hello` is incomplete.
    #[serde(rename = "E_PROTOCOL")]
    Protocol,
    /// The request uses a capability this build does not implement.
    #[serde(rename = "E_UNSUPPORTED")]
    Unsupported,
    /// The job was already submitted and the platform cannot retract it.
    #[serde(rename = "E_CANCEL_UNSUPPORTED")]
    CancelUnsupported,
    /// The printer or driver does not accept raw bytes.
    #[serde(rename = "E_RAW_UNSUPPORTED")]
    RawUnsupported,
    /// The embedded PDF is password protected.
    #[serde(rename = "E_PDF_ENCRYPTED")]
    PdfEncrypted,
    /// A `jobId` or `windowId` does not exist.
    #[serde(rename = "E_NOT_FOUND")]
    NotFound,
    /// A runtime limit that shrinking the request can clear.
    #[serde(rename = "E_TOO_LARGE")]
    TooLarge,
    /// The renderer process crashed.
    #[serde(rename = "E_RENDER_CRASH")]
    RenderCrash,
    /// Another `serve` instance owns the data directory.
    #[serde(rename = "E_ALREADY_RUNNING")]
    AlreadyRunning,
}

impl ErrorCode {
    /// Every code, in the order of the published error table.
    pub const ALL: [Self; 18] = [
        Self::Connect,
        Self::Auth,
        Self::Origin,
        Self::Csrf,
        Self::Schema,
        Self::PrinterNotFound,
        Self::PaperUnsupported,
        Self::JobFailed,
        Self::Timeout,
        Self::Protocol,
        Self::Unsupported,
        Self::CancelUnsupported,
        Self::RawUnsupported,
        Self::PdfEncrypted,
        Self::NotFound,
        Self::TooLarge,
        Self::RenderCrash,
        Self::AlreadyRunning,
    ];

    /// The wire spelling, identical to the serde rename.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "E_CONNECT",
            Self::Auth => "E_AUTH",
            Self::Origin => "E_ORIGIN",
            Self::Csrf => "E_CSRF",
            Self::Schema => "E_SCHEMA",
            Self::PrinterNotFound => "E_PRINTER_NOT_FOUND",
            Self::PaperUnsupported => "E_PAPER_UNSUPPORTED",
            Self::JobFailed => "E_JOB_FAILED",
            Self::Timeout => "E_TIMEOUT",
            Self::Protocol => "E_PROTOCOL",
            Self::Unsupported => "E_UNSUPPORTED",
            Self::CancelUnsupported => "E_CANCEL_UNSUPPORTED",
            Self::RawUnsupported => "E_RAW_UNSUPPORTED",
            Self::PdfEncrypted => "E_PDF_ENCRYPTED",
            Self::NotFound => "E_NOT_FOUND",
            Self::TooLarge => "E_TOO_LARGE",
            Self::RenderCrash => "E_RENDER_CRASH",
            Self::AlreadyRunning => "E_ALREADY_RUNNING",
        }
    }
}

/// A problem carried by a successful response. A warning never fails a job.
#[non_exhaustive]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum WarningCode {
    /// The raster fallback was used; the request still succeeded.
    #[serde(rename = "W_RASTER_ONLY")]
    RasterOnly,
    /// `fontFamily` names neither a bundled family nor one from `data.fontFile`.
    #[serde(rename = "W_FONT_UNKNOWN")]
    FontUnknown,
    /// The font lacks glyphs for some characters; `detail` lists the code points.
    #[serde(rename = "W_MISSING_GLYPH")]
    MissingGlyph,
    /// The protocol minor version differs; the request ran in compatibility mode.
    #[serde(rename = "W_VERSION_MISMATCH")]
    VersionMismatch,
    /// Table rows were paginated from estimated heights.
    #[serde(rename = "W_TABLE_HEIGHTS_ESTIMATED")]
    TableHeightsEstimated,
    /// One table row is taller than the page body and got its own page.
    #[serde(rename = "W_TABLE_ROW_TOO_TALL")]
    TableRowTooTall,
    /// A page mixed PDF overlays with HTML items; PDF items paint above HTML
    /// items regardless of array order.
    #[serde(rename = "W_PDF_OVERLAY_TOP")]
    PdfOverlayTop,
    /// `quietZone` is below the minimum for the symbology; the renderer widens
    /// it to the minimum.
    #[serde(rename = "W_BARCODE_QUIET_ZONE")]
    BarcodeQuietZone,
    /// A batch rebuilt the PDF from pages; input AcroForm, Outlines or
    /// Metadata were not carried over.
    #[serde(rename = "W_PDF_MERGE_METADATA_LOST")]
    PdfMergeMetadataLost,
}

impl WarningCode {
    /// Every code, in the order of the published warning table.
    pub const ALL: [Self; 9] = [
        Self::RasterOnly,
        Self::FontUnknown,
        Self::MissingGlyph,
        Self::VersionMismatch,
        Self::TableHeightsEstimated,
        Self::TableRowTooTall,
        Self::PdfOverlayTop,
        Self::BarcodeQuietZone,
        Self::PdfMergeMetadataLost,
    ];

    /// The wire spelling, identical to the serde rename.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RasterOnly => "W_RASTER_ONLY",
            Self::FontUnknown => "W_FONT_UNKNOWN",
            Self::MissingGlyph => "W_MISSING_GLYPH",
            Self::VersionMismatch => "W_VERSION_MISMATCH",
            Self::TableHeightsEstimated => "W_TABLE_HEIGHTS_ESTIMATED",
            Self::TableRowTooTall => "W_TABLE_ROW_TOO_TALL",
            Self::PdfOverlayTop => "W_PDF_OVERLAY_TOP",
            Self::BarcodeQuietZone => "W_BARCODE_QUIET_ZONE",
            Self::PdfMergeMetadataLost => "W_PDF_MERGE_METADATA_LOST",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ErrorCode, WarningCode};

    #[test]
    fn codes_serialise_as_their_documented_spelling() {
        for code in ErrorCode::ALL {
            let value = serde_json::to_value(code).unwrap();
            assert_eq!(value.as_str(), Some(code.as_str()));
            assert!(code.as_str().starts_with("E_"));
        }
        for code in WarningCode::ALL {
            let value = serde_json::to_value(code).unwrap();
            assert_eq!(value.as_str(), Some(code.as_str()));
            assert!(code.as_str().starts_with("W_"));
        }
    }

    #[test]
    fn warning_codes_are_the_published_table() {
        assert_eq!(
            WarningCode::ALL.map(WarningCode::as_str),
            [
                "W_RASTER_ONLY",
                "W_FONT_UNKNOWN",
                "W_MISSING_GLYPH",
                "W_VERSION_MISMATCH",
                "W_TABLE_HEIGHTS_ESTIMATED",
                "W_TABLE_ROW_TOO_TALL",
                "W_PDF_OVERLAY_TOP",
                "W_BARCODE_QUIET_ZONE",
                "W_PDF_MERGE_METADATA_LOST",
            ]
        );
    }
}
