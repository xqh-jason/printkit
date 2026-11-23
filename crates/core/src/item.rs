//! The seven item types and their styles.
//!
//! Styles are separate structs per item type so an unknown style key fails
//! instead of leaking into another type's renderer.

use std::borrow::Cow;
use std::collections::BTreeMap;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::units::Length;

/// One element on a page. Array order is z-order.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Item {
    /// A text block rendered by the text pipeline.
    Text {
        /// Item id; non-empty and unique across the whole document.
        #[schemars(length(min = 1))]
        id: String,
        /// Position and size of the item on the page.
        frame: Frame,
        /// Text style; omitted fields keep their defaults.
        #[serde(default)]
        style: TextStyle,
        /// Text body and optional embedded font.
        data: TextData,
    },
    /// Inline HTML or a fetched URL, rendered in a web view.
    Html {
        /// Item id; non-empty and unique across the whole document.
        #[schemars(length(min = 1))]
        id: String,
        /// Position and size of the item on the page.
        frame: Frame,
        /// HTML style; `overflow` defaults to `hidden`.
        #[serde(default)]
        style: HtmlStyle,
        /// Inline HTML or a URL, never both.
        data: HtmlData,
    },
    /// A raster image, embedded as a data URI.
    Image {
        /// Item id; non-empty and unique across the whole document.
        #[schemars(length(min = 1))]
        id: String,
        /// Position and size of the item on the page.
        frame: Frame,
        /// Image style; `fit` defaults to `contain`.
        #[serde(default)]
        style: ImageStyle,
        /// Image data as a base64 data URI.
        data: ImageData,
    },
    /// One page of an embedded PDF, merged into the output.
    Pdf {
        /// Item id; non-empty and unique across the whole document.
        #[schemars(length(min = 1))]
        id: String,
        /// Position and size of the item on the page.
        frame: Frame,
        /// PDF style; the empty object is always accepted.
        #[serde(default)]
        style: PdfStyle,
        /// PDF data and the one-based page to place.
        data: PdfData,
    },
    /// A linear or two-dimensional barcode.
    Barcode {
        /// Item id; non-empty and unique across the whole document.
        #[schemars(length(min = 1))]
        id: String,
        /// Position and size of the item on the page.
        frame: Frame,
        /// Barcode style; omitted fields keep their defaults.
        #[serde(default)]
        style: BarcodeStyle,
        /// Symbology and the encoded value.
        data: BarcodeData,
    },
    /// A vector shape, rule or grid.
    Shape {
        /// Item id; non-empty and unique across the whole document.
        #[schemars(length(min = 1))]
        id: String,
        /// Position and size of the item on the page.
        frame: Frame,
        /// Shape style; omitted fields keep their defaults.
        #[serde(default)]
        style: ShapeStyle,
        /// Shape kind and its kind-specific geometry.
        data: ShapeData,
    },
    /// A data table with an optional totals footer.
    Table {
        /// Item id; non-empty and unique across the whole document.
        #[schemars(length(min = 1))]
        id: String,
        /// Position and size of the item on the page.
        frame: Frame,
        /// Table style; omitted fields keep their defaults.
        #[serde(default)]
        style: TableStyle,
        /// Column definitions, rows and optional footer.
        data: TableData,
    },
}

impl Item {
    /// The item id, used in violation messages.
    pub fn id(&self) -> &str {
        match self {
            Self::Text { id, .. }
            | Self::Html { id, .. }
            | Self::Image { id, .. }
            | Self::Pdf { id, .. }
            | Self::Barcode { id, .. }
            | Self::Shape { id, .. }
            | Self::Table { id, .. } => id,
        }
    }

    /// The frame shared by every item type.
    pub fn frame(&self) -> &Frame {
        match self {
            Self::Text { frame, .. }
            | Self::Html { frame, .. }
            | Self::Image { frame, .. }
            | Self::Pdf { frame, .. }
            | Self::Barcode { frame, .. }
            | Self::Shape { frame, .. }
            | Self::Table { frame, .. } => frame,
        }
    }

