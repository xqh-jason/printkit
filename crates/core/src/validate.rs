//! Semantic validation.
//!
//! Structure (types, required fields, unknown keys) is enforced by serde during
//! parsing. Everything serde cannot express - ranges, cross-field conflicts,
//! resource sizes, symbology rules - is checked here. Schema violations report
//! `E_SCHEMA`; an embedded PDF that must be decrypted reports
//! `E_PDF_ENCRYPTED`. Both carry a JSON Pointer to the offending field.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::data_uri::decode_data_uri;
use crate::document::{
    DOCUMENT_VERSION, Document, Margins, Meta, Orientation, OutputSpec, Page, PageSize, PageSpec,
};
use crate::item::{
    BarcodeData, BarcodeKind, Cell, Frame, HtmlData, ImageData, Item, PdfData, ShapeData,
    ShapeKind, TableData, TextData, TextStyle,
};
use crate::limits::{
    MAX_BARCODE_BYTES, MAX_COPIES, MAX_FONT_BYTES, MAX_GRID_CELLS, MAX_ITEMS_PER_PAGE, MAX_PAGES,
    MAX_RESOURCE_BYTES, MAX_TABLE_COLUMNS, MAX_TABLE_ROWS, MAX_TAG_CHARS, MAX_TAGS, MAX_TEXT_CHARS,
    MIN_COPIES,
};
use crate::units::Length;

/// The error code every schema validation failure reports.
pub const E_SCHEMA: &str = "E_SCHEMA";
/// An embedded PDF is password protected.
pub const E_PDF_ENCRYPTED: &str = "E_PDF_ENCRYPTED";
/// An embedded PDF uses an object the merge cannot copy.
pub const E_UNSUPPORTED: &str = "E_UNSUPPORTED";

/// A semantic validation failure. `pointer` is a JSON Pointer into the input.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Violation {
    /// JSON Pointer to the offending field, for example
    /// `/pages/0/items/2/frame`.
    pub pointer: String,
    /// Error code: `E_SCHEMA`, `E_PDF_ENCRYPTED` or `E_UNSUPPORTED`.
    pub code: &'static str,
    /// Human-readable description; wording is not stable for machine parsing.
    pub message: String,
    /// The PDF object number when the failure names one, for example `99 0 R`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
}

impl Violation {
    /// A schema violation at `pointer`; reports `E_SCHEMA`.
    pub fn new(pointer: impl Into<String>, message: impl Into<String>) -> Self {
        Self::with_code(E_SCHEMA, pointer, message)
    }

    /// A violation with an explicit error code.
    pub fn with_code(
        code: &'static str,
        pointer: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            pointer: pointer.into(),
            code,
            message: message.into(),
            object: None,
        }
    }
}

/// Collects violations so one response names every problem, not just the first.
#[derive(Default)]
struct Validator {
    violations: Vec<Violation>,
}

impl Validator {
    fn reject(&mut self, pointer: impl Into<String>, message: impl Into<String>) {
        self.violations.push(Violation::new(pointer, message));
    }

    fn reject_with_code(
        &mut self,
        code: &'static str,
        pointer: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.violations
            .push(Violation::with_code(code, pointer, message));
    }
}

impl Document {
    /// Semantic checks that serde cannot express.
    pub fn validate(&self) -> Result<(), Vec<Violation>> {
        let mut validator = Validator::default();

        if self.version != DOCUMENT_VERSION {
            validator.reject(
                "/version",
                format!("only version {DOCUMENT_VERSION} is supported"),
            );
        }
        validate_pages(&self.pages, &mut validator);
        validate_output(&self.output, &mut validator);
        validate_meta(&self.meta, &mut validator);

        if validator.violations.is_empty() {
            Ok(())
        } else {
            Err(validator.violations)
        }
    }
}

