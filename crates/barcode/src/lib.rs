#![deny(missing_docs)]

//! Barcode symbologies rendered as inline SVG.
//!
//! `rxing` owns the encoding algorithms; this crate maps document fields to
//! encoder parameters and turns the module matrix into millimetre rectangles.
//! The output is vector only: a raster barcode would lose edge sharpness and
//! trip the `W_RASTER_ONLY` warning contract.

use std::fmt::Write as _;

use printkit_core::{BarcodeData, BarcodeKind, BarcodeStyle, EcLevel, Frame};
use rxing::{BarcodeFormat, EncodeHints, MultiFormatWriter, Writer};

/// A barcode that could not be encoded.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The encoder rejected the value: it is not valid for the symbology (bad
    /// length or characters) or does not fit the code's capacity. The message
    /// is the encoder's, not the item's field name.
    #[error("barcode encoder rejected the value: {0}")]
    Encode(String),
}

/// Minimum quiet-zone widths in modules for a linear symbology.
///
/// EAN-13 requires 11 modules on the left and 7 on the right; Code 39 and
/// Code 128 require 10 on both sides.
fn minimum_quiet_zone_modules(kind: BarcodeKind) -> Option<(f64, f64)> {
    match kind {
        BarcodeKind::Ean13 => Some((11.0, 7.0)),
        BarcodeKind::Code39 | BarcodeKind::Code128 => Some((10.0, 10.0)),
        BarcodeKind::Qr | BarcodeKind::Pdf417 | BarcodeKind::DataMatrix => None,
    }
}

/// Minimum left/right quiet zone in millimetres for a linear symbology.
pub fn minimum_quiet_zone_mm(kind: BarcodeKind, bar_width_mm: f64) -> Option<(f64, f64)> {
    minimum_quiet_zone_modules(kind)
        .map(|(left, right)| (left * bar_width_mm, right * bar_width_mm))
}

/// Human-readable shortfall when `quietZone` is below the symbology minimum.
pub fn quiet_zone_warning(data: &BarcodeData, style: &BarcodeStyle) -> Option<String> {
    let (left, right) = minimum_quiet_zone_modules(data.kind)?;
    let module = style.bar_width.mm().max(0.0);
    if module <= 0.0 {
        return None;
    }
    let configured = style.quiet_zone.mm().max(0.0);
    let required_left = left * module;
    let required_right = right * module;
    if configured + 0.001 >= required_left.max(required_right) {
        return None;
    }
    Some(format!(
        "quietZone {configured:.2} mm is below the {required_left:.2} mm left /          {required_right:.2} mm right minimum for this barcode; the renderer uses the minimum"
    ))
}