    /// The JSON `type` discriminator, for errors that name the item type.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Html { .. } => "html",
            Self::Image { .. } => "image",
            Self::Pdf { .. } => "pdf",
            Self::Barcode { .. } => "barcode",
            Self::Shape { .. } => "shape",
            Self::Table { .. } => "table",
        }
    }
}

/// The parts of a text item that a text renderer reads.
pub struct TextItem<'a> {
    /// Item id as written in the document.
    pub id: &'a str,
    /// Position and size of the item on the page.
    pub frame: &'a Frame,
    /// Text style resolved for rendering.
    pub style: &'a TextStyle,
    /// Text body and optional embedded font.
    pub data: &'a TextData,
}

impl Item {
    /// A text view of the item, or `None` for the other six item types.
    pub fn as_text(&self) -> Option<TextItem<'_>> {
        match self {
            Self::Text {
                id,
                frame,
                style,
                data,
            } => Some(TextItem {
                id,
                frame,
                style,
                data,
            }),
            _ => None,
        }
    }
}

/// A rectangle relative to the top-left corner of the page, in millimetres.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct Frame {
    /// Horizontal offset from the page's left edge, in millimetres; non-negative.
    #[schemars(range(min = 0.0))]
    pub x: Length,
    /// Vertical offset from the page's top edge, in millimetres; non-negative.
    #[schemars(range(min = 0.0))]
    pub y: Length,
    /// Frame width in millimetres; finite and strictly positive.
    #[schemars(schema_with = "crate::schema_ext::positive_length")]
    pub width: Length,
    /// Frame height in millimetres; finite and strictly positive.
    #[schemars(schema_with = "crate::schema_ext::positive_length")]
    pub height: Length,
}

/// Horizontal alignment of text inside its frame.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TextAlign {
    /// Align to the left edge (default).
    #[default]
    Left,
    /// Centre horizontally inside the frame.
    Center,
    /// Align to the right edge.
    Right,
}

/// Vertical alignment of text inside its frame.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VAlign {
    /// Align to the top edge (default).
    #[default]
    Top,
    /// Centre vertically inside the frame.
    Middle,
    /// Align to the bottom edge.
    Bottom,
}

/// Text style. All lengths are millimetres; `rotation` is degrees clockwise.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct TextStyle {
    /// A bundled family name, or a family declared by `data.fontFile`.
    pub font_family: String,
    /// Glyph size in millimetres; finite and strictly positive. Default `3.7`.
    #[schemars(schema_with = "crate::schema_ext::positive_length")]
    pub font_size: Length,
    /// Use a bold face; the family must carry one.
    pub bold: bool,
    /// Use an italic or oblique face.
    pub italic: bool,
    /// Draw an underline below every line.
    pub underline: bool,
    /// CSS colour; only `#rrggbb` is accepted so a document cannot inject CSS.
    #[schemars(pattern(r"^#[0-9a-fA-F]{6}$"))]
    pub color: String,
    /// Extra space between glyphs in millimetres; non-negative. Default `0`.
    pub letter_spacing: Length,
    /// Line height as a multiple of `fontSize`; finite and positive. Default `1.4`.
    #[schemars(schema_with = "crate::schema_ext::positive_number")]
    pub line_height: f64,
    /// Horizontal alignment inside the frame. Default `left`.
    pub align: TextAlign,
    /// Rotation in degrees clockwise around the frame's centre. Default `0`.
    pub rotation: f64,
    /// Vertical alignment inside the frame. Default `top`.
    pub valign: VAlign,
    /// Wrap lines at the frame width; `false` keeps the text on one line. Default `true`.
    pub wrap: bool,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            font_family: "Source Han Sans SC".to_owned(),
            font_size: Length::Mm(3.7),
            bold: false,
            italic: false,
            underline: false,
            color: "#000000".to_owned(),
            letter_spacing: Length::Mm(0.0),
            line_height: 1.4,
            align: TextAlign::Left,
            rotation: 0.0,
            valign: VAlign::Top,
            wrap: true,
        }
    }
}

/// The text body of a text item.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
#[schemars(transform = crate::schema_ext::text_data_conditions)]
pub struct TextData {
    /// Text to render: non-empty, at most 10,000 Unicode characters.
    #[schemars(length(min = 1, max = crate::limits::MAX_TEXT_CHARS))]
    pub text: String,
    /// Embedded WOFF2 or TTF font as a base64 data URI, at most 5 MiB decoded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_file: Option<String>,
}

