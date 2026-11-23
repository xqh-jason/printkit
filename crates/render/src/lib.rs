#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used))]
#![deny(missing_docs)]

//! Document to HTML.
//!
//! CSS only: no external stylesheet, no script, no URL. Document-supplied HTML
//! is sanitised, and a URL item receives its body from [`Fetched`], which the
//! engine fills before rendering. The renderer itself performs no I/O, so the
//! WebView only ever sees memory-resident bytes.
//!
//! This crate must not depend on `platform*`: rendering only knows HTML.

use std::collections::BTreeMap;
use std::fmt;

mod fonts;
mod html;
mod sanitize;
mod shape;
mod table;

pub use fonts::RenderWarning;
pub use html::{
    Rendered, render_document, render_document_with, render_document_with_heights, to_html,
};

/// Measured height in millimetres for each `<tr>`, keyed by `data-row-key`.
///
/// The engine fills this from the platform renderer after a first layout pass;
/// rows without an entry fall back to the declared height or the estimate.
pub type RowHeights = BTreeMap<String, f64>;

/// Bodies fetched for `html` items that carry `data.url`.
///
/// Keyed by the exact URL in the document. The engine fetches each unique URL
/// once; render lookups are pure, which keeps the HTML serialiser testable
/// without a network.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fetched {
    bodies: BTreeMap<String, String>,
}

impl Fetched {
    /// An empty map; every `data.url` item fails with
    /// [`RenderError::MissingFetch`] until the engine inserts its body.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the body fetched for `url`; a later insert for the same URL
    /// replaces the earlier body.
    pub fn insert(&mut self, url: impl Into<String>, body: impl Into<String>) {
        self.bodies.insert(url.into(), body.into());
    }

    /// The body recorded for `url`, or `None` when the engine never fetched it.
    pub fn get(&self, url: &str) -> Option<&str> {
        self.bodies.get(url).map(String::as_str)
    }

    /// Whether no body has been inserted at all.
    pub fn is_empty(&self) -> bool {
        self.bodies.is_empty()
    }
}

/// A document that validated but cannot be turned into HTML.
#[non_exhaustive]
#[derive(Debug)]
pub enum RenderError {
    /// The barcode encoder rejected the value.
    Barcode {
        /// Id of the document item whose barcode failed.
        item_id: String,
        /// Encoder message, for example an unknown symbology or a value the
        /// chosen symbology cannot represent.
        message: String,
    },
    /// A URL item reached the renderer without a fetched body.
    MissingFetch {
        /// Id of the `html` item that carries `data.url`.
        item_id: String,
        /// The exact URL the engine did not fetch.
        url: String,
    },
    /// An image data URI could not be decoded for inline SVG.
    Image {
        /// Id of the image item whose `src` failed to decode.
        item_id: String,
        /// Why the payload was rejected: `src` is not a base64 data URI, or
        /// the SVG payload is not UTF-8.
        message: String,
    },
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Barcode { item_id, message } => {
                write!(formatter, "barcode item {item_id}: {message}")
            }
            Self::MissingFetch { item_id, url } => {
                write!(formatter, "html item {item_id}: no fetched body for {url}")
            }
            Self::Image { item_id, message } => {
                write!(formatter, "image item {item_id}: {message}")
            }
        }
    }
}

impl std::error::Error for RenderError {}