/// Render one barcode item as an inline `<svg>` element.
///
/// The frame is the physical box in millimetres. Linear codes (Code 128,
/// Code 39, EAN-13) keep their module width (`style.barWidth`) and are
/// centred inside the frame, shrinking only when the symbol plus quiet zone
/// would not fit; the symbology's minimum quiet zone is always kept. Two-
/// dimensional codes (QR, PDF 417, Data Matrix) are scaled to the frame
/// because their module count is fixed by the payload. The value text is
/// drawn below the symbol when `style.showText` is set and
/// `style.textSize` leaves room for it.
///
/// Output is vector only: one `<rect>` per run of black modules, never a
/// raster image. The only failure is [`Error::Encode`].
pub fn to_svg(data: &BarcodeData, style: &BarcodeStyle, frame: &Frame) -> Result<String, Error> {
    let width = frame.width.mm();
    let height = frame.height.mm();
    let quiet_zone = style.quiet_zone.mm().max(0.0);
    let text_size = style.text_size.mm().max(0.0);
    let show_text = style.show_text && text_size < height;

    let format = format_of(data.kind);
    let hints = EncodeHints {
        ErrorCorrection: Some(error_correction(data.kind, style.ec_level)),
        // The quiet zone is applied in millimetres below; encoder margins would
        // add a second, unit-less margin that cannot be sized exactly.
        Margin: Some("0".to_owned()),
        ..EncodeHints::default()
    };
    let linear = matches!(
        data.kind,
        BarcodeKind::Code128 | BarcodeKind::Code39 | BarcodeKind::Ean13
    );
    let matrix = MultiFormatWriter
        .encode_with_hints(
            &data.value,
            &format,
            0,
            // One module row is enough for a linear code; the writer expands
            // it to the frame height when the SVG is built.
            if linear { 1 } else { 0 },
            &hints,
        )
        .map_err(|error| Error::Encode(error.to_string()))?;
    let matrix = trim_white_border(&matrix);

    let mut svg = String::with_capacity(256);
    let _ = write!(
        svg,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100%\" height=\"100%\" viewBox=\"0 0 {} {}\" preserveAspectRatio=\"none\" fill=\"{}\">",
        number(width),
        number(height),
        style.color
    );

    let top = 0.0;
    let symbol_height = if show_text {
        height - text_size
    } else {
        height
    };

    if linear {
        let modules = matrix.width() as f64;
        let mut module_mm = style.bar_width.mm().max(0.0);
        let mut symbol_width = modules * module_mm;
        let (minimum_left, minimum_right) =
            minimum_quiet_zone_modules(data.kind).unwrap_or((0.0, 0.0));
        let mut left_margin = quiet_zone.max(minimum_left * module_mm);
        let mut right_margin = quiet_zone.max(minimum_right * module_mm);
        let total = symbol_width + left_margin + right_margin;
        if total > width && total > 0.0 {
            // Shrink the whole symbol, quiet zone included, so it never
            // overflows its frame. The minimum-quiet-zone ratios stay intact.
            let scale = width / total;
            module_mm *= scale;
            symbol_width *= scale;
            left_margin *= scale;
            right_margin *= scale;
        }
        let left = (width - (symbol_width + left_margin + right_margin)) / 2.0 + left_margin;
        for run in runs(&matrix) {
            let x = left + f64::from(run.x) * module_mm;
            let run_width = f64::from(run.width) * module_mm;
            if run_width > 0.0 {
                let _ = write!(
                    svg,
                    "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"/>",
                    number(x),
                    number(top),
                    number(run_width),
                    number(symbol_height)
                );
            }
        }
    } else {
        let area_height = symbol_height;
        let content_width = (width - quiet_zone * 2.0).max(0.1);
        let content_height = (area_height - quiet_zone * 2.0).max(0.1);
        let module_mm =
            (content_width / matrix.width() as f64).min(content_height / matrix.height() as f64);
        let symbol_width = matrix.width() as f64 * module_mm;
        let symbol_height = matrix.height() as f64 * module_mm;
        let left = (width - symbol_width) / 2.0;
        let top = (area_height - symbol_height) / 2.0;
        for run in runs(&matrix) {
            let _ = write!(
                svg,
                "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"/>",
                number(left + run.x as f64 * module_mm),
                number(top + run.y as f64 * module_mm),
                number(run.width as f64 * module_mm),
                number(run.height as f64 * module_mm)
            );
        }
    }

    if show_text {
        let baseline = height - text_size * 0.2;
        let _ = write!(
            svg,
            "<text x=\"{}\" y=\"{}\" font-size=\"{}\" text-anchor=\"middle\" font-family=\"Noto Sans Mono\" fill=\"{}\">{}</text>",
            number(width / 2.0),
            number(baseline),
            number(text_size),
            style.color,
            escape_xml(&data.value)
        );
    }
    svg.push_str("</svg>");
    Ok(svg)
}

fn format_of(kind: BarcodeKind) -> BarcodeFormat {
    match kind {
        BarcodeKind::Code128 => BarcodeFormat::CODE_128,
        BarcodeKind::Code39 => BarcodeFormat::CODE_39,
        BarcodeKind::Ean13 => BarcodeFormat::EAN_13,
        BarcodeKind::Qr => BarcodeFormat::QR_CODE,
        BarcodeKind::Pdf417 => BarcodeFormat::PDF_417,
        BarcodeKind::DataMatrix => BarcodeFormat::DATA_MATRIX,
    }
}