/// Overflow policy for an HTML item.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Overflow {
    /// Clip content that does not fit the frame (default).
    #[default]
    Hidden,
    /// Let content draw outside the frame.
    Visible,
}

/// Style of an HTML item; the empty object keeps the item shape uniform.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct HtmlStyle {
    /// Behaviour for content taller or wider than the frame. Default `hidden`.
    pub overflow: Overflow,
}

/// Inline HTML, or a URL fetched by the engine. Exactly one is set.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
#[schemars(transform = crate::schema_ext::html_data_conditions)]
pub struct HtmlData {
    /// Inline HTML source; mutually exclusive with `url`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    /// Absolute `http` or `https` URL fetched before rendering; mutually
    /// exclusive with `html`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// How an image fills its frame.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImageFit {
    /// Scale the image down to fit inside the frame, preserving aspect ratio
    /// (default).
    #[default]
    Contain,
    /// Scale the image to cover the frame, preserving aspect ratio and
    /// cropping the overflow.
    Cover,
    /// Stretch the image to the frame's width and height.
    Fill,
    /// Draw the image at its natural size; the frame clips any overflow.
    None,
}

/// Style of an image item.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct ImageStyle {
    /// How the image fills the frame. Default `contain`.
    pub fit: ImageFit,
}

/// Raster image data: a base64 data URI of at most 8 MiB decoded.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
#[schemars(transform = crate::schema_ext::image_data_conditions)]
pub struct ImageData {
    /// Image as a base64 data URI, at most 8 MiB decoded.
    pub src: String,
}

/// Embedded PDF items have no style of their own; the empty object keeps the
/// item shape uniform.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
#[schemars(deny_unknown_fields)]
pub struct PdfStyle {}

/// The PDF payload of a PDF item, addressed by page.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
#[schemars(transform = crate::schema_ext::pdf_data_conditions)]
pub struct PdfData {
    /// PDF as a base64 data URI, at most 8 MiB decoded.
    pub src: String,
    /// One-based page inside the embedded PDF.
    #[serde(default = "default_pdf_page")]
    #[schemars(range(min = 1))]
    pub page: u32,
}

fn default_pdf_page() -> u32 {
    1
}

/// Barcode symbology.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum BarcodeKind {
    /// Code 128; variable-length printable ASCII.
    #[serde(rename = "code128")]
    Code128,
    /// Code 39; digits, upper-case letters and `- . $ / + %` plus space.
    #[serde(rename = "code39")]
    Code39,
    /// EAN-13; 12 digits, or 13 with the check digit already appended.
    #[serde(rename = "ean13")]
    Ean13,
    /// QR Code; at most 2,048 bytes, uses `ecLevel`.
    #[serde(rename = "qr")]
    Qr,
    /// PDF417.
    #[serde(rename = "pdf417")]
    Pdf417,
    /// Data Matrix.
    #[serde(rename = "datamatrix")]
    DataMatrix,
}

/// Reed-Solomon error correction level for two-dimensional codes.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub enum EcLevel {
    /// About 7% of codewords recoverable.
    L,
    /// About 15% recoverable (default).
    #[default]
    M,
    /// About 25% recoverable.
    Q,
    /// About 30% recoverable.
    H,
}

/// Style of a barcode item. Lengths are millimetres.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct BarcodeStyle {
    /// Width of one module of a linear code, in millimetres.
    #[schemars(schema_with = "crate::schema_ext::positive_length")]
    pub bar_width: Length,
    /// Error correction level for two-dimensional symbols. Default `M`; ignored
    /// by linear codes.
    pub ec_level: EcLevel,
    /// Quiet zone around the symbol in millimetres; non-negative. Default `2.0`.
    #[schemars(range(min = 0.0))]
    pub quiet_zone: Length,
    /// Draw the encoded value as text under a linear code. Default `false`.
    pub show_text: bool,
    /// Size of the human-readable text in millimetres; positive. Default `2.5`.
    #[schemars(schema_with = "crate::schema_ext::positive_length")]
    pub text_size: Length,
    /// Bar or module colour; only `#rrggbb` is accepted. Default `#000000`.
    #[schemars(pattern(r"^#[0-9a-fA-F]{6}$"))]
    pub color: String,
}