fn validate_pages(pages: &[Page], validator: &mut Validator) {
    if pages.is_empty() {
        validator.reject("/pages", "a document needs at least one page");
    }
    if pages.len() > MAX_PAGES {
        validator.reject(
            "/pages",
            format!("a document holds at most {MAX_PAGES} pages"),
        );
    }
    // Item ids reach CSS selectors and table measurement keys, both of which
    // are document-scoped; a second occurrence would silently alias the first.
    let mut item_ids: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for (index, page) in pages.iter().enumerate() {
        let base = format!("/pages/{index}");
        validate_page_spec(&page.spec, &format!("{base}/spec"), validator);
        if page.items.len() > MAX_ITEMS_PER_PAGE {
            validator.reject(
                format!("{base}/items"),
                format!("a page holds at most {MAX_ITEMS_PER_PAGE} items"),
            );
        }
        for (item_index, item) in page.items.iter().enumerate() {
            let pointer = format!("{base}/items/{item_index}");
            match item_ids.get(item.id()) {
                Some((first_page, first_item)) => validator.reject(
                    format!("{pointer}/id"),
                    format!(
                        "item id {:?} must be unique across the document; it first appears at \
                         /pages/{first_page}/items/{first_item}",
                        item.id()
                    ),
                ),
                None => {
                    item_ids.insert(item.id(), (index, item_index));
                }
            }
            validate_item(item, &pointer, validator);
            let (paper_width, paper_height) = page.spec.size_mm();
            if paper_width.is_finite() && paper_height.is_finite() {
                validate_frame_bounds(item.frame(), paper_width, paper_height, &pointer, validator);
            }
        }
    }
}

fn validate_page_spec(spec: &PageSpec, base: &str, validator: &mut Validator) {
    match spec.size {
        PageSize::Custom => {
            match spec.width {
                Some(width) if width.is_positive() && width.mm() <= crate::limits::MAX_PAPER_MM => {
                }
                Some(width) if width.is_positive() => validator.reject(
                    format!("{base}/width"),
                    format!(
                        "custom paper width must not exceed {} mm",
                        crate::limits::MAX_PAPER_MM
                    ),
                ),
                Some(_) => validator.reject(
                    format!("{base}/width"),
                    "custom paper width must be a finite, positive number of millimetres",
                ),
                None => validator.reject(format!("{base}/width"), "custom paper requires width"),
            }
            match spec.height {
                Some(height)
                    if height.is_positive() && height.mm() <= crate::limits::MAX_PAPER_MM => {}
                Some(height) if height.is_positive() => validator.reject(
                    format!("{base}/height"),
                    format!(
                        "custom paper height must not exceed {} mm",
                        crate::limits::MAX_PAPER_MM
                    ),
                ),
                Some(_) => validator.reject(
                    format!("{base}/height"),
                    "custom paper height must be a finite, positive number of millimetres",
                ),
                None => validator.reject(format!("{base}/height"), "custom paper requires height"),
            }
            if let Some(orientation) = spec.orientation {
                let derived = if spec.custom_is_landscape() {
                    Orientation::Landscape
                } else {
                    Orientation::Portrait
                };
                if orientation != derived {
                    validator.reject(
                        format!("{base}/orientation"),
                        "orientation conflicts with the custom width and height",
                    );
                }
            }
        }
        _ => {
            if spec.width.is_some() {
                validator.reject(
                    format!("{base}/width"),
                    "width is only allowed when size is custom",
                );
            }
            if spec.height.is_some() {
                validator.reject(
                    format!("{base}/height"),
                    "height is only allowed when size is custom",
                );
            }
        }
    }
    validate_margins(&spec.margins, &format!("{base}/margins"), validator);
    if let Some(background) = &spec.background {
        validate_resource(
            background,
            MAX_RESOURCE_BYTES,
            &["image/"],
            &format!("{base}/background"),
            validator,
        );
    }
}

fn validate_margins(margins: &Margins, base: &str, validator: &mut Validator) {
    for (name, value) in [
        ("top", margins.top),
        ("right", margins.right),
        ("bottom", margins.bottom),
        ("left", margins.left),
    ] {
        if !is_non_negative(value) {
            validator.reject(
                format!("{base}/{name}"),
                "margin must be a finite, non-negative number of millimetres",
            );
        }
    }
}

