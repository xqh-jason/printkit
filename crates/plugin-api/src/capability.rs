//! Declarative capability kinds.
//!
//! These types only describe what a plugin may contribute; the host does not
//! implement any of them in S1.
//!
//! Wire spelling is kebab case (`printer-backend`). `CapabilityKind` is a
//! declarative plugin contribution kind and is deliberately distinct from the
//! frozen protocol `CapabilityFamily` (`template`): no new protocol family is
//! introduced by this contract, and no mapping between the two is implied.
//!
//! The enum is `#[non_exhaustive]`: a future 1.x may add a kind, and consumers
//! must keep a wildcard arm. A wire spelling change is an API 2 change.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// What kind of capability a plugin declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CapabilityKind {
    /// A printer backend implementation.
    PrinterBackend,
    /// An extra PDF/export format.
    ExportFormat,
    /// A source for report or statistics data.
    ReportSource,
}

/// Why a capability string cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CapabilityKindError {
    /// The string is empty.
    #[error("a capability kind must not be empty")]
    Empty,
    /// The string is not one of the documented kinds.
    #[error("unknown capability kind: {0}")]
    Unknown(String),
}

impl CapabilityKind {
    /// The wire spelling, kebab case.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PrinterBackend => "printer-backend",
            Self::ExportFormat => "export-format",
            Self::ReportSource => "report-source",
        }
    }
}

impl FromStr for CapabilityKind {
    type Err = CapabilityKindError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.trim().replace('_', "-").to_ascii_lowercase();
        match normalized.as_str() {
            "" => Err(CapabilityKindError::Empty),
            "printer-backend" => Ok(Self::PrinterBackend),
            "export-format" => Ok(Self::ExportFormat),
            "report-source" => Ok(Self::ReportSource),
            other => Err(CapabilityKindError::Unknown(other.to_owned())),
        }
    }
}

impl fmt::Display for CapabilityKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One capability declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilitySpec {
    /// Stable id of the contribution, unique within its kind.
    pub id: String,
    /// What the contribution is.
    pub kind: CapabilityKind,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn parsing_accepts_both_spellings() {
        for (raw, kind) in [
            ("printer-backend", CapabilityKind::PrinterBackend),
            ("printer_backend", CapabilityKind::PrinterBackend),
            ("EXPORT-FORMAT", CapabilityKind::ExportFormat),
            ("report-source", CapabilityKind::ReportSource),
        ] {
            assert_eq!(CapabilityKind::from_str(raw).unwrap(), kind);
        }
    }

    #[test]
    fn unknown_kinds_are_rejected() {
        assert!(CapabilityKind::from_str("").is_err());
        assert!(CapabilityKind::from_str("toolbar").is_err());
    }
}