impl Default for BarcodeStyle {
    fn default() -> Self {
        Self {
            bar_width: Length::Mm(0.25),
            ec_level: EcLevel::M,
            quiet_zone: Length::Mm(2.0),
            show_text: false,
            text_size: Length::Mm(2.5),
            color: "#000000".to_owned(),
        }
    }
}

/// The payload of a barcode item.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
#[schemars(transform = crate::schema_ext::barcode_data_conditions)]
pub struct BarcodeData {
    /// Symbology: `code128`, `code39`, `ean13`, `qr`, `pdf417` or `datamatrix`.
    pub kind: BarcodeKind,
    /// Encoded value; must not be empty. `ean13` takes 12 digits, or 13 with a
    /// matching check digit; `code128` accepts printable ASCII; two-dimensional
    /// codes hold at most 2,048 bytes.
    pub value: String,
}

/// Vector shape kind.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ShapeKind {
    /// A horizontal rule through the middle of the frame.
    #[serde(rename = "line")]
    Line,
    /// A rectangle filling the frame, inset by half the stroke width.
    #[serde(rename = "rect")]
    Rect,
    /// A circle filling the smaller frame dimension.
    #[serde(rename = "circle")]
    Circle,
    /// An ellipse filling the frame, inset by half the stroke width.
    #[serde(rename = "ellipse")]
    Ellipse,
    /// A closed polygon; coordinates come from `points`.
    #[serde(rename = "polygon")]
    Polygon,
    /// A grid of `cell.rows` by `cell.cols` cells.
    #[serde(rename = "tianGrid")]
    TianGrid,
    /// Horizontal rules along the frame's top and bottom edges.
    #[serde(rename = "textRule")]
    TextRule,
}

/// Style of a shape item. Lengths are millimetres.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct ShapeStyle {
    /// Outline colour; only `#rrggbb` is accepted. Default `#000000`.
    #[schemars(pattern(r"^#[0-9a-fA-F]{6}$"))]
    pub stroke: String,
    /// Outline width in millimetres; non-negative, `0` hides the outline.
    /// Default `0.2`.
    #[schemars(range(min = 0.0))]
    pub stroke_width: Length,
    /// Fill colour as `#rrggbb`; omitted or `null` leaves the shape unfilled.
    #[schemars(pattern(r"^#[0-9a-fA-F]{6}$"))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill: Option<String>,
    /// Dash pattern in millimetres; `null` draws a solid line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dash: Option<Vec<Length>>,
}

impl Default for ShapeStyle {
    fn default() -> Self {
        Self {
            stroke: "#000000".to_owned(),
            stroke_width: Length::Mm(0.2),
            fill: None,
            dash: None,
        }
    }
}

/// A grid of cells, used by the `tianGrid` shape.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct Cell {
    /// Number of cell rows; at least 1.
    #[schemars(range(min = 1, max = crate::limits::MAX_GRID_CELLS))]
    pub rows: u32,
    /// Number of cell columns; at least 1. `rows * cols` may not exceed 10,000.
    #[schemars(range(min = 1, max = crate::limits::MAX_GRID_CELLS))]
    pub cols: u32,
}

/// Which rules a `textRule` shape draws.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
#[schemars(deny_unknown_fields)]
pub struct TextRuleLines {
    /// Draw a rule along the frame's top edge. Default `true`.
    pub top: bool,
    /// Draw a rule along the frame's bottom edge. Default `true`.
    pub bottom: bool,
}

impl Default for TextRuleLines {
    fn default() -> Self {
        Self {
            top: true,
            bottom: true,
        }
    }
}

/// The geometry of a shape item.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct ShapeData {
    /// Which geometry is drawn; selects the other fields that are allowed.
    pub kind: ShapeKind,
    /// Alternating x and y coordinates relative to the frame, in millimetres.
    /// Required for, and only allowed with, `kind: "polygon"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub points: Option<Vec<Length>>,
    /// Required for, and only allowed with, `kind: "tianGrid"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell: Option<Cell>,
    /// Only meaningful for `kind: "textRule"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<TextRuleLines>,
}