fn error_correction(kind: BarcodeKind, level: EcLevel) -> String {
    match kind {
        // PDF417 accepts a numeric level from 0 to 8; the document enum maps to
        // the middle of each documented band.
        BarcodeKind::Pdf417 => match level {
            EcLevel::L => "0",
            EcLevel::M => "2",
            EcLevel::Q => "4",
            EcLevel::H => "6",
        },
        _ => match level {
            EcLevel::L => "L",
            EcLevel::M => "M",
            EcLevel::Q => "Q",
            EcLevel::H => "H",
        },
    }
    .to_owned()
}

/// One black rectangle in module coordinates.
struct Run {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// Find the smallest module rectangle around the black modules.
fn trim_white_border(matrix: &rxing::common::BitMatrix) -> rxing::common::BitMatrix {
    let width = matrix.width();
    let height = matrix.height();
    let mut min_x = width;
    let mut max_x = 0;
    let mut min_y = height;
    let mut max_y = 0;
    let mut found = false;
    for y in 0..height {
        for x in 0..width {
            if matrix.get(x, y) {
                found = true;
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }
    if !found {
        return matrix.clone();
    }
    let crop_width = max_x - min_x + 1;
    let crop_height = max_y - min_y + 1;
    let mut cropped =
        rxing::common::BitMatrix::new(crop_width, crop_height).unwrap_or_else(|_| matrix.clone());
    for y in 0..crop_height {
        for x in 0..crop_width {
            if matrix.get(min_x + x, min_y + y) {
                cropped.set(x, y);
            }
        }
    }
    cropped
}

/// Merge black modules into maximal rectangles: a row run is extended downwards
/// while the next row covers exactly the same columns.
fn runs(matrix: &rxing::common::BitMatrix) -> Vec<Run> {
    let width = matrix.width();
    let height = matrix.height();
    let mut covered = vec![vec![false; width as usize]; height as usize];
    let mut result = Vec::new();
    for y in 0..height {
        let mut x = 0;
        while x < width {
            if !matrix.get(x, y) || covered[y as usize][x as usize] {
                x += 1;
                continue;
            }
            let start = x;
            while x < width && matrix.get(x, y) && !covered[y as usize][x as usize] {
                x += 1;
            }
            let mut end_y = y + 1;
            while end_y < height && same_run(matrix, start, x, end_y) {
                end_y += 1;
            }
            for row in y..end_y {
                for column in start..x {
                    covered[row as usize][column as usize] = true;
                }
            }
            result.push(Run {
                x: start,
                y,
                width: x - start,
                height: end_y - y,
            });
        }
    }
    result
}

fn same_run(matrix: &rxing::common::BitMatrix, start: u32, end: u32, y: u32) -> bool {
    (start..end).all(|x| matrix.get(x, y))
        && (if start > 0 {
            !matrix.get(start - 1, y)
        } else {
            true
        })
        && (if end < matrix.width() {
            !matrix.get(end, y)
        } else {
            true
        })
}

fn number(value: f64) -> String {
    let mut text = format!("{value:.3}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    if text.is_empty() || text == "-0" {
        "0".to_owned()
    } else {
        text
    }
}

fn escape_xml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use printkit_core::{Frame, Length};

    use super::*;

    fn frame(width: f64, height: f64) -> Frame {
        Frame {
            x: Length::Mm(0.0),
            y: Length::Mm(0.0),
            width: Length::Mm(width),
            height: Length::Mm(height),
        }
    }

    fn data(kind: BarcodeKind, value: &str) -> BarcodeData {
        BarcodeData {
            kind,
            value: value.to_owned(),
        }
    }

    #[test]
    fn linear_code_is_vector_and_fits_its_frame() {
        let style = BarcodeStyle {
            quiet_zone: Length::Mm(0.0),
            ..BarcodeStyle::default()
        };
        let svg = to_svg(
            &data(BarcodeKind::Code128, "PK-00042"),
            &style,
            &frame(40.0, 12.0),
        )
        .expect("code128 encodes");
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("<rect"));
        assert!(!svg.contains("<canvas"));
    }

    #[test]
    fn two_dimensional_code_fills_a_matching_frame() {
        let style = BarcodeStyle {
            quiet_zone: Length::Mm(0.0),
            ec_level: EcLevel::M,
            ..BarcodeStyle::default()
        };
        let svg = to_svg(
            &data(BarcodeKind::Qr, "PRINTKIT"),
            &style,
            &frame(20.0, 20.0),
        )
        .expect("qr encodes");
        let first = svg.split("<rect ").nth(1).expect("at least one rectangle");
        assert!(first.contains("x=\"0\""));
        assert!(first.contains("y=\"0\""));
    }

    #[test]
    fn linear_symbologies_require_their_standard_quiet_zones() {
        assert_eq!(
            minimum_quiet_zone_mm(BarcodeKind::Ean13, 0.25),
            Some((2.75, 1.75))
        );
        assert_eq!(
            minimum_quiet_zone_mm(BarcodeKind::Code128, 0.25),
            Some((2.5, 2.5))
        );
        assert_eq!(
            minimum_quiet_zone_mm(BarcodeKind::Code39, 0.25),
            Some((2.5, 2.5))
        );
        assert_eq!(minimum_quiet_zone_mm(BarcodeKind::Qr, 0.25), None);
        let default = BarcodeStyle::default();
        let ean = data(BarcodeKind::Ean13, "4006381333931");
        assert!(quiet_zone_warning(&ean, &default).is_some());
        let wide = BarcodeStyle {
            quiet_zone: Length::Mm(3.0),
            ..default
        };
        assert!(quiet_zone_warning(&ean, &wide).is_none());
    }

    #[test]
    fn every_symbology_encodes_without_panic() {
        let style = BarcodeStyle::default();
        for (kind, value) in [
            (BarcodeKind::Code128, "PK-00042"),
            (BarcodeKind::Code39, "PK-00042"),
            (BarcodeKind::Ean13, "4006381333931"),
            (BarcodeKind::Qr, "PRINTKIT"),
            (BarcodeKind::Pdf417, "PRINTKIT-417"),
            (BarcodeKind::DataMatrix, "PRINTKIT-DM"),
        ] {
            let svg =
                to_svg(&data(kind, value), &style, &frame(40.0, 20.0)).expect("symbology encodes");
            assert!(svg.contains("<rect"), "{kind:?} draws no modules");
        }
    }

    #[test]
    fn modules_take_the_declared_colour() {
        let style = BarcodeStyle {
            color: "#123456".to_owned(),
            quiet_zone: Length::Mm(0.0),
            ..BarcodeStyle::default()
        };
        let svg = to_svg(
            &data(BarcodeKind::Code128, "PK-00042"),
            &style,
            &frame(40.0, 12.0),
        )
        .expect("code128 encodes");
        assert!(svg.contains("fill=\"#123456\""), "colour is missing: {svg}");
    }

    #[test]
    fn a_linear_symbol_wider_than_its_frame_is_shrunk_to_fit() {
        let style = BarcodeStyle {
            quiet_zone: Length::Mm(0.0),
            bar_width: Length::Mm(1.0),
            ..BarcodeStyle::default()
        };
        let svg = to_svg(
            &data(BarcodeKind::Code128, "PK-00042"),
            &style,
            &frame(10.0, 12.0),
        )
        .expect("code128 encodes");
        for rect in svg.split("<rect ").skip(1) {
            let x = attribute(rect, "x");
            let width = attribute(rect, "width");
            assert!(x + width <= 10.001, "rect overflows the frame: {rect}");
        }
    }

    fn attribute(rect: &str, name: &str) -> f64 {
        let pattern = format!("{name}=\"");
        let start = rect.find(&pattern).expect("attribute exists") + pattern.len();
        let end = rect[start..].find('"').expect("attribute ends") + start;
        rect[start..end].parse().expect("attribute is a number")
    }
}
