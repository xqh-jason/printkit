//! HTML serialisation.

use std::fmt::Write as _;

use base64::Engine as _;
use printkit_core::{
    Frame, HtmlStyle, ImageData, ImageStyle, Item, Overflow, Page, TextAlign, Validated,
};

use crate::{Fetched, RenderError, RenderWarning, RowHeights, fonts, sanitize, shape, table};

const STYLE: &str = concat!(
    "html,body{margin:0;padding:0;font-family:\"printkit Han Sans SC\";}",
    ".page{position:relative;overflow:hidden;box-sizing:border-box;}",
    ".item{position:absolute;white-space:pre-wrap;}",
    ".page-background{position:absolute;left:0;top:0;width:100%;height:100%;object-fit:fill;}",
    ".page-background svg{display:block;width:100%;height:100%;}",
    ".item-image img,.item-image svg,.item-image .image-content{display:block;width:100%;height:100%;}",
    ".item-image.image-fit-none svg{width:auto;height:auto;}",
    ".data-table{border-collapse:collapse;table-layout:fixed;width:100%;}",
    "thead{display:table-header-group;}",
    ".data-table td,.data-table th{box-sizing:border-box;padding:0;overflow:hidden;overflow-wrap:anywhere;line-height:1.15;}",
    ".data-table th{font-weight:bold;}",
);

/// HTML plus the number of paper pages it will occupy.
///
/// Table fragments add pages beyond the document's page list; the renderer
/// host needs the real count to capture every page of the web content.
///
/// `warnings` carries font problems found while serialising: an unknown family
/// and characters the chosen subset cannot address.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    /// The single HTML document: one `.page` box per paper page, every item
    /// absolutely positioned inside its page.
    pub html: String,
    /// Paper pages emitted, including table continuation pages.
    pub pages: usize,
    /// One-based paper page where each document page starts. Table
    /// continuations shift later document pages, and an embedded PDF item must
    /// be painted on the paper page that carries its document page.
    pub page_starts: Vec<usize>,
    /// One entry per paper page, in layout order, in millimetres.
    pub page_sizes: Vec<(f64, f64)>,
    /// Font, barcode and table problems that did not stop serialisation; empty
    /// when the document has none.
    pub warnings: Vec<RenderWarning>,
}

/// Serialise a validated document into one HTML document.
///
/// Every page becomes a `.page` box at its own paper size. The `@page` rule
/// only describes the first page for screen layout; the host captures each
/// box's rectangle separately, so a PDF may mix paper sizes.
pub fn to_html(validated: &Validated) -> Result<String, RenderError> {
    render_document(validated).map(|rendered| rendered.html)
}

/// Serialise the document using bodies fetched by the engine.
pub fn render_document_with(
    validated: &Validated,
    fetched: &Fetched,
) -> Result<Rendered, RenderError> {
    render_document_with_heights(validated, fetched, None)
}

/// Serialise with WebKit-measured table row heights.
///
/// `heights` comes from a first layout pass and keys every `<tr>` the renderer
/// emits; `None` keeps the font-size estimate, which may split a table at the
/// wrong row. A caller that could not measure a table must surface
/// `W_TABLE_HEIGHTS_ESTIMATED` instead of hiding the fallback.
pub fn render_document_with_heights(
    validated: &Validated,
    fetched: &Fetched,
    heights: Option<&RowHeights>,
) -> Result<Rendered, RenderError> {
    let document = validated.document();
    let mut out = String::with_capacity(4096);
    out.push_str("<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><title>");
    let title = document
        .meta
        .title
        .as_deref()
        .or(document.meta.template_id.as_deref())
        .unwrap_or("printkit");
    escape_html_into(&mut out, title);
    out.push_str("</title><style>");
    if let Some(first) = document.pages.first() {
        let (width, height) = first.spec.size_mm();
        let _ = write!(out, "@page{{size:{width}mm {height}mm;margin:0;}}");
    }
    out.push_str(&fonts::face_css(document));
    out.push_str(STYLE);
    out.push_str("</style></head><body>");
    let last = document.pages.len().saturating_sub(1);
    let mut pages = 0;
    let mut page_starts = Vec::with_capacity(document.pages.len());
    let mut page_sizes = Vec::with_capacity(document.pages.len());
    let mut table_warnings = Vec::new();
    for (index, page) in document.pages.iter().enumerate() {
        page_starts.push(pages + 1);
        let layout = write_page(&mut out, page, index == last, fetched, heights)?;
        pages += 1 + layout.continuations;
        table_warnings.extend(layout.warnings);
        let size = page.spec.size_mm();
        page_sizes.extend(std::iter::repeat_n(size, 1 + layout.continuations));
    }
    out.push_str("</body></html>");
    let mut warnings = fonts::warnings(validated);
    warnings.extend(table_warnings);
    Ok(Rendered {
        html: out,
        pages,
        page_starts,
        page_sizes,
        warnings,
    })
}