/// A width that is either fixed in millimetres or derived from the content.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", type = "\"auto\" | number")
)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Dim {
    /// Derive the width from the content; the JSON spelling is `"auto"`.
    Auto,
    /// A fixed width; the JSON spelling is the bare number of millimetres.
    Mm(Length),
}

impl Dim {
    /// The fixed value, or `None` for `auto`.
    pub fn mm(self) -> Option<f64> {
        match self {
            Self::Auto => None,
            Self::Mm(value) => Some(value.mm()),
        }
    }
}

/// `"auto"` or a number. A separate type keeps the two spellings in one place
/// instead of an untagged enum that would also accept unrelated values.
#[derive(Deserialize)]
#[serde(untagged)]
enum AutoOrNumber {
    Keyword(AutoKeyword),
    Number(f64),
}

#[derive(Deserialize)]
enum AutoKeyword {
    #[serde(rename = "auto")]
    Auto,
}

impl Serialize for Dim {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::Mm(value) => serializer.serialize_f64(value.mm()),
        }
    }
}

impl<'de> Deserialize<'de> for Dim {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match AutoOrNumber::deserialize(deserializer)? {
            AutoOrNumber::Keyword(AutoKeyword::Auto) => Ok(Self::Auto),
            AutoOrNumber::Number(value) => Ok(Self::Mm(Length::Mm(value))),
        }
    }
}

impl JsonSchema for Dim {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Dim")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "oneOf": [
                {"const": "auto"},
                f64::json_schema(generator),
            ]
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

/// One table column.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct Column {
    /// Key used by rows and the footer to address this column.
    #[schemars(length(min = 1))]
    pub key: String,
    /// Header text shown in the header row; may be empty.
    pub title: String,
    /// Column width in millimetres, or `"auto"`.
    pub width: Dim,
    /// Text alignment inside the column. Default `left`.
    #[serde(default)]
    pub align: TextAlign,
}

/// Last row of a table, typically totals.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct TableFooter {
    /// Text shown in the first column when `values` has no entry for its key,
    /// for example `Total`.
    pub label: String,
    /// Cell text keyed by column key; a missing key renders an empty cell, except
    /// in the first column, which falls back to `label`.
    pub values: BTreeMap<String, String>,
}

/// The rows and columns of a table item.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct TableData {
    /// Columns in left-to-right order; at least 1 and at most 32.
    #[schemars(length(min = 1, max = crate::limits::MAX_TABLE_COLUMNS))]
    pub columns: Vec<Column>,
    /// Rows whose keys are column keys and values are cell text; at most 10,000.
    #[schemars(length(max = crate::limits::MAX_TABLE_ROWS))]
    pub rows: Vec<BTreeMap<String, String>>,
    /// Optional totals row; only the last fragment of a split table carries it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer: Option<TableFooter>,
}

/// Style of a table item. Lengths are millimetres.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct TableStyle {
    /// Repeat the header row on every page the table spans.
    pub header_repeat: bool,
    /// Cell text size in millimetres; positive. Default `2.8`.
    #[schemars(schema_with = "crate::schema_ext::positive_length")]
    pub font_size: Length,
    /// Row height in millimetres, or `"auto"` for content height.
    pub row_height: Dim,
    /// Line width in millimetres; non-negative, `0` draws no borders.
    /// Default `0.15`.
    #[schemars(range(min = 0.0))]
    pub border_width: Length,
    /// Border colour; only `#rrggbb` is accepted. Default `#000000`.
    #[schemars(pattern(r"^#[0-9a-fA-F]{6}$"))]
    pub border_color: String,
    /// Shade every second data row. Default `false`.
    pub zebra: bool,
}

impl Default for TableStyle {
    fn default() -> Self {
        Self {
            header_repeat: true,
            font_size: Length::Mm(2.8),
            row_height: Dim::Mm(Length::Mm(6.0)),
            border_width: Length::Mm(0.15),
            border_color: "#000000".to_owned(),
            zebra: false,
        }
    }
}
