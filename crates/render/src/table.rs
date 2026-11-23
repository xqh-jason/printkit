//! Tables as HTML.
//!
//! WebKit's PDF export does not fragment an overflowing block, so a table that
//! does not fit its page is split here: rows are chunked by the declared row
//! height, the header is repeated when `headerRepeat` is set, and the footer
//! only lands on the last chunk. Each chunk is an absolutely positioned table,
//! so geometry stays millimetre-exact.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use printkit_core::{Dim, Frame, Margins, TableData, TableStyle, TextAlign};

use crate::fonts::RenderWarning;
use crate::html::{escape_html, escape_html_into};
use crate::shape::number;

/// A slice of table rows plus whether this fragment carries the header and the
/// final totals row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fragment {
    pub start: usize,
    pub end: usize,
    pub header: bool,
    pub footer: bool,
}

/// A page-worth of rows plus the planning problems found for this table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FragmentPlan {
    pub fragments: Vec<Fragment>,
    pub warnings: Vec<RenderWarning>,
}

/// Stable key for one table row, across every fragment of one table.
pub(crate) fn row_key(id: &str, index: usize) -> String {
    format!("{id}:row:{index}")
}

/// Stable key for the header, repeated verbatim on every fragment.
pub(crate) fn header_key(id: &str) -> String {
    format!("{id}:header")
}

/// Stable key for the totals row, which only lands on the last fragment.
pub(crate) fn footer_key(id: &str) -> String {
    format!("{id}:footer")
}

/// Split rows into page-sized fragments.
///
/// The footer row and the repeated header are reserved on every fragment; a
/// slightly smaller page is preferable to a row that overflows the paper.
/// `heights` carries WebKit's measured row height per `data-row-key`; a
/// missing key falls back to the declared height or the font-size estimate.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan(
    id: &str,
    data: &TableData,
    style: &TableStyle,
    frame: &Frame,
    page_height: f64,
    margins: &Margins,
    heights: Option<&BTreeMap<String, f64>>,
) -> FragmentPlan {
    let estimate = row_height(style);
    let border = if style.border_width.mm() > 0.0 {
        style.border_width.mm()
    } else {
        0.0
    };
    let measured = |key: &str| measured_height(heights, key, border);
    let header_height = measured(&header_key(id)).unwrap_or(estimate);
    let footer_height = if data.footer.is_some() {
        measured(&footer_key(id)).unwrap_or(estimate)
    } else {
        0.0
    };
    let bottom = margins.bottom.mm();
    // The first fragment must stop at the frame's bottom edge as well as the
    // paper's, so a short frame cannot push rows over its own boundary.
    let frame_bottom = frame.y.mm() + frame.height.mm();
    let paper_bottom = page_height - bottom;
    let first_available = (paper_bottom.min(frame_bottom) - frame.y.mm()).max(estimate);
    let later_available = (page_height - bottom - margins.top.mm()).max(estimate);

    let mut fragments = Vec::new();
    let mut warnings = Vec::new();
    let mut start = 0;
    while start < data.rows.len() {
        let first = fragments.is_empty();
        let available = if first {
            first_available
        } else {
            later_available
        };
        let header = first || style.header_repeat;
        let header_height = if header { header_height } else { 0.0 };
        // Row heights are measured, so a fragment ends where the body actually
        // fills up; an integer capacity would silently overrun the page.
        let mut used = header_height + footer_height;
        let mut end = start;
        while end < data.rows.len() {
            let height = measured(&row_key(id, end)).unwrap_or(estimate);
            if end > start && used + height > available {
                break;
            }
            used += height;
            end += 1;
            if used > available {
                // A single row taller than the body still gets its own page:
                // dropping it would be worse than one overfull sheet.
                break;
            }
        }
        if end == start + 1 && used > available {
            let height = (used - header_height - footer_height).max(0.0);
            warnings.push(RenderWarning {
                code: "W_TABLE_ROW_TOO_TALL",
                message: format!(
                    "table {id}: row {} is {height:.2} mm and does not fit the \
                     {available:.2} mm page body with its repeated header and footer",
                    start + 1
                ),
            });
        }
        fragments.push(Fragment {
            start,
            end,
            header,
            footer: end == data.rows.len(),
        });
        start = end;
    }
    if fragments.is_empty() {
        fragments.push(Fragment {
            start: 0,
            end: 0,
            header: true,
            footer: true,
        });
    }
    FragmentPlan {
        fragments,
        warnings,
    }
}