fn validate_output(output: &OutputSpec, validator: &mut Validator) {
    if !(MIN_COPIES..=MAX_COPIES).contains(&output.copies) {
        validator.reject(
            "/output/copies",
            format!("copies must be between {MIN_COPIES} and {MAX_COPIES}"),
        );
    }
    if let Some(printer) = &output.printer
        && printer.paper_source.is_some()
    {
        validator.reject(
            "/output/printer/paperSource",
            "paperSource must be null in version 1",
        );
    }
}

fn validate_meta(meta: &Meta, validator: &mut Validator) {
    let Some(tags) = &meta.tags else {
        return;
    };
    if tags.len() > MAX_TAGS {
        validator.reject("/meta/tags", format!("at most {MAX_TAGS} tags are allowed"));
    }
    for (index, tag) in tags.iter().enumerate() {
        if tag.chars().count() > MAX_TAG_CHARS {
            validator.reject(
                format!("/meta/tags/{index}"),
                format!("a tag holds at most {MAX_TAG_CHARS} characters"),
            );
        }
    }
}

fn validate_item(item: &Item, base: &str, validator: &mut Validator) {
    if item.id().trim().is_empty() {
        validator.reject(format!("{base}/id"), "item id must not be empty");
    }
    validate_frame(item.frame(), &format!("{base}/frame"), validator);

    match item {
        Item::Text { style, data, .. } => {
            validate_text_style(style, &format!("{base}/style"), validator);
            validate_text_data(data, &format!("{base}/data"), validator);
        }
        Item::Html { data, .. } => validate_html_data(data, &format!("{base}/data"), validator),
        Item::Image { data, .. } => validate_image_data(data, &format!("{base}/data"), validator),
        Item::Pdf { data, .. } => validate_pdf_data(data, &format!("{base}/data"), validator),
        Item::Barcode { style, data, .. } => {
            validate_color(&style.color, &format!("{base}/style/color"), validator);
            if !style.bar_width.is_positive() {
                validator.reject(
                    format!("{base}/style/barWidth"),
                    "bar width must be a finite, positive number of millimetres",
                );
            }
            if !is_non_negative(style.quiet_zone) {
                validator.reject(
                    format!("{base}/style/quietZone"),
                    "quiet zone must be a finite, non-negative number of millimetres",
                );
            }
            if !style.text_size.is_positive() {
                validator.reject(
                    format!("{base}/style/textSize"),
                    "text size must be a finite, positive number of millimetres",
                );
            }
            validate_barcode_data(data, &format!("{base}/data"), validator);
        }
        Item::Shape { style, data, .. } => {
            validate_color(&style.stroke, &format!("{base}/style/stroke"), validator);
            if let Some(fill) = &style.fill {
                validate_color(fill, &format!("{base}/style/fill"), validator);
            }
            if !is_non_negative(style.stroke_width) {
                validator.reject(
                    format!("{base}/style/strokeWidth"),
                    "stroke width must be a finite, non-negative number of millimetres",
                );
            }
            if let Some(dash) = &style.dash {
                if dash.is_empty() {
                    validator.reject(
                        format!("{base}/style/dash"),
                        "dash must list at least one length",
                    );
                }
                for (index, value) in dash.iter().enumerate() {
                    if !is_non_negative(*value) {
                        validator.reject(
                            format!("{base}/style/dash/{index}"),
                            "dash length must be finite and non-negative",
                        );
                    }
                }
            }
            validate_shape_data(data, &format!("{base}/data"), validator);
        }
        Item::Table { style, data, .. } => {
            validate_color(
                &style.border_color,
                &format!("{base}/style/borderColor"),
                validator,
            );
            if !style.font_size.is_positive() {
                validator.reject(
                    format!("{base}/style/fontSize"),
                    "font size must be a finite, positive number of millimetres",
                );
            }
            if !is_non_negative(style.border_width) {
                validator.reject(
                    format!("{base}/style/borderWidth"),
                    "border width must be a finite, non-negative number of millimetres",
                );
            }
            if let Some(height) = style.row_height.mm()
                && (!height.is_finite() || height <= 0.0)
            {
                validator.reject(
                    format!("{base}/style/rowHeight"),
                    "row height must be a finite, positive number of millimetres",
                );
            }
            validate_table_data(data, &format!("{base}/data"), validator);
        }
    }
}

