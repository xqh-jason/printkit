//! The document envelope: pages, paper specs, output options and metadata.
//!
//! Every struct denies unknown fields: a misspelled field must fail loudly
//! instead of silently doing nothing.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::item::Item;
use crate::limits;
use crate::units::Length;

/// The document version understood by this build.
pub const DOCUMENT_VERSION: u32 = 1;

/// A print document: explicit pages in print order.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct Document {
    /// Document version; this build accepts `1` only.
    #[schemars(description = "Document version; this build accepts 1 only.")]
    pub version: u32,
    /// Pages in print order; between 1 and 200.
    #[schemars(length(min = 1, max = limits::MAX_PAGES))]
    pub pages: Vec<Page>,
    /// Job-level output options; defaults to one copy with collation on.
    #[serde(default)]
    pub output: OutputSpec,
    #[serde(default)]
    /// Caller metadata; defaults to empty.
    pub meta: Meta,
}

/// One page: its paper spec plus items in z-order.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct Page {
    /// Paper size, orientation and margins.
    pub spec: PageSpec,
    #[serde(default)]
    /// Items in z-order, drawn back to front; at most 500 per page.
    #[schemars(length(max = limits::MAX_ITEMS_PER_PAGE))]
    pub items: Vec<Item>,
}

/// Named paper size. `custom` takes its dimensions from `width` and `height`.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PageSize {
    /// ISO A3, 297 by 420 millimetres.
    #[serde(rename = "A3")]
    A3,
    /// ISO A4, 210 by 297 millimetres (default).
    #[default]
    #[serde(rename = "A4")]
    A4,
    /// ISO A5, 148 by 210 millimetres.
    #[serde(rename = "A5")]
    A5,
    /// US Letter, 215.9 by 279.4 millimetres.
    #[serde(rename = "Letter")]
    Letter,
    /// Dimensions come from `width` and `height`.
    #[serde(rename = "custom")]
    Custom,
}

impl PageSize {
    /// Width and height in millimetres in portrait orientation.
    pub fn portrait_mm(self) -> Option<(f64, f64)> {
        match self {
            Self::A3 => Some((297.0, 420.0)),
            Self::A4 => Some((210.0, 297.0)),
            Self::A5 => Some((148.0, 210.0)),
            Self::Letter => Some((215.9, 279.4)),
            Self::Custom => None,
        }
    }
}

/// Reading direction of a page.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    /// Portrait; named sizes keep their taller side vertical (default).
    #[default]
    Portrait,
    /// Landscape; named sizes rotate the paper.
    Landscape,
}

/// The paper spec of a page.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
#[schemars(deny_unknown_fields)]
#[schemars(transform = crate::schema_ext::page_spec_conditions)]
pub struct PageSpec {
    /// Named paper size. `custom` requires `width` and `height`.
    pub size: PageSize,
    /// Final paper width in millimetres. Required for, and only allowed with,
    /// `size: "custom"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::schema_ext::positive_length_option")]
    pub width: Option<Length>,
    /// Final paper height in millimetres. Required for, and only allowed with,
    /// `size: "custom"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::schema_ext::positive_length_option")]
    pub height: Option<Length>,
    /// Omitted means: named paper is portrait, custom paper derives it from
    /// `width` and `height`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orientation: Option<Orientation>,
    /// Page margins; zero on every side by default.
    pub margins: Margins,
    /// Registered background image as a base64 data URI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
}

impl PageSpec {
    /// Orientation actually applied. For `custom` paper it follows the
    /// dimensions when the field is omitted.
    pub fn effective_orientation(&self) -> Orientation {
        match (self.size, self.orientation) {
            (PageSize::Custom, Some(orientation)) => orientation,
            (PageSize::Custom, None) => match (self.width, self.height) {
                (Some(width), Some(height)) if width.mm() > height.mm() => Orientation::Landscape,
                _ => Orientation::Portrait,
            },
            (_, Some(orientation)) => orientation,
            (_, None) => Orientation::Portrait,
        }
    }

    /// Final paper size in millimetres. Custom paper is already written as the
    /// final face, so only named sizes are rotated.
    pub fn size_mm(&self) -> (f64, f64) {
        let Some((width, height)) = self.size.portrait_mm() else {
            return (
                self.width.unwrap_or_default().mm(),
                self.height.unwrap_or_default().mm(),
            );
        };
        match self.effective_orientation() {
            Orientation::Portrait => (width, height),
            Orientation::Landscape => (height, width),
        }
    }

    /// Paper name used by `printers.papers` and paper matching.
    ///
    /// Named sizes keep their wire spelling. Custom paper carries its final
    /// dimensions, because two custom sheets with different sizes are not the
    /// same paper even when both documents say `size: "custom"`.
    pub fn paper_name(&self) -> String {
        match self.size {
            PageSize::A3 => "A3".to_owned(),
            PageSize::A4 => "A4".to_owned(),
            PageSize::A5 => "A5".to_owned(),
            PageSize::Letter => "Letter".to_owned(),
            PageSize::Custom => {
                let (width, height) = self.size_mm();
                format!("custom:{width}x{height}")
            }
        }
    }

    /// Whether the dimensions of a custom page are written landscape.
    pub fn custom_is_landscape(&self) -> bool {
        match (self.width, self.height) {
            (Some(width), Some(height)) => width.mm() > height.mm(),
            _ => false,
        }
    }
}

/// Page margins in millimetres.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
#[schemars(deny_unknown_fields)]
pub struct Margins {
    /// Top margin in millimetres; finite and non-negative. Default `0`.
    #[schemars(range(min = 0.0))]
    pub top: Length,
    /// Right margin in millimetres; finite and non-negative. Default `0`.
    #[schemars(range(min = 0.0))]
    pub right: Length,
    /// Bottom margin in millimetres; finite and non-negative. Default `0`.
    #[schemars(range(min = 0.0))]
    pub bottom: Length,
    /// Left margin in millimetres; finite and non-negative. Default `0`.
    #[schemars(range(min = 0.0))]
    pub left: Length,
}

/// How copies are grouped.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Duplex {
    /// One side per sheet (default).
    #[default]
    Simplex,
    /// Both sides, bound along the long edge.
    DuplexLong,
    /// Both sides, bound along the short edge.
    DuplexShort,
}

/// Printer selection and per-job options.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct PrinterSpec {
    /// Printer name; `null` means the system default printer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `null` means: follow the driver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplex: Option<Duplex>,
    /// Paper tray name. Version 1 only accepts `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::schema_ext::null_only")]
    pub paper_source: Option<String>,
}

/// Output options applied to the whole document.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
#[schemars(deny_unknown_fields)]
pub struct OutputSpec {
    /// Number of copies; 1 to 999, default `1`.
    #[schemars(range(min = limits::MIN_COPIES, max = limits::MAX_COPIES))]
    pub copies: u32,
    /// `true` prints one complete set per copy, `false` prints page by page.
    pub collate: bool,
    /// Printer selection and per-job options; omitted means the system default
    /// printer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printer: Option<PrinterSpec>,
}

impl Default for OutputSpec {
    fn default() -> Self {
        Self {
            copies: 1,
            collate: true,
            printer: None,
        }
    }
}

/// Caller metadata. Only `tags` is free-form; it is the aggregation dimension.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct Meta {
    /// Template that produced the document, if any; wire name `templateId`.
    /// Not interpreted by the engine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    /// Human-readable title, for administration views.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// At most 8 tags, each at most 24 Unicode characters.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::schema_ext::tags")]
    pub tags: Option<Vec<String>>,
    /// Caller or system that submitted the document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}