/// A measured row height plus the collapsed border WebKit draws outside it.
fn measured_height(heights: Option<&BTreeMap<String, f64>>, key: &str, border: f64) -> Option<f64> {
    let value = heights?.get(key).copied()?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    Some((value + border).max(0.1))
}

/// Render one fragment as an absolutely positioned table.
pub(crate) fn render(
    id: &str,
    frame: &Frame,
    style: &TableStyle,
    data: &TableData,
    fragment: &Fragment,
    top: f64,
) -> String {
    let width = frame.width.mm();
    let font_size = style.font_size.mm();
    let border = style.border_width.mm();
    let row_height = style.row_height.mm();

    let mut html = String::with_capacity(512);
    let _ = write!(
        html,
        "<div class=\"item item-table\" data-item-id=\"{}\" style=\"left:{}mm;top:{}mm;width:{}mm;\">",
        escape_html(id),
        number(frame.x.mm()),
        number(top),
        number(width)
    );
    let _ = write!(
        html,
        "<table class=\"data-table\" style=\"font-size:{}mm;\">",
        number(font_size)
    );

    // A collapsed border is painted outside the declared box: the columns must
    // sum to the frame minus one border, or the table's right edge overshoots.
    let content_width = if border > 0.0 {
        (width - border).max(0.0)
    } else {
        width
    };
    let widths = column_widths(data, content_width);
    let row_style = match row_height {
        Some(value) => format!("height:{}mm;", number(value)),
        None => String::new(),
    };
    let cell_border = if border > 0.0 {
        format!("border:{}mm solid {};", number(border), style.border_color)
    } else {
        "border:none;".to_owned()
    };

    if fragment.header {
        let _ = write!(
            html,
            "<thead><tr data-row-key=\"{}\">",
            escape_html(&header_key(id))
        );
        for (index, column) in data.columns.iter().enumerate() {
            let _ = write!(
                html,
                "<th style=\"width:{}mm;text-align:{};{}{}\">",
                number(widths[index]),
                align_css(column.align),
                row_style,
                cell_border
            );
            escape_html_into(&mut html, &column.title);
            html.push_str("</th>");
        }
        html.push_str("</tr></thead>");
    }

    html.push_str("<tbody>");
    for (row_index, row) in data.rows[fragment.start..fragment.end].iter().enumerate() {
        let zebra = if style.zebra && (fragment.start + row_index) % 2 == 1 {
            "background:#f0f0f0;"
        } else {
            ""
        };
        let _ = write!(
            html,
            "<tr data-row-key=\"{}\"",
            escape_html(&row_key(id, fragment.start + row_index))
        );
        if !zebra.is_empty() {
            let _ = write!(html, " style=\"{zebra}{row_style}\"");
        } else if !row_style.is_empty() {
            let _ = write!(html, " style=\"{row_style}\"");
        }
        html.push('>');
        for (index, column) in data.columns.iter().enumerate() {
            let _ = write!(
                html,
                "<td style=\"width:{}mm;text-align:{};{}\">",
                number(widths[index]),
                align_css(column.align),
                cell_border
            );
            escape_html_into(
                &mut html,
                row.get(&column.key).map(String::as_str).unwrap_or(""),
            );
            html.push_str("</td>");
        }
        html.push_str("</tr>");
    }

    if fragment.footer
        && let Some(footer) = &data.footer
    {
        let _ = write!(
            html,
            "<tr class=\"table-footer\" data-row-key=\"{}\" style=\"{row_style}",
            escape_html(&footer_key(id))
        );
        html.push_str("font-weight:bold;\">");
        for (index, column) in data.columns.iter().enumerate() {
            let value = footer
                .values
                .get(&column.key)
                .map(String::as_str)
                .unwrap_or(if index == 0 {
                    footer.label.as_str()
                } else {
                    ""
                });
            let _ = write!(
                html,
                "<td style=\"width:{}mm;text-align:{};{}\">",
                number(widths[index]),
                align_css(column.align),
                cell_border
            );
            escape_html_into(&mut html, value);
            html.push_str("</td>");
        }
        html.push_str("</tr>");
    }
    html.push_str("</tbody></table></div>");
    html
}