fn validate_frame(frame: &Frame, base: &str, validator: &mut Validator) {
    for (name, value) in [("x", frame.x), ("y", frame.y)] {
        if !is_non_negative(value) {
            validator.reject(
                format!("{base}/{name}"),
                "coordinate must be a finite, non-negative number of millimetres",
            );
        }
    }
    for (name, value) in [("width", frame.width), ("height", frame.height)] {
        if !value.is_positive() {
            validator.reject(
                format!("{base}/{name}"),
                "size must be a finite, positive number of millimetres",
            );
        }
    }
}

fn validate_frame_bounds(
    frame: &Frame,
    paper_width: f64,
    paper_height: f64,
    base: &str,
    validator: &mut Validator,
) {
    let dark = 0.01;
    if frame.x.mm() + frame.width.mm() > paper_width + dark
        || frame.y.mm() + frame.height.mm() > paper_height + dark
    {
        validator.reject(
            format!("{base}/frame"),
            format!("frame extends past the {paper_width}mm x {paper_height}mm paper"),
        );
    }
}

fn validate_text_style(style: &TextStyle, base: &str, validator: &mut Validator) {
    if !style.font_size.is_positive() {
        validator.reject(
            format!("{base}/fontSize"),
            "font size must be a finite, positive number of millimetres",
        );
    }
    if !is_non_negative(style.letter_spacing) {
        validator.reject(
            format!("{base}/letterSpacing"),
            "letter spacing must be finite",
        );
    }
    if !style.line_height.is_finite() || style.line_height <= 0.0 {
        validator.reject(
            format!("{base}/lineHeight"),
            "line height must be a finite, positive multiplier",
        );
    }
    if !style.rotation.is_finite() {
        validator.reject(format!("{base}/rotation"), "rotation must be finite");
    }
    validate_color(&style.color, &format!("{base}/color"), validator);
}

fn validate_text_data(data: &TextData, base: &str, validator: &mut Validator) {
    if data.text.is_empty() {
        validator.reject(format!("{base}/text"), "text must not be empty");
    }
    if data.text.chars().count() > MAX_TEXT_CHARS {
        validator.reject(
            format!("{base}/text"),
            format!("text holds at most {MAX_TEXT_CHARS} characters"),
        );
    }
    if let Some(font) = &data.font_file {
        validate_resource(
            font,
            MAX_FONT_BYTES,
            &["font/", "application/font", "application/octet-stream"],
            &format!("{base}/fontFile"),
            validator,
        );
    }
}

fn validate_html_data(data: &HtmlData, base: &str, validator: &mut Validator) {
    match (&data.html, &data.url) {
        (Some(_), Some(_)) => validator.reject(base, "use exactly one of html or url"),
        (None, None) => validator.reject(base, "use exactly one of html or url"),
        (None, Some(url)) => {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                validator.reject(
                    format!("{base}/url"),
                    "url must use the http or https scheme",
                );
            }
        }
        (Some(_), None) => {}
    }
}

fn validate_image_data(data: &ImageData, base: &str, validator: &mut Validator) {
    let pointer = format!("{base}/src");
    validate_resource(
        &data.src,
        MAX_RESOURCE_BYTES,
        &["image/"],
        &pointer,
        validator,
    );
    if data.src.starts_with("data:image/webp")
        && decode_data_uri(&data.src).is_some_and(|bytes| animated_webp(&bytes))
    {
        validator.reject_with_code(
            E_UNSUPPORTED,
            &pointer,
            "animated WebP is not supported; export a still image",
        );
    }
}

/// WebP animation flag in a `VP8X` chunk, or an animation chunk.
fn animated_webp(bytes: &[u8]) -> bool {
    if bytes.len() < 16 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return false;
    }
    let mut index = 12;
    while index + 8 <= bytes.len() {
        let chunk = &bytes[index..index + 4];
        let size = u32::from_le_bytes([
            bytes[index + 4],
            bytes[index + 5],
            bytes[index + 6],
            bytes[index + 7],
        ]) as usize;
        let data = index + 8;
        if chunk == b"VP8X" && bytes.get(data).is_some_and(|flags| flags & 0x02 != 0) {
            return true;
        }
        if chunk == b"ANIM" || chunk == b"ANMF" {
            return true;
        }
        index = data.saturating_add(size).saturating_add(size & 1);
    }
    false
}

