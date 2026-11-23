//! The type-state marker for a document that passed semantic validation.
//!
//! Renderers take `&Validated`, so "validate before rendering" is a compile
//! time rule rather than a convention.

use crate::document::Document;
use crate::validate::Violation;

/// A document that passed [`Document::validate`].
#[derive(Debug, Clone, PartialEq)]
pub struct Validated(Document);

impl Validated {
    pub(crate) fn new(document: Document) -> Self {
        Self(document)
    }

    /// The validated document.
    pub fn document(&self) -> &Document {
        &self.0
    }

    /// Unwrap back into the plain document.
    pub fn into_document(self) -> Document {
        self.0
    }
}

impl Document {
    /// Validate and return the marker type accepted by renderers.
    pub fn validated(self) -> Result<Validated, Vec<Violation>> {
        self.validate()?;
        Ok(Validated::new(self))
    }
}