fn row_height(style: &TableStyle) -> f64 {
    match style.row_height.mm() {
        Some(value) => value.max(0.1),
        // Auto rows are content-sized; the estimate keeps pagination finite and
        // only affects where a fragment ends, not how rows are drawn.
        None => (style.font_size.mm() * 1.35).max(0.1),
    }
}

/// Widths in millimetres, normalised so the columns sum to the frame width.
/// Fixed columns keep their value when it fits; `auto` columns share the rest.
fn column_widths(data: &TableData, total: f64) -> Vec<f64> {
    let mut fixed = 0.0;
    let mut auto = 0_usize;
    for column in &data.columns {
        match column.width {
            Dim::Auto => auto += 1,
            Dim::Mm(value) => fixed += value.mm().max(0.0),
        }
    }
    let mut widths = Vec::with_capacity(data.columns.len());
    if auto == 0 {
        if fixed > total && fixed > 0.0 {
            for column in &data.columns {
                widths.push(match column.width {
                    Dim::Mm(value) => value.mm().max(0.0) * total / fixed,
                    Dim::Auto => 0.0,
                });
            }
        } else {
            for column in &data.columns {
                widths.push(match column.width {
                    Dim::Mm(value) => value.mm().max(0.0),
                    Dim::Auto => 0.0,
                });
            }
        }
        return widths;
    }
    if fixed > total && fixed > 0.0 {
        // Fixed columns overflow the frame. Give every column a fair share so
        // an `auto` column cannot collapse to zero and silently lose its
        // content; the fixed widths are scaled into the remaining budget.
        let auto_share = total / data.columns.len() as f64;
        let fixed_budget = (total - auto_share * auto as f64).max(0.0);
        let fixed_scale = fixed_budget / fixed;
        for column in &data.columns {
            widths.push(match column.width {
                Dim::Auto => auto_share,
                Dim::Mm(value) => value.mm().max(0.0) * fixed_scale,
            });
        }
        return widths;
    }
    let remaining = (total - fixed).max(0.0);
    let auto_share = remaining / auto as f64;
    for column in &data.columns {
        widths.push(match column.width {
            Dim::Auto => auto_share,
            Dim::Mm(value) => value.mm().max(0.0),
        });
    }
    widths
}

fn align_css(align: TextAlign) -> &'static str {
    match align {
        TextAlign::Left => "left",
        TextAlign::Center => "center",
        TextAlign::Right => "right",
    }
}

#[cfg(test)]
mod tests {
    use printkit_core::{Column, Dim, Length, TableData, TextAlign};

    use super::{Fragment, column_widths, header_key, plan, render, row_key};

    fn table(widths: Vec<Dim>) -> TableData {
        TableData {
            columns: widths
                .into_iter()
                .enumerate()
                .map(|(index, width)| Column {
                    key: format!("c{index}"),
                    title: String::new(),
                    width,
                    align: TextAlign::Left,
                })
                .collect(),
            rows: Vec::new(),
            footer: None,
        }
    }

    #[test]
    fn fixed_columns_wider_than_the_frame_shrink_and_leave_auto_columns_visible() {
        let widths = column_widths(&table(vec![Dim::Mm(Length::Mm(200.0)), Dim::Auto]), 150.0);
        assert_eq!(widths, vec![75.0, 75.0]);
        let sum: f64 = widths.iter().sum();
        assert!((sum - 150.0).abs() < 0.001);
    }

    #[test]
    fn auto_columns_share_the_space_left_by_fixed_columns() {
        let widths = column_widths(
            &table(vec![Dim::Mm(Length::Mm(40.0)), Dim::Auto, Dim::Auto]),
            150.0,
        );
        assert_eq!(widths, vec![40.0, 55.0, 55.0]);
    }