fn validate_pdf_data(data: &PdfData, base: &str, validator: &mut Validator) {
    let src_pointer = format!("{base}/src");
    validate_resource(
        &data.src,
        MAX_RESOURCE_BYTES,
        &["application/pdf"],
        &src_pointer,
        validator,
    );
    if data.page == 0 {
        validator.reject(format!("{base}/page"), "page numbers start at 1");
    }
    // The PDF object graph needs `lopdf`; `printkit-pdf::preflight` checks it
    // in the engine, after document validation and before any render.
}

fn validate_barcode_data(data: &BarcodeData, base: &str, validator: &mut Validator) {
    let pointer = format!("{base}/value");
    if data.value.is_empty() {
        validator.reject(&pointer, "barcode value must not be empty");
        return;
    }
    match data.kind {
        BarcodeKind::Ean13 => {
            let digits = data.value.as_bytes();
            let valid_digits = digits.iter().all(u8::is_ascii_digit);
            match digits.len() {
                12 if valid_digits => {}
                13 if valid_digits => {
                    if !ean13_check_digit(digits) {
                        validator.reject(&pointer, "ean13 check digit does not match");
                    }
                }
                12 | 13 => {
                    validator.reject(&pointer, "ean13 value must contain digits only");
                }
                _ => validator.reject(&pointer, "ean13 value must have 12 or 13 digits"),
            }
        }
        BarcodeKind::Code39 => {
            if !data.value.bytes().all(is_code39_character) {
                validator.reject(&pointer, "value contains a character code39 cannot encode");
            }
        }
        BarcodeKind::Code128 => {
            if !data.value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
                validator.reject(&pointer, "value must be printable ASCII for code128");
            }
        }
        BarcodeKind::Qr | BarcodeKind::Pdf417 | BarcodeKind::DataMatrix => {
            if data.value.len() > MAX_BARCODE_BYTES {
                validator.reject(
                    &pointer,
                    format!("a two-dimensional code holds at most {MAX_BARCODE_BYTES} bytes"),
                );
            }
        }
    }
}

fn validate_shape_data(data: &ShapeData, base: &str, validator: &mut Validator) {
    match data.kind {
        ShapeKind::Polygon => match &data.points {
            None => validator.reject(format!("{base}/points"), "a polygon requires points"),
            Some(points) => {
                if points.len() < 6 || points.len() % 2 != 0 {
                    validator.reject(
                        format!("{base}/points"),
                        "a polygon needs at least three x/y pairs",
                    );
                }
                for (index, value) in points.iter().enumerate() {
                    if !value.is_finite() {
                        validator.reject(
                            format!("{base}/points/{index}"),
                            "point coordinates must be finite",
                        );
                    }
                }
            }
        },
        _ => {
            if data.points.is_some() {
                validator.reject(
                    format!("{base}/points"),
                    "points are only allowed for a polygon",
                );
            }
        }
    }
    match data.kind {
        ShapeKind::TianGrid => match data.cell {
            None => validator.reject(format!("{base}/cell"), "a tianGrid requires cell"),
            Some(Cell { rows, cols }) => {
                if rows == 0 {
                    validator.reject(format!("{base}/cell/rows"), "row count must be at least 1");
                }
                if cols == 0 {
                    validator.reject(
                        format!("{base}/cell/cols"),
                        "column count must be at least 1",
                    );
                }
                if u64::from(rows) * u64::from(cols) > u64::from(MAX_GRID_CELLS) {
                    validator.reject(
                        format!("{base}/cell"),
                        format!("a tianGrid holds at most {MAX_GRID_CELLS} cells"),
                    );
                }
            }
        },
        _ => {
            if data.cell.is_some() {
                validator.reject(
                    format!("{base}/cell"),
                    "cell is only allowed for a tianGrid",
                );
            }
        }
    }
    if data.lines.is_some() && data.kind != ShapeKind::TextRule {
        validator.reject(
            format!("{base}/lines"),
            "lines are only allowed for a textRule",
        );
    }
}

