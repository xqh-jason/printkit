//! Vector shapes as inline SVG.
//!
//! Coordinates are millimetres and the SVG view box is the item frame, so one
//! user unit is one millimetre once the element is laid out. Strokes are
//! centred on their path; the paths are inset by half the stroke width so the
//! painted area stays inside the frame.

use std::fmt::Write as _;

use printkit_core::{Frame, ShapeData, ShapeKind, ShapeStyle};

/// Render one shape item as an inline `<svg>` element.
pub fn to_svg(data: &ShapeData, style: &ShapeStyle, frame: &Frame) -> String {
    let width = frame.width.mm();
    let height = frame.height.mm();
    let stroke = style.stroke_width.mm().max(0.0);
    let half = stroke / 2.0;

    let mut svg = String::with_capacity(256);
    let _ = write!(
        svg,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100%\" height=\"100%\" viewBox=\"0 0 {} {}\" preserveAspectRatio=\"none\">",
        number(width),
        number(height)
    );
    let _ = write!(
        svg,
        "<g stroke=\"{}\" stroke-width=\"{}\" fill=\"{}\" stroke-linecap=\"butt\" stroke-linejoin=\"miter\"",
        style.stroke,
        number(stroke),
        style.fill.as_deref().unwrap_or("none")
    );
    if let Some(dash) = &style.dash
        && !dash.is_empty()
    {
        svg.push_str(" stroke-dasharray=\"");
        for (index, value) in dash.iter().enumerate() {
            if index > 0 {
                svg.push(' ');
            }
            let _ = write!(svg, "{}", number(value.mm().max(0.0)));
        }
        svg.push('"');
    }
    svg.push('>');

    match data.kind {
        ShapeKind::Line => {
            // A bare line has no endpoints in the contract. A horizontal rule
            // through the middle of the frame is the predictable reading.
            let y = height / 2.0;
            let _ = write!(
                svg,
                "<line x1=\"0\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/>",
                number(y),
                number(width),
                number(y)
            );
        }
        ShapeKind::Rect => {
            let inner_width = (width - stroke).max(0.0);
            let inner_height = (height - stroke).max(0.0);
            let _ = write!(
                svg,
                "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"/>",
                number(half),
                number(half),
                number(inner_width),
                number(inner_height)
            );
        }
        ShapeKind::Circle => {
            let diameter = width.min(height);
            let radius = ((diameter - stroke) / 2.0).max(0.0);
            let _ = write!(
                svg,
                "<circle cx=\"{}\" cy=\"{}\" r=\"{}\"/>",
                number(width / 2.0),
                number(height / 2.0),
                number(radius)
            );
        }
        ShapeKind::Ellipse => {
            let _ = write!(
                svg,
                "<ellipse cx=\"{}\" cy=\"{}\" rx=\"{}\" ry=\"{}\"/>",
                number(width / 2.0),
                number(height / 2.0),
                number(((width - stroke) / 2.0).max(0.0)),
                number(((height - stroke) / 2.0).max(0.0))
            );
        }
        ShapeKind::Polygon => {
            let points: Vec<String> = data
                .points
                .iter()
                .flatten()
                .map(|value| number(value.mm()))
                .collect();
            let _ = write!(svg, "<polygon points=\"{}\"/>", points.join(" "));
        }
        ShapeKind::TianGrid => {
            let rows = data.cell.map(|cell| cell.rows).unwrap_or(1).max(1);
            let cols = data.cell.map(|cell| cell.cols).unwrap_or(1).max(1);
            let inner_width = (width - stroke).max(0.0);
            let inner_height = (height - stroke).max(0.0);
            let _ = write!(
                svg,
                "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"/>",
                number(half),
                number(half),
                number(inner_width),
                number(inner_height)
            );
            for row in 1..rows {
                let y = height * f64::from(row) / f64::from(rows);
                let _ = write!(
                    svg,
                    "<line x1=\"0\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/>",
                    number(y),
                    number(width),
                    number(y)
                );
            }
            for column in 1..cols {
                let x = width * f64::from(column) / f64::from(cols);
                let _ = write!(
                    svg,
                    "<line x1=\"{}\" y1=\"0\" x2=\"{}\" y2=\"{}\"/>",
                    number(x),
                    number(x),
                    number(height)
                );
            }
        }
        ShapeKind::TextRule => {
            let lines = data.lines.unwrap_or_default();
            if lines.top {
                let _ = write!(
                    svg,
                    "<line x1=\"0\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/>",
                    number(half),
                    number(width),
                    number(half)
                );
            }
            if lines.bottom {
                let y = height - half;
                let _ = write!(
                    svg,
                    "<line x1=\"0\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/>",
                    number(y),
                    number(width),
                    number(y)
                );
            }
        }
    }

    svg.push_str("</g></svg>");
    svg
}

/// Format a millimetre value with at most three decimals, without trailing
/// zeros. SVG output is also read by humans in `--dump-html`.
pub(crate) fn number(value: f64) -> String {
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