/// Serialise the document and report how many paper pages it covers.
///
/// A document whose `html` items only carry inline markup needs no network; a
/// `url` item fails with [`RenderError::MissingFetch`] rather than rendering an
/// empty frame.
pub fn render_document(validated: &Validated) -> Result<Rendered, RenderError> {
    render_document_with(validated, &Fetched::default())
}

/// One document page's paper boxes after table continuation.
struct PageLayout {
    continuations: usize,
    warnings: Vec<RenderWarning>,
}

fn write_page(
    out: &mut String,
    page: &Page,
    last_document_page: bool,
    fetched: &Fetched,
    heights: Option<&RowHeights>,
) -> Result<PageLayout, RenderError> {
    let (_, height) = page.spec.size_mm();
    let margins = page.spec.margins;
    // Plan every table before writing the page: the plan decides whether this
    // page needs continuation pages, and therefore whether it breaks.
    let plans: Vec<table::FragmentPlan> = page
        .items
        .iter()
        .map(|item| match item {
            Item::Table {
                id,
                frame,
                style,
                data,
            } => table::plan(id, data, style, frame, height, &margins, heights),
            _ => table::FragmentPlan::default(),
        })
        .collect();
    let continuation_count: usize = plans
        .iter()
        .map(|plan| plan.fragments.len().saturating_sub(1))
        .sum();
    let break_after = !last_document_page || continuation_count > 0;

    write_page_open(out, page, break_after);
    let mut item_warnings = Vec::new();
    for (index, item) in page.items.iter().enumerate() {
        match item {
            Item::Text {
                id,
                frame,
                style,
                data,
            } => write_text(out, id, frame, style, data),
            Item::Shape {
                id,
                frame,
                style,
                data,
            } => write_vector(out, "shape", id, frame, shape::to_svg(data, style, frame)),
            Item::Barcode {
                id,
                frame,
                style,
                data,
            } => {
                if let Some(message) = printkit_barcode::quiet_zone_warning(data, style) {
                    item_warnings.push(RenderWarning {
                        code: "W_BARCODE_QUIET_ZONE",
                        message,
                    });
                }
                let svg = printkit_barcode::to_svg(data, style, frame).map_err(|error| {
                    RenderError::Barcode {
                        item_id: id.clone(),
                        message: error.to_string(),
                    }
                })?;
                write_vector(out, "barcode", id, frame, svg);
            }
            Item::Table {
                id,
                frame,
                style,
                data,
            } => {
                out.push_str(&table::render(
                    id,
                    frame,
                    style,
                    data,
                    &plans[index].fragments[0],
                    frame.y.mm(),
                ));
            }
            Item::Html {
                id,
                frame,
                style,
                data,
            } => write_html_item(out, id, frame, style, data, fetched)?,
            Item::Image {
                id,
                frame,
                style,
                data,
            } => write_image(out, id, frame, style, data)?,
            Item::Pdf { .. } => {}
        }
    }
    out.push_str("</div>");

    // Continuation pages repeat the paper, not the other items: a table chunk
    // that did not fit continues from the top margin of a fresh page.
    let mut emitted = 0;
    for (index, item) in page.items.iter().enumerate() {
        let Item::Table {
            id,
            frame,
            style,
            data,
        } = item
        else {
            continue;
        };
        for fragment in plans[index].fragments.iter().skip(1) {
            emitted += 1;
            let last_continuation = emitted == continuation_count;
            let break_after = !(last_document_page && last_continuation);
            write_page_open(out, page, break_after);
            out.push_str(&table::render(
                id,
                frame,
                style,
                data,
                fragment,
                margins.top.mm(),
            ));
            out.push_str("</div>");
        }
    }
    let mut warnings: Vec<RenderWarning> =
        plans.into_iter().flat_map(|plan| plan.warnings).collect();
    warnings.extend(item_warnings);
    Ok(PageLayout {
        continuations: continuation_count,
        warnings,
    })
}

fn write_page_open(out: &mut String, page: &Page, break_after: bool) {
    let (width, height) = page.spec.size_mm();
    let margins = page.spec.margins;
    // Margins are padding on the page box. Items are positioned with absolute
    // coordinates from the page's top-left corner, so a frame is not shifted by
    // the padding; margins only constrain flow content.
    let _ = write!(
        out,
        "<div class=\"page\" style=\"width:{width}mm;height:{height}mm;padding:{}mm {}mm {}mm {}mm;",
        margins.top, margins.right, margins.bottom, margins.left
    );
    if break_after {
        out.push_str("break-after:page;");
    }
    out.push_str("\">");
    if let Some(background) = &page.spec.background {
        write_background(out, background);
    }
}

