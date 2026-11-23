#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used))]
#![deny(missing_docs)]

//! The printkit document model.
//!
//! `Document` is declarative JSON data. This crate is a leaf - it must not
//! depend on any other internal crate and must not perform IO. Validation lives
//! here so that the service, the CLI and the generated JSON Schema all share one
//! rule set.

pub mod data_uri;
pub mod document;
pub mod item;
pub mod limits;
pub mod parse;
pub mod schema_ext;
pub mod units;
pub mod validate;
pub mod validated;

pub use document::{
    DOCUMENT_VERSION, Document, Duplex, Margins, Meta, Orientation, OutputSpec, Page, PageSize,
    PageSpec, PrinterSpec,
};
pub use item::{
    BarcodeData, BarcodeKind, BarcodeStyle, Cell, Column, Dim, EcLevel, Frame, HtmlData, HtmlStyle,
    ImageData, ImageFit, ImageStyle, Item, Overflow, PdfData, PdfStyle, ShapeData, ShapeKind,
    ShapeStyle, TableData, TableFooter, TableStyle, TextAlign, TextData, TextItem, TextRuleLines,
    TextStyle, VAlign,
};
pub use limits::*;
pub use parse::{DocumentError, ParseError, parse, validate_bytes};
pub use units::Length;
pub use validate::{E_PDF_ENCRYPTED, E_SCHEMA, E_UNSUPPORTED, Violation};
pub use validated::Validated;
