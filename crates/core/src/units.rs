//! Physical units used by the document model.

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A length in millimetres.
///
/// JSON carries a bare number in millimetres.
///
/// ```
/// use printkit_core::Length;
///
/// let paper_width = Length::Mm(210.0);
/// assert_eq!(paper_width.mm(), 210.0);
/// // A length serialises as the bare number of millimetres, never as an
/// // object such as `{"mm": 210.0}`.
/// ```
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", as = "f64")
)]
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub enum Length {
    /// Absolute length in millimetres.
    Mm(f64),
}

impl Length {
    /// The value in millimetres.
    pub fn mm(self) -> f64 {
        match self {
            Self::Mm(value) => value,
        }
    }

    /// Whether the value is finite.
    pub fn is_finite(self) -> bool {
        match self {
            Self::Mm(value) => value.is_finite(),
        }
    }

    /// Whether the value is below zero.
    pub fn is_negative(self) -> bool {
        match self {
            Self::Mm(value) => value < 0.0,
        }
    }

    /// Whether the value is a finite, strictly positive length.
    pub fn is_positive(self) -> bool {
        self.is_finite() && self.mm() > 0.0
    }
}

impl Default for Length {
    fn default() -> Self {
        Self::Mm(0.0)
    }
}

impl fmt::Display for Length {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mm(value) => value.fmt(f),
        }
    }
}

impl Serialize for Length {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.mm())
    }
}

impl<'de> Deserialize<'de> for Length {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        f64::deserialize(deserializer).map(Self::Mm)
    }
}

impl JsonSchema for Length {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Length")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let mut schema = f64::json_schema(generator);
        schema.insert(
            "description".into(),
            "Length in millimetres; the only unit in version 1.".into(),
        );
        schema
    }

    fn inline_schema() -> bool {
        true
    }
}