/// Paint the page background below every item.
///
/// WebKit's PDF painter does not reliably rasterise an SVG reference held in
/// an `<img>`: a data-URI SVG background exported as a blank (white) page on a
/// headless macOS runner even after the image reported `complete` and decoded.
/// Inline SVG is vector content in the render tree, painted by `createPDF`
/// like shape and barcode markup, so SVG backgrounds are embedded directly.
/// Raster backgrounds keep the `<img>` path.
fn write_background(out: &mut String, background: &str) {
    if let Some((media_type, payload)) = decode_data_uri(background)
        && media_type.eq_ignore_ascii_case("image/svg+xml")
        && let Ok(svg) = String::from_utf8(payload)
    {
        // Scope any `<style>` in the background to its own wrapper so it
        // cannot restyle the host page or another page's items.
        let svg = sanitize::sanitize_svg(&svg, Some(".page-background"));
        let svg = svg_with_fit(&svg, printkit_core::ImageFit::Fill);
        let _ = write!(out, "<div class=\"page-background\">{svg}</div>");
        return;
    }
    let _ = write!(
        out,
        "<img class=\"page-background\" alt=\"\" src=\"{}\" />",
        escape_html(background)
    );
}

fn write_text(
    out: &mut String,
    id: &str,
    frame: &Frame,
    style: &printkit_core::TextStyle,
    data: &printkit_core::TextData,
) {
    let bold = if style.bold { "700" } else { "400" };
    let italic = if style.italic { "italic" } else { "normal" };
    let decoration = if style.underline { "underline" } else { "none" };
    let spacing = style.letter_spacing.mm();
    let rotation = style.rotation;
    let wrap = if style.wrap { "pre-wrap" } else { "nowrap" };
    let align_items = match style.valign {
        printkit_core::VAlign::Top => "flex-start",
        printkit_core::VAlign::Middle => "center",
        printkit_core::VAlign::Bottom => "flex-end",
    };
    let _ = write!(
        out,
        "<div class=\"item item-text\" data-item-id=\"{}\" style=\"left:{}mm;top:{}mm;width:{}mm;height:{}mm;display:flex;align-items:{};font-family:'{}';font-size:{}mm;font-weight:{};font-style:{};text-decoration:{};letter-spacing:{}mm;line-height:{};color:{};white-space:{};overflow:hidden;transform:rotate({}deg);\">\
         <span style=\"display:block;width:100%;text-align:{};overflow-wrap:anywhere;word-break:break-word;\">",
        escape_html(id),
        frame.x,
        frame.y,
        frame.width,
        frame.height,
        align_items,
        escape_html(&fonts::css_string(fonts::resolved_family(
            &style.font_family,
            data.font_file.is_some(),
        ))),
        style.font_size,
        bold,
        italic,
        decoration,
        spacing,
        style.line_height,
        style.color,
        wrap,
        rotation,
        align_css(style.align),
    );
    escape_html_into(out, &data.text);
    out.push_str("</span></div>");
}
fn write_html_item(
    out: &mut String,
    id: &str,
    frame: &Frame,
    style: &HtmlStyle,
    data: &printkit_core::HtmlData,
    fetched: &Fetched,
) -> Result<(), RenderError> {
    let source = match (&data.html, &data.url) {
        (Some(html), _) => html.as_str(),
        (None, Some(url)) => fetched.get(url).ok_or_else(|| RenderError::MissingFetch {
            item_id: id.to_owned(),
            url: url.clone(),
        })?,
        (None, None) => "",
    };
    let scope = item_scope(id);
    let content = sanitize::sanitize_fragment_scoped(source, &scope);
    let overflow = match style.overflow {
        Overflow::Hidden => "hidden",
        Overflow::Visible => "visible",
    };
    let _ = write!(
        out,
        "<div class=\"item item-html\" data-item-id=\"{}\" style=\"left:{}mm;top:{}mm;width:{}mm;height:{}mm;overflow:{};white-space:normal;\">",
        escape_html(id),
        frame.x,
        frame.y,
        frame.width,
        frame.height,
        overflow
    );
    out.push_str(&content);
    out.push_str("</div>");
    Ok(())
}