    #[test]
    fn measured_rows_drive_the_break_and_keep_stable_keys() {
        use std::collections::BTreeMap;

        use printkit_core::{Frame, Margins, TableStyle};

        let mut data = table(vec![Dim::Mm(Length::Mm(40.0)), Dim::Auto]);
        data.rows = (0..20)
            .map(|index| BTreeMap::from([("c0".to_owned(), index.to_string())]))
            .collect();
        let style = TableStyle {
            row_height: Dim::Auto,
            ..TableStyle::default()
        };
        let frame = Frame {
            x: Length::Mm(20.0),
            y: Length::Mm(20.0),
            width: Length::Mm(170.0),
            height: Length::Mm(277.0),
        };
        let mut heights = BTreeMap::new();
        heights.insert(header_key("t"), 5.0);
        for index in 0..20 {
            heights.insert(row_key("t", index), 20.0);
        }

        let plan = plan(
            "t",
            &data,
            &style,
            &frame,
            297.0,
            &Margins::default(),
            Some(&heights),
        );
        // The header (5.15mm with border) leaves room for 13 measured rows.
        assert_eq!(plan.fragments.len(), 2);
        assert_eq!((plan.fragments[0].start, plan.fragments[0].end), (0, 13));
        assert_eq!((plan.fragments[1].start, plan.fragments[1].end), (13, 20));
        assert!(plan.fragments[0].header && plan.fragments[1].header);
        assert!(!plan.fragments[0].footer && plan.fragments[1].footer);
        assert_ne!(row_key("t", 0), row_key("t", 13));
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn a_row_taller_than_the_page_body_gets_its_own_page() {
        use std::collections::BTreeMap;

        use printkit_core::{Frame, Margins, TableStyle};

        let mut data = table(vec![Dim::Mm(Length::Mm(40.0)), Dim::Auto]);
        data.rows = (0..2)
            .map(|index| BTreeMap::from([("c0".to_owned(), index.to_string())]))
            .collect();
        let style = TableStyle {
            row_height: Dim::Auto,
            ..TableStyle::default()
        };
        let frame = Frame {
            x: Length::Mm(20.0),
            y: Length::Mm(20.0),
            width: Length::Mm(170.0),
            height: Length::Mm(277.0),
        };
        let mut heights = BTreeMap::new();
        heights.insert(header_key("t"), 5.0);
        heights.insert(row_key("t", 0), 300.0);
        heights.insert(row_key("t", 1), 20.0);

        let plan = plan(
            "t",
            &data,
            &style,
            &frame,
            297.0,
            &Margins::default(),
            Some(&heights),
        );
        assert_eq!((plan.fragments[0].start, plan.fragments[0].end), (0, 1));
        assert_eq!(plan.warnings.len(), 1);
        assert_eq!(plan.warnings[0].code, "W_TABLE_ROW_TOO_TALL");
    }

    #[test]
    fn zebra_striping_uses_the_global_row_number_across_fragments() {
        let mut data = table(vec![Dim::Auto]);
        data.rows = (0..3)
            .map(|index| std::collections::BTreeMap::from([("c0".to_owned(), index.to_string())]))
            .collect();
        let frame = printkit_core::Frame {
            x: Length::Mm(0.0),
            y: Length::Mm(0.0),
            width: Length::Mm(50.0),
            height: Length::Mm(20.0),
        };
        let style = printkit_core::TableStyle {
            zebra: true,
            ..printkit_core::TableStyle::default()
        };
        let html = render(
            "t",
            &frame,
            &style,
            &data,
            &Fragment {
                start: 1,
                end: 3,
                header: false,
                footer: false,
            },
            0.0,
        );
        // Global row 1 is odd and must be striped even though it is the first
        // row of this continuation fragment.
        assert!(
            html.contains("data-row-key=\"t:row:1\" style=\"background:#f0f0f0"),
            "{html}"
        );
        assert!(
            !html.contains("data-row-key=\"t:row:2\" style=\"background:#f0f0f0"),
            "{html}"
        );
    }

    #[test]
    fn a_collapsed_border_does_not_widen_the_table_past_its_frame() {
        use printkit_core::{Frame, TableStyle};

        let data = table(vec![Dim::Mm(Length::Mm(40.0)), Dim::Auto]);
        let frame = Frame {
            x: Length::Mm(0.0),
            y: Length::Mm(0.0),
            width: Length::Mm(170.0),
            height: Length::Mm(50.0),
        };
        let style = TableStyle::default();
        let html = render(
            "t",
            &frame,
            &style,
            &data,
            &Fragment {
                start: 0,
                end: 0,
                header: true,
                footer: false,
            },
            0.0,
        );
        let expected = 170.0 - 0.15 - 40.0;
        assert!(
            html.contains(&format!("width:{}mm", super::number(expected))),
            "{html}"
        );
    }
}