fn validate_table_data(data: &TableData, base: &str, validator: &mut Validator) {
    if data.columns.is_empty() {
        validator.reject(
            format!("{base}/columns"),
            "a table needs at least one column",
        );
    }
    if data.columns.len() > MAX_TABLE_COLUMNS {
        validator.reject(
            format!("{base}/columns"),
            format!("a table holds at most {MAX_TABLE_COLUMNS} columns"),
        );
    }
    let mut keys = BTreeSet::new();
    for (index, column) in data.columns.iter().enumerate() {
        if column.key.trim().is_empty() {
            validator.reject(
                format!("{base}/columns/{index}/key"),
                "column key must not be empty",
            );
        }
        if !keys.insert(column.key.as_str()) {
            validator.reject(
                format!("{base}/columns/{index}/key"),
                "column keys must be unique",
            );
        }
        if let Some(width) = column.width.mm()
            && (!width.is_finite() || width < 0.0)
        {
            validator.reject(
                format!("{base}/columns/{index}/width"),
                "column width must be a finite, non-negative number of millimetres",
            );
        }
    }
    if data.rows.len() > MAX_TABLE_ROWS {
        validator.reject(
            format!("{base}/rows"),
            format!("a table holds at most {MAX_TABLE_ROWS} rows"),
        );
    }
    for (index, row) in data.rows.iter().enumerate() {
        for key in row.keys() {
            if !keys.contains(key.as_str()) {
                validator.reject(
                    format!("{base}/rows/{index}/{key}"),
                    "row key does not name a column",
                );
            }
        }
    }
    if let Some(footer) = &data.footer {
        for key in footer.values.keys() {
            if !keys.contains(key.as_str()) {
                validator.reject(
                    format!("{base}/footer/values/{key}"),
                    "footer key does not name a column",
                );
            }
        }
    }
}

/// A registered resource: `data:<mime>;base64,<payload>` within `max_bytes`
/// after decoding. Anything else is rejected rather than passed to a renderer.
fn validate_resource(
    value: &str,
    max_bytes: usize,
    expected: &[&str],
    pointer: &str,
    validator: &mut Validator,
) {
    let Some(rest) = value.strip_prefix("data:") else {
        validator.reject(pointer, "resource must be a base64 data URI");
        return;
    };
    let Some((meta, _payload)) = rest.split_once(',') else {
        validator.reject(pointer, "resource must be a base64 data URI");
        return;
    };
    let mime = meta.split(';').next().unwrap_or("");
    if !expected.iter().any(|prefix| mime.starts_with(prefix)) {
        validator.reject(
            pointer,
            format!(
                "resource must use one of these media types: {}",
                expected.join(", ")
            ),
        );
        return;
    }
    let Some(bytes) = crate::data_uri::decode_data_uri(value) else {
        validator.reject(pointer, "resource payload is not valid base64");
        return;
    };
    if bytes.is_empty() {
        validator.reject(pointer, "resource payload is empty");
        return;
    }
    if bytes.len() > max_bytes {
        validator.reject(pointer, format!("resource exceeds {max_bytes} bytes"));
    }
}

fn validate_color(value: &str, pointer: &str, validator: &mut Validator) {
    if !is_hex_color(value) {
        validator.reject(pointer, "colour must be written as #rrggbb");
    }
}

fn is_hex_color(value: &str) -> bool {
    let Some(hex) = value.strip_prefix('#') else {
        return false;
    };
    hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_non_negative(value: Length) -> bool {
    value.is_finite() && !value.is_negative()
}

fn ean13_check_digit(digits: &[u8]) -> bool {
    let mut sum = 0u32;
    for (index, digit) in digits[..12].iter().enumerate() {
        let value = u32::from(digit - b'0');
        sum += if index % 2 == 0 { value } else { value * 3 };
    }
    let expected = (10 - sum % 10) % 10;
    u32::from(digits[12] - b'0') == expected
}

fn is_code39_character(byte: u8) -> bool {
    byte.is_ascii_digit()
        || byte.is_ascii_uppercase()
        || matches!(byte, b'-' | b'.' | b' ' | b'$' | b'/' | b'+' | b'%')
}