/// Set the SVG root's `preserveAspectRatio` so fit matches raster images.
fn svg_with_fit(svg: &str, fit: printkit_core::ImageFit) -> String {
    let value = match fit {
        printkit_core::ImageFit::Contain | printkit_core::ImageFit::None => "xMidYMid meet",
        printkit_core::ImageFit::Cover => "xMidYMid slice",
        printkit_core::ImageFit::Fill => "none",
    };
    let Some(tag_end) = svg.find('>') else {
        return svg.to_owned();
    };
    let root = &svg[..=tag_end];
    let cleaned = remove_attribute(root, "preserveAspectRatio");
    let insert = if cleaned.ends_with("/>") {
        cleaned.len() - 2
    } else {
        cleaned.len() - 1
    };
    let mut out = String::with_capacity(svg.len() + 40);
    out.push_str(&cleaned[..insert]);
    let _ = write!(out, " preserveAspectRatio=\"{value}\"");
    out.push_str(&cleaned[insert..]);
    out.push_str(&svg[tag_end + 1..]);
    out
}

fn remove_attribute(tag: &str, name: &str) -> String {
    let mut out = String::with_capacity(tag.len());
    let mut cursor = 0;
    while let Some(relative) = tag[cursor..].find(name) {
        let name_start = cursor + relative;
        let before_is_space =
            name_start == 0 || tag.as_bytes()[name_start - 1].is_ascii_whitespace();
        if !before_is_space {
            out.push_str(&tag[cursor..name_start + name.len()]);
            cursor = name_start + name.len();
            continue;
        }
        let after_name = name_start + name.len();
        let Some(equal_relative) = tag[after_name..].find('=') else {
            out.push_str(&tag[cursor..after_name]);
            cursor = after_name;
            continue;
        };
        let equal = after_name + equal_relative;
        let mut value_start = equal + 1;
        while value_start < tag.len() && tag.as_bytes()[value_start].is_ascii_whitespace() {
            value_start += 1;
        }
        let value_end = if value_start < tag.len()
            && (tag.as_bytes()[value_start] == b'"' || tag.as_bytes()[value_start] == b'\'')
        {
            let quote = if tag.as_bytes()[value_start] == b'"' {
                '"'
            } else {
                '\''
            };
            tag[value_start + 1..]
                .find(quote)
                .map(|offset| value_start + 1 + offset + 1)
                .unwrap_or(tag.len())
        } else {
            tag[value_start..]
                .find(|character: char| {
                    character.is_ascii_whitespace() || character == '>' || character == '/'
                })
                .map(|offset| value_start + offset)
                .unwrap_or(tag.len())
        };
        let mut remove_start = name_start;
        while remove_start > cursor && tag.as_bytes()[remove_start - 1].is_ascii_whitespace() {
            remove_start -= 1;
        }
        out.push_str(&tag[cursor..remove_start]);
        cursor = value_end;
    }
    out.push_str(&tag[cursor..]);
    out
}

fn write_image(
    out: &mut String,
    id: &str,
    frame: &Frame,
    style: &ImageStyle,
    data: &ImageData,
) -> Result<(), RenderError> {
    let (media_type, payload) = decode_data_uri(&data.src).ok_or_else(|| RenderError::Image {
        item_id: id.to_owned(),
        message: "src is not a valid base64 data URI".to_owned(),
    })?;
    let object_fit = match style.fit {
        printkit_core::ImageFit::Contain => "contain",
        printkit_core::ImageFit::Cover => "cover",
        printkit_core::ImageFit::Fill => "fill",
        printkit_core::ImageFit::None => "none",
    };
    let fit_class = if matches!(style.fit, printkit_core::ImageFit::None) {
        "item item-image image-fit-none"
    } else {
        "item item-image"
    };
    let _ = write!(
        out,
        "<div class=\"{}\" data-item-id=\"{}\" style=\"left:{}mm;top:{}mm;width:{}mm;height:{}mm;overflow:hidden;\">",
        fit_class,
        escape_html(id),
        frame.x,
        frame.y,
        frame.width,
        frame.height
    );
    if media_type.eq_ignore_ascii_case("image/svg+xml") {
        let svg = String::from_utf8(payload).map_err(|_| RenderError::Image {
            item_id: id.to_owned(),
            message: "SVG payload is not UTF-8".to_owned(),
        })?;
        let svg = svg_with_fit(
            &sanitize::sanitize_svg(&svg, Some(&item_scope(id))),
            style.fit,
        );
        out.push_str(&svg);
    } else {
        let _ = write!(
            out,
            "<img class=\"image-content\" src=\"{}\" style=\"object-fit:{object_fit};\">",
            escape_html(&data.src)
        );
    }
    out.push_str("</div>");
    Ok(())
}

/// Attribute selector scoping one item's CSS, with CSS string escapes for
/// quotes, backslashes and control characters.
fn item_scope(id: &str) -> String {
    let mut escaped = String::with_capacity(id.len());
    for character in id.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            character if character.is_control() => {
                let _ = write!(escaped, "\\{:x} ", character as u32);
            }
            character => escaped.push(character),
        }
    }
    format!("[data-item-id=\"{escaped}\"]")
}

/// Split a base64 data URI into its media type and decoded bytes.
fn decode_data_uri(value: &str) -> Option<(String, Vec<u8>)> {
    let rest = value.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let media_type = meta
        .split(';')
        .next()
        .filter(|value| !value.is_empty())?
        .to_owned();
    if !meta.ends_with(";base64") {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    Some((media_type, bytes))
}

fn write_vector(out: &mut String, kind: &str, id: &str, frame: &Frame, svg: String) {
    let _ = write!(
        out,
        "<div class=\"item item-{kind}\" data-item-id=\"{}\" style=\"left:{}mm;top:{}mm;width:{}mm;height:{}mm;\">",
        escape_html(id),
        frame.x,
        frame.y,
        frame.width,
        frame.height
    );
    out.push_str(&svg);
    out.push_str("</div>");
}

fn align_css(align: TextAlign) -> &'static str {
    match align {
        TextAlign::Left => "left",
        TextAlign::Center => "center",
        TextAlign::Right => "right",
    }
}

pub(crate) fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    escape_html_into(&mut escaped, value);
    escaped
}

pub(crate) fn escape_html_into(out: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(character),
        }
    }
}

#[cfg(test)]
mod tests {
    use printkit_core::{Document, Validated};
    use serde_json::{Value, json};

    use super::{render_document, render_document_with, to_html};
    use crate::Fetched;

    fn validated(value: Value) -> Validated {
        let document: Document = serde_json::from_value(value).unwrap();
        document.validated().unwrap()
    }

    fn page(spec: Value, items: Value) -> Value {
        json!({"spec": spec, "items": items})
    }

    fn text_item(id: &str, text: &str) -> Value {
        json!({
            "id": id,
            "type": "text",
            "frame": {"x": 20.0, "y": 30.0, "width": 100.0, "height": 10.0},
            "data": {"text": text},
        })
    }

    #[test]
    fn html_style_rules_are_scoped_to_the_item() {
        let html = to_html(&validated(json!({
            "version": 1,
            "pages": [page(json!({"size": "A4"}), json!([{
                "id": "box",
                "type": "html",
                "frame": {"x": 10.0, "y": 10.0, "width": 100.0, "height": 40.0},
                "data": {"html": "<style>body{color:red;display:none} .x{POSITION:FIXED;color:red}@page{size:A4}</style><b>x</b>"}
            }]))],
        })))
        .unwrap();
        assert!(
            html.contains("[data-item-id=\"box\"] .x{color:red}"),
            "{html}"
        );
        assert!(!html.contains("@page{size:A4}"), "{html}");
        assert!(!html.contains("display:none"), "{html}");
        assert!(
            !html.to_ascii_lowercase().contains("position:fixed"),
            "{html}"
        );
        assert!(
            !html.to_ascii_lowercase().contains("position : fixed"),
            "{html}"
        );
    }

    #[test]
    fn typed_page_sizes_follow_the_document_order() {
        let document = validated(json!({
            "version": 1,
            "pages": [
                page(json!({"size": "A4"}), json!([text_item("one", "one")])),
                page(json!({"size": "A5"}), json!([text_item("two", "two")])),
            ],
        }));
        let rendered = render_document_with(&document, &Fetched::default()).unwrap();
        assert_eq!(rendered.page_starts, vec![1, 2]);
        assert_eq!(rendered.page_sizes, vec![(210.0, 297.0), (148.0, 210.0)]);
    }

    #[test]
    fn text_style_fields_reach_the_html() {
        let html = to_html(&validated(json!({
            "version": 1,
            "pages": [page(json!({"size": "A4"}), json!([{
                "id": "styled",
                "type": "text",
                "frame": {"x": 10.0, "y": 10.0, "width": 100.0, "height": 20.0},
                "style": {
                    "bold": true, "italic": true, "underline": true,
                    "letterSpacing": 0.5, "rotation": 3.0,
                    "valign": "middle", "wrap": false
                },
                "data": {"text": "styled"}
            }]))],
        })))
        .unwrap();
        for needle in [
            "font-weight:700",
            "font-style:italic",
            "text-decoration:underline",
            "letter-spacing:0.5mm",
            "align-items:center",
            "white-space:nowrap",
            "transform:rotate(3",
        ] {
            assert!(html.contains(needle), "missing {needle}: {html}");
        }
    }

    #[test]
    fn emits_page_size_and_absolute_items() {
        let html = to_html(&validated(json!({
            "version": 1,
            "pages": [page(json!({"size": "A4"}), json!([text_item("title", "printkit")]))],
        })))
        .unwrap();
        assert!(html.contains("@page{size:210mm 297mm;margin:0;}"));
        assert!(html.contains("width:210mm;height:297mm"));
        assert!(html.contains("left:20mm;top:30mm;width:100mm;height:10mm"));
        assert!(html.contains("font-size:3.7mm"));
        assert!(html.contains(">printkit</span>"));
    }

    #[test]
    fn escapes_document_text() {
        let html = to_html(&validated(json!({
            "version": 1,
            "pages": [page(
                json!({"size": "A4"}),
                json!([text_item("title", "printkit <hello> & \"world\"")]),
            )],
        })))
        .unwrap();
        assert!(html.contains("printkit &lt;hello&gt; &amp; &quot;world&quot;"));
        assert!(!html.contains("<hello>"));
    }

    #[test]
    fn applies_margins_and_alignment() {
        let mut item = text_item("title", "centred");
        item["frame"] = json!({"x": 0.0, "y": 0.0, "width": 10.0, "height": 5.0});
        item["style"] = json!({"align": "center"});
        let html = to_html(&validated(json!({
            "version": 1,
            "pages": [page(
                json!({"size": "A5", "margins": {"top": 5.0, "left": 2.0}}),
                json!([item]),
            )],
        })))
        .unwrap();
        assert!(html.contains("width:148mm;height:210mm"));
        assert!(html.contains("padding:5mm 0mm 0mm 2mm"));
        assert!(html.contains("text-align:center"));
    }

    #[test]
    fn emits_every_page_and_breaks_between_them() {
        let html = to_html(&validated(json!({
            "version": 1,
            "pages": [
                page(json!({"size": "A4"}), json!([text_item("first", "one")])),
                page(
                    json!({"size": "custom", "width": 241, "height": 140}),
                    json!([text_item("second", "two")]),
                ),
            ],
        })))
        .unwrap();
        assert_eq!(html.matches("class=\"page\"").count(), 2);
        assert_eq!(html.matches("break-after:page").count(), 1);
        assert!(html.contains("width:241mm;height:140mm"));
        assert!(html.contains(">one</span>"));
        assert!(html.contains(">two</span>"));
    }

    #[test]
    fn renders_shapes_and_barcodes_as_inline_svg() {
        let html = to_html(&validated(json!({
            "version": 1,
            "pages": [page(
                json!({"size": "A4"}),
                json!([
                    {
                        "id": "shape",
                        "type": "shape",
                        "frame": {"x": 10.0, "y": 10.0, "width": 20.0, "height": 10.0},
                        "data": {"kind": "rect"},
                    },
                    {
                        "id": "code",
                        "type": "barcode",
                        "frame": {"x": 10.0, "y": 30.0, "width": 20.0, "height": 20.0},
                        "data": {"kind": "qr", "value": "PRINTKIT"},
                    },
                ]),
            )],
        })))
        .unwrap();
        assert_eq!(html.matches("<svg").count(), 2);
        assert_eq!(html.matches("<canvas").count(), 0);
        assert!(html.contains("class=\"item item-shape\""));
        assert!(html.contains("class=\"item item-barcode\""));
    }

    #[test]
    fn long_table_repeats_the_header_and_keeps_the_footer_on_the_last_page() {
        let rows: Vec<Value> = (0..120)
            .map(|index| json!({"no": format!("{index:03}"), "amount": "1.00"}))
            .collect();
        let document = json!({
            "version": 1,
            "pages": [page(
                json!({"size": "A4"}),
                json!([{
                    "id": "long",
                    "type": "table",
                    "frame": {"x": 20.0, "y": 20.0, "width": 170.0, "height": 277.0},
                    "data": {
                        "columns": [
                            {"key": "no", "title": "No", "width": 40.0},
                            {"key": "amount", "title": "Amount", "width": "auto"},
                        ],
                        "rows": rows,
                        "footer": {"label": "Total", "values": {"amount": "120.00"}},
                    },
                    "style": {"fontSize": 2.8, "rowHeight": 6.0},
                }]),
            )],
        });
        let rendered = render_document(&validated(document)).unwrap();
        assert_eq!(rendered.pages, 3);
        assert_eq!(rendered.html.matches("class=\"page\"").count(), 3);
        assert_eq!(rendered.html.matches(">No</th>").count(), 3);
        assert_eq!(rendered.html.matches("Total").count(), 1);
    }

    #[test]
    fn page_starts_account_for_table_continuation_pages() {
        let rows: Vec<Value> = (0..120)
            .map(|index| json!({"no": format!("{index:03}"), "amount": "1.00"}))
            .collect();
        let document = json!({
            "version": 1,
            "pages": [
                page(
                    json!({"size": "A4"}),
                    json!([{
                        "id": "long",
                        "type": "table",
                        "frame": {"x": 20.0, "y": 20.0, "width": 170.0, "height": 277.0},
                        "data": {
                            "columns": [
                                {"key": "no", "title": "No", "width": 40.0},
                                {"key": "amount", "title": "Amount", "width": "auto"},
                            ],
                            "rows": rows,
                        },
                        "style": {"fontSize": 2.8, "rowHeight": 6.0},
                    }]),
                ),
                page(json!({"size": "A4"}), json!([text_item("after", "after table")])),
            ],
        });
        let rendered = render_document(&validated(document)).unwrap();
        assert_eq!(rendered.pages, 4);
        assert_eq!(rendered.page_starts, vec![1, 4]);
    }

    #[test]
    fn escapes_a_document_font_family_in_the_style_attribute() {
        let mut item = text_item("title", "x");
        item["style"] = json!({"fontFamily": "bad\" onmouseover=\"alert(1)"});
        item["data"]["fontFile"] = json!("data:font/woff2;base64,AAAA");
        let html = to_html(&validated(json!({
            "version": 1,
            "pages": [page(json!({"size": "A4"}), json!([item]))],
        })))
        .unwrap();
        assert!(!html.contains("onmouseover=\""));
        assert!(html.contains("font-family:'bad\\&quot; onmouseover=\\&quot;alert(1)'"));
    }

    #[test]
    fn html_items_are_sanitised_and_clipped_to_their_frame() {
        let html = to_html(&validated(json!({
            "version": 1,
            "pages": [page(json!({"size": "A4"}), json!([{
                "id": "block",
                "type": "html",
                "frame": {"x": 10.0, "y": 10.0, "width": 100.0, "height": 40.0},
                "data": {"html": "<div onclick=\"x\">hello</div><script>alert(1)</script><img src=\"http://example.invalid/a.png\">"},
            }]))],
        })))
        .unwrap();
        assert!(html.contains("overflow:hidden"));
        assert!(html.contains(">hello</div>"));
        assert!(!html.contains("<script"));
        assert!(!html.contains("onclick"));
        assert!(!html.contains("http://"));
    }

    #[test]
    fn html_overflow_visible_keeps_the_frame_unclipped() {
        let mut item = json!({
            "id": "block",
            "type": "html",
            "frame": {"x": 10.0, "y": 10.0, "width": 100.0, "height": 40.0},
            "data": {"html": "<p>flow</p>"},
        });
        item["style"] = json!({"overflow": "visible"});
        let html = to_html(&validated(json!({
            "version": 1,
            "pages": [page(json!({"size": "A4"}), json!([item]))],
        })))
        .unwrap();
        assert!(html.contains("overflow:visible"));
    }

    #[test]
    fn image_items_render_data_uris_and_inline_svg() {
        let png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let svg = "data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciPjxzY3JpcHQ+YWxlcnQoMSk8L3NjcmlwdD48cmVjdCB3aWR0aD0iMTAiIGhlaWdodD0iMTAiLz48L3N2Zz4=";
        let html = to_html(&validated(json!({
            "version": 1,
            "pages": [page(json!({"size": "A4"}), json!([
                {
                    "id": "raster",
                    "type": "image",
                    "frame": {"x": 10.0, "y": 10.0, "width": 20.0, "height": 20.0},
                    "style": {"fit": "cover"},
                    "data": {"src": png},
                },
                {
                    "id": "vector",
                    "type": "image",
                    "frame": {"x": 40.0, "y": 10.0, "width": 20.0, "height": 20.0},
                    "data": {"src": svg},
                },
            ]))],
        })))
        .unwrap();
        assert!(html.contains("object-fit:cover"));
        assert!(html.contains(png));
        assert!(html.contains("<rect"));
        assert!(!html.contains("<script"));
        assert!(!html.contains("http://www.w3.org/2000/svg"));
    }

    #[test]
    fn a_default_ean13_quiet_zone_shortfall_reaches_the_warnings() {
        let rendered = render_document_with(
            &validated(json!({
                "version": 1,
                "pages": [page(json!({"size": "A4"}), json!([{
                    "id": "ean",
                    "type": "barcode",
                    "frame": {"x": 10.0, "y": 10.0, "width": 80.0, "height": 30.0},
                    "style": {},
                    "data": {"kind": "ean13", "value": "4006381333931"},
                }]))],
            })),
            &Fetched::default(),
        )
        .unwrap();
        assert!(
            rendered
                .warnings
                .iter()
                .any(|warning| warning.code == "W_BARCODE_QUIET_ZONE"),
            "{:?}",
            rendered.warnings
        );
    }

    #[test]
    fn svg_fit_sets_preserve_aspect_ratio_and_keeps_none_intrinsic() {
        let svg = "data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcxMjAwMC9zdmciIHdpZHRoPSIyMCIgaGVpZ2h0PSIxMCIgdmlld0JveD0iMCAwIDIwIDEwIj48cmVjdCB3aWR0aD0iMjAiIGhlaWdodD0iMTAiLz48L3N2Zz4=";
        let items = ["contain", "cover", "fill", "none"];
        for fit in items {
            let html = to_html(&validated(json!({
                "version": 1,
                "pages": [page(json!({"size": "A4"}), json!([{
                    "id": "svg",
                    "type": "image",
                    "frame": {"x": 10.0, "y": 10.0, "width": 20.0, "height": 20.0},
                    "style": {"fit": fit},
                    "data": {"src": svg},
                }]))],
            })))
            .unwrap();
            let expected = match fit {
                "cover" => "preserveAspectRatio=\"xMidYMid slice\"",
                "fill" => "preserveAspectRatio=\"none\"",
                _ => "preserveAspectRatio=\"xMidYMid meet\"",
            };
            assert!(html.contains(expected), "{fit}: {html}");
            if fit == "none" {
                assert!(html.contains("image-fit-none"), "{html}");
            }
        }
    }

    #[test]
    fn url_items_render_the_fetched_body() {
        let mut fetched = Fetched::new();
        fetched.insert("http://example.invalid/a.html", "<p>remote</p>");
        let document = validated(json!({
            "version": 1,
            "pages": [page(json!({"size": "A4"}), json!([{
                "id": "remote",
                "type": "html",
                "frame": {"x": 10.0, "y": 10.0, "width": 100.0, "height": 40.0},
                "data": {"url": "http://example.invalid/a.html"},
            }]))],
        }));
        let rendered = render_document_with(&document, &fetched).unwrap();
        assert!(rendered.html.contains("<p>remote</p>"));
        assert_eq!(
            render_document(&document).unwrap_err().to_string(),
            "html item remote: no fetched body for http://example.invalid/a.html"
        );
    }

    #[test]
    fn document_title_reaches_the_html_head() {
        let html = to_html(&validated(json!({
            "version": 1,
            "pages": [page(json!({"size": "A4"}), json!([text_item("body", "x")]))],
            "meta": {"title": "Invoice <A&B>"},
        })))
        .unwrap();
        assert!(
            html.contains("<title>Invoice &lt;A&amp;B&gt;</title>"),
            "{html}"
        );
    }

    #[test]
    fn page_background_is_emitted_before_items_and_on_continuations() {
        let background = "data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHdpZHRoPSIxMDAiIGhlaWdodD0iMTAwIj48cmVjdCB3aWR0aD0iMTAwJSIgaGVpZ2h0PSIxMDAlIiBmaWxsPSIjZmYwMDAwIi8+PC9zdmc+";
        let rows: Vec<serde_json::Value> = (0..120)
            .map(|index| json!({"no": format!("{index:03}"), "amount": "1.00"}))
            .collect();
        let html = to_html(&validated(json!({
            "version": 1,
            "pages": [{
                "spec": {"size": "A4", "background": background},
                "items": [{
                    "id": "long",
                    "type": "table",
                    "frame": {"x": 20.0, "y": 20.0, "width": 170.0, "height": 277.0},
                    "data": {
                        "columns": [
                            {"key": "no", "title": "No", "width": 40.0},
                            {"key": "amount", "title": "Amount", "width": "auto"}
                        ],
                        "rows": rows
                    },
                    "style": {"fontSize": 2.8, "rowHeight": 6.0}
                }]
            }],
        })))
        .unwrap();
        let pages = html.matches("class=\"page\"").count();
        let backgrounds = html.matches("class=\"page-background\"").count();
        assert!(pages > 1, "the table must span continuation pages");
        assert_eq!(
            backgrounds, pages,
            "every continuation page keeps the background"
        );
        let first = html.find("class=\"page-background\"").unwrap();
        let first_item = html.find("class=\"item item-table\"").unwrap();
        assert!(first < first_item, "the background must paint below items");
        // An SVG background is inline vector content, not an `<img>`: WebKit's
        // PDF painter can export a data-URI SVG image as a blank page.
        assert!(html.contains("<div class=\"page-background\"><svg"));
        assert!(html.contains("fill=\"#ff0000\""));
        assert!(!html.contains("<img class=\"page-background\""));
    }

    #[test]
    fn raster_page_background_keeps_the_image_element() {
        let background = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let html = to_html(&validated(json!({
            "version": 1,
            "pages": [page(
                json!({"size": "A4", "background": background}),
                json!([text_item("label", "x")]),
            )],
        })))
        .unwrap();
        assert!(html.contains(&format!(
            "<img class=\"page-background\" alt=\"\" src=\"{background}\" />"
        )));
        assert!(!html.contains("<div class=\"page-background\">"));
    }
}
