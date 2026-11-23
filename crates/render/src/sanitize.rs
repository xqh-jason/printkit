//! Sanitisation of document-supplied HTML.
//!
//! The renderer runs in a WebView with JavaScript disabled and no navigation,
//! but that is not enough: an external `src`, a `link` element or a CSS `url()`
//! would still ask the network process for bytes. This module removes the
//! script surface and every reference that is not carried inline, so a
//! document cannot turn into a request.
//!
//! The scanner is deliberately conservative. Anything it cannot parse is
//! dropped rather than passed through: a fragment that loses an attribute is
//! still printable, while one malformed tag that slips through is a hole.
//! CSS escapes and comments are removed before `url()` is inspected, because
//! `u\72l(...)` and `u/**/rl(...)` both name the same function to the parser.
//! Text nodes are decoded once and re-escaped: `&amp;` stays a literal
//! ampersand for the reader, while an entity-encoded `<script>` stays text.

/// Elements removed with their content, plus void elements that only load
/// resources. `style` is handled separately because its text is CSS, not raw
/// markup.
const DROPPED: &[&str] = &[
    "applet", "base", "embed", "frame", "frameset", "iframe", "link", "meta", "object", "script",
];

/// Attributes that load a resource when they carry a URL.
const URL_ATTRIBUTES: &[&str] = &[
    "action",
    "background",
    "cite",
    "codebase",
    "data",
    "formaction",
    "href",
    "longdesc",
    "manifest",
    "poster",
    "src",
    "xlink:href",
];

/// Presentation attributes whose value may hold a `url()` reference.
const PRESENTATION_URL_ATTRIBUTES: &[&str] = &[
    "background",
    "clip-path",
    "cursor",
    "fill",
    "filter",
    "marker-end",
    "marker-mid",
    "marker-start",
    "mask",
    "stroke",
];

/// Attributes always removed: they carry a list of URLs or embed a whole
/// document.
const DROPPED_ATTRIBUTES: &[&str] = &[
    "imagesrcset",
    "ping",
    "srcdoc",
    "srcset",
    "xmlns",
    "xmlns:xlink",
];

/// Sanitise one HTML fragment for embedding in a page item.
#[cfg(test)]
pub(crate) fn sanitize_fragment(input: &str) -> String {
    sanitize(input, None)
}

/// Sanitise a fragment and scope its `<style>` rules to one item.
pub(crate) fn sanitize_fragment_scoped(input: &str, scope: &str) -> String {
    sanitize(input, Some(scope))
}

/// Sanitise inline SVG bytes before embedding them directly in the page.
/// `scope` keeps an SVG `<style>` from restyling the host page.
pub(crate) fn sanitize_svg(input: &str, scope: Option<&str>) -> String {
    sanitize(input, scope)
}

fn sanitize(input: &str, scope: Option<&str>) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'<' {
            match reference_at(&input[index..]) {
                Some(Reference::Decoded {
                    character,
                    consumed,
                }) => {
                    escape_text_char(&mut out, character);
                    index += consumed;
                }
                Some(Reference::Verbatim { consumed }) => {
                    // Keep names this scanner does not decode (`&nbsp;`) so the
                    // browser's own table applies. The slice is entity-shaped,
                    // so it cannot carry markup.
                    out.push_str(&input[index..index + consumed]);
                    index += consumed;
                }
                None => {
                    let character = input[index..].chars().next().unwrap_or('\u{fffd}');
                    escape_text_char(&mut out, character);
                    index += character.len_utf8();
                }
            }
            continue;
        }

        if input[index..].starts_with("<!--") {
            index = match input[index + 4..].find("-->") {
                Some(offset) => index + 4 + offset + 3,
                None => bytes.len(),
            };
            continue;
        }
        if input[index..].starts_with("<!") || input[index..].starts_with("<?") {
            index = match tag_end(bytes, index + 1) {
                Some(end) => end + 1,
                None => bytes.len(),
            };
            continue;
        }

        let Some(end) = tag_end(bytes, index + 1) else {
            // An unterminated tag at the end cannot be emitted safely.
            break;
        };

        if input[index..].starts_with("</") {
            let name = tag_name(&input[index + 2..end]);
            if let Some(name) = name
                && !DROPPED.contains(&name.as_str())
            {
                out.push_str("</");
                out.push_str(&name);
                out.push('>');
            }
            index = end + 1;
            continue;
        }

        let body = &input[index + 1..end];
        let Some(name) = tag_name(body) else {
            index = end + 1;
            continue;
        };

        if name == "script" {
            index = skip_raw_text(input, end + 1, "script");
            continue;
        }
        if name == "style" {
            let (css, next) = raw_text(input, end + 1, "style");
            let css = sanitize_css(css);
            let css = scope.map_or(css.clone(), |scope| scope_css(&css, scope));
            out.push_str("<style>");
            out.push_str(&css);
            out.push_str("</style>");
            index = next;
            continue;
        }
        if DROPPED.contains(&name.as_str()) {
            index = end + 1;
            continue;
        }

        write_tag(&mut out, &name, body);
        index = end + 1;
    }
    out
}

/// Index of the `>` closing the tag whose body starts at `from`, ignoring
/// `>` inside quoted attribute values.
fn tag_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut index = from;
    let mut quote = None;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(open) => {
                if byte == open {
                    quote = None;
                }
            }
            None => match byte {
                b'"' | b'\'' => quote = Some(byte),
                b'>' => return Some(index),
                _ => {}
            },
        }
        index += 1;
    }
    None
}

/// The element name at the start of a tag body, lowercased.
fn tag_name(body: &str) -> Option<String> {
    let bytes = body.as_bytes();
    let mut end = 0;
    while end < bytes.len() && is_name_byte(bytes[end]) {
        end += 1;
    }
    if end == 0 {
        return None;
    }
    Some(body[..end].to_ascii_lowercase())
}

fn starts_with_scheme(input: &str, index: usize, scheme: &str) -> bool {
    input
        .as_bytes()
        .get(index..index + scheme.len())
        .is_some_and(|slice| slice.eq_ignore_ascii_case(scheme.as_bytes()))
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':')
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

/// Skip the content of a raw-text element and return the index after its
/// closing tag. An element without a closing tag consumes the rest: leaving
/// the remainder in the page would be exactly the content that was meant to be
/// removed.
fn skip_raw_text(input: &str, from: usize, name: &str) -> usize {
    raw_text(input, from, name).1
}

/// Split a raw-text element: the text before its closing tag, and the index
/// after the closing tag.
fn raw_text<'a>(input: &'a str, from: usize, name: &str) -> (&'a str, usize) {
    let closing = format!("</{name}");
    let Some(offset) = find_ascii_case_insensitive(&input[from..], &closing) else {
        return (&input[from..], input.len());
    };
    let close_start = from + offset;
    let after_close = match tag_end(input.as_bytes(), close_start + 2) {
        Some(end) => end + 1,
        None => input.len(),
    };
    (&input[from..close_start], after_close)
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&start| {
        haystack[start..start + needle.len()]
            .iter()
            .zip(needle)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

/// Escape one character of a text node. `&`, `<` and `>` are re-escaped so a
/// decoded reference cannot become markup or start a second reference.
fn escape_text_char(out: &mut String, character: char) {
    match character {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        _ => out.push(character),
    }
}

/// One `&...;` found in a text node.
enum Reference {
    /// A reference this scanner decodes; the character is emitted escaped.
    Decoded { character: char, consumed: usize },
    /// A well-formed reference this scanner leaves to the browser.
    Verbatim { consumed: usize },
}

/// Longest entity name honoured. A longer run cannot be a named reference and
/// would only make the scan quadratic on hostile input.
const MAX_REFERENCE_NAME_BYTES: usize = 32;

/// Classify one character reference at the start of `input`.
///
/// The five XML entities and numeric references are decoded and re-escaped.
/// Other well-formed names stay verbatim so the browser's larger table still
/// applies (`&nbsp;`); malformed ones are ordinary text, so the `&` is escaped.
fn reference_at(input: &str) -> Option<Reference> {
    let rest = input.strip_prefix('&')?;
    if let Some(hex) = rest.strip_prefix("#x").or_else(|| rest.strip_prefix("#X")) {
        return numeric_reference(hex, 16, 2);
    }
    if let Some(decimal) = rest.strip_prefix('#') {
        return numeric_reference(decimal, 10, 1);
    }
    let name = entity_name(rest)?;
    let consumed = 1 + name.len() + 1;
    let character = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => return Some(Reference::Verbatim { consumed }),
    };
    Some(Reference::Decoded {
        character,
        consumed,
    })
}

fn numeric_reference(input: &str, radix: u32, prefix_len: usize) -> Option<Reference> {
    let digits = reference_digits(input, radix)?;
    let consumed = 1 + prefix_len + digits.len() + 1;
    let value = u32::from_str_radix(digits, radix).ok();
    let character = match value {
        Some(0) | None => '\u{fffd}',
        Some(value) => char::from_u32(value).unwrap_or('\u{fffd}'),
    };
    Some(Reference::Decoded {
        character,
        consumed,
    })
}

fn entity_name(input: &str) -> Option<&str> {
    let bytes = input.as_bytes();
    if !bytes.first()?.is_ascii_alphabetic() {
        return None;
    }
    let limit = bytes.len().min(MAX_REFERENCE_NAME_BYTES);
    let mut end = 1;
    while end < limit && bytes[end].is_ascii_alphanumeric() {
        end += 1;
    }
    if end >= bytes.len() || bytes[end] != b';' {
        return None;
    }
    Some(&input[..end])
}

fn reference_digits(input: &str, radix: u32) -> Option<&str> {
    let bytes = input.as_bytes();
    let limit = bytes.len().min(MAX_REFERENCE_NAME_BYTES);
    let mut end = 0;
    while end < limit && bytes[end] != b';' && (bytes[end] as char).is_digit(radix) {
        end += 1;
    }
    if end == 0 || end >= bytes.len() || bytes[end] != b';' {
        return None;
    }
    Some(&input[..end])
}

/// Emit one filtered start tag.
fn write_tag(out: &mut String, name: &str, body: &str) {
    let bytes = body.as_bytes();
    let mut index = 0;
    while index < bytes.len() && is_name_byte(bytes[index]) {
        index += 1;
    }

    out.push('<');
    out.push_str(name);

    let mut self_closing = false;
    loop {
        while index < bytes.len() && is_space(bytes[index]) {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        if bytes[index] == b'/' {
            self_closing = true;
            index += 1;
            continue;
        }

        let name_start = index;
        while index < bytes.len()
            && !is_space(bytes[index])
            && !matches!(bytes[index], b'=' | b'/' | b'>')
        {
            index += 1;
        }
        let attribute = &body[name_start..index];
        if attribute.is_empty() {
            break;
        }

        while index < bytes.len() && is_space(bytes[index]) {
            index += 1;
        }
        let mut value = "";
        if index < bytes.len() && bytes[index] == b'=' {
            index += 1;
            while index < bytes.len() && is_space(bytes[index]) {
                index += 1;
            }
            value = match bytes.get(index) {
                Some(&quote @ (b'"' | b'\'')) => {
                    index += 1;
                    let value_start = index;
                    while index < bytes.len() && bytes[index] != quote {
                        index += 1;
                    }
                    let value = &body[value_start..index];
                    index = (index + 1).min(bytes.len());
                    value
                }
                _ => {
                    let value_start = index;
                    while index < bytes.len() && !is_space(bytes[index]) {
                        index += 1;
                    }
                    &body[value_start..index]
                }
            };
        }

        let lower = attribute.to_ascii_lowercase();
        if !attribute_bytes_valid(attribute)
            || lower.starts_with("on")
            || DROPPED_ATTRIBUTES.contains(&lower.as_str())
            || contains_javascript(value)
        {
            continue;
        }
        if lower == "style" {
            let css = sanitize_css(value);
            if !css.is_empty() {
                out.push_str(" style=\"");
                out.push_str(&escape_attribute(&css));
                out.push('"');
            }
            continue;
        }
        if URL_ATTRIBUTES.contains(&lower.as_str()) && !allowed_url(value) {
            continue;
        }
        let cleaned;
        if PRESENTATION_URL_ATTRIBUTES.contains(&lower.as_str()) {
            cleaned = sanitize_css(value);
            if cleaned.trim().is_empty() {
                continue;
            }
            value = &cleaned;
        }

        out.push(' ');
        out.push_str(&lower);
        out.push_str("=\"");
        out.push_str(&escape_attribute(value));
        out.push('"');
    }

    out.push_str(if self_closing { "/>" } else { ">" });
}

fn attribute_bytes_valid(attribute: &str) -> bool {
    !attribute.is_empty()
        && attribute
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.'))
}

fn contains_javascript(value: &str) -> bool {
    value.to_ascii_lowercase().contains("javascript:")
}

/// A URL attribute survives only when it points inside the document: an inline
/// `data:` payload or a fragment. Relative paths and every scheme that would
/// reach the network or the file system are dropped.
fn allowed_url(value: &str) -> bool {
    let value = value.trim();
    value.starts_with('#')
        || value
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
}

fn escape_attribute(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '"' => escaped.push_str("&quot;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// Remove CSS rules that can load a resource.
///
/// Comments, backslashes and angle brackets are stripped before the scan so
/// escape sequences cannot spell a function name the scan would miss, and so a
/// stray `</style>` cannot survive as markup-looking text.
fn sanitize_css(input: &str) -> String {
    scrub_position(&sanitize_css_arguments(input, false))
}

/// `url_argument` marks the body of a function that accepts a URL string.
/// A quoted argument there is a resource reference, so only inline payloads
/// survive; elsewhere a quoted string is ordinary CSS content.
fn sanitize_css_arguments(input: &str, url_argument: bool) -> String {
    let bytes = input.as_bytes();
    let mut cleaned = String::with_capacity(input.len());
    let mut index = 0;
    while index < bytes.len() {
        if input[index..].starts_with("/*") {
            index = match input[index + 2..].find("*/") {
                Some(offset) => index + 2 + offset + 2,
                None => bytes.len(),
            };
            continue;
        }
        if starts_with_scheme(input, index, "http://")
            || starts_with_scheme(input, index, "https://")
        {
            // An external address is dropped wherever it appears: inside
            // `url()`, inside a string argument, or in an uncommon function.
            while index < bytes.len()
                && !is_space(bytes[index])
                && !matches!(bytes[index], b')' | b';' | b'"' | b'\'' | b'}')
            {
                index += 1;
            }
            continue;
        }
        let character = input[index..].chars().next().unwrap_or('\u{fffd}');
        index += character.len_utf8();
        match character {
            '\\' => {
                // Drop the escape marker; the following character stays as
                // plain text, which cannot form an escaped function name.
                if let Some(next) = input[index..].chars().next() {
                    cleaned.push(next);
                    index += next.len_utf8();
                }
            }
            '<' | '>' => {}
            _ => cleaned.push(character),
        }
    }

    let mut out = String::with_capacity(cleaned.len());
    let mut index = 0;
    while index < cleaned.len() {
        if url_argument && let Some(quote) = css_quote(&cleaned, index) {
            let value_start = index + 1;
            let Some(offset) = cleaned[value_start..].find(quote) else {
                // An unterminated string cannot be inspected safely.
                break;
            };
            let value_end = value_start + offset;
            if allowed_url(cleaned[value_start..value_end].trim()) {
                out.push_str(&cleaned[index..=value_end]);
            }
            index = value_end + 1;
            continue;
        }
        if let Some(offset) = find_ascii_case_insensitive(&cleaned[index..], "@import") {
            out.push_str(&cleaned[index..index + offset]);
            index += offset + "@import".len();
            index = skip_until(&cleaned, index, ';');
            continue;
        }
        if let Some(offset) = find_ascii_case_insensitive(&cleaned[index..], "url(") {
            out.push_str(&cleaned[index..index + offset]);
            let open = index + offset + 3;
            let end = function_end(&cleaned, open);
            let inner_end = if end > open + 1 && cleaned.as_bytes().get(end - 1) == Some(&b')') {
                end - 1
            } else {
                end
            };
            let inner = &cleaned[open + 1..inner_end.max(open + 1)];
            let inner = inner.trim().trim_matches('"').trim_matches('\'').trim();
            if allowed_url(inner) {
                out.push_str(&cleaned[index + offset..end]);
            }
            index = end;
            continue;
        }
        if let Some(open_offset) = url_argument_function(&cleaned[index..]) {
            let open = index + open_offset;
            let end = function_end(&cleaned, open);
            let inner_end = if end > open + 1 && cleaned.as_bytes().get(end - 1) == Some(&b')') {
                end - 1
            } else {
                end
            };
            let body = &cleaned[open + 1..inner_end.max(open + 1)];
            out.push_str(&cleaned[index..=open]);
            out.push_str(&sanitize_css_arguments(body, true));
            if end > open + 1 && cleaned.as_bytes().get(end - 1) == Some(&b')') {
                out.push(')');
            }
            index = end;
            continue;
        }
        out.push_str(&cleaned[index..]);
        break;
    }
    out
}

/// Remove `position: fixed` / `sticky` (any case and spacing) so an item
/// cannot escape its page box or the viewport. `absolute` stays: it is
/// contained by the item's positioned wrapper and clipped.
fn scrub_position(css: &str) -> String {
    let bytes = css.as_bytes();
    let mut out = String::with_capacity(css.len());
    let mut copied = 0;
    let mut index = 0;
    let mut quote = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(active) = quote {
            if byte == b'\\' {
                index = (index + 2).min(bytes.len());
                continue;
            }
            if byte == active {
                quote = None;
            }
            index += 1;
            continue;
        }
        if byte == b'"' || byte == b'\'' {
            quote = Some(byte);
            index += 1;
            continue;
        }
        let word = css.get(index..index + 8);
        if word.is_some_and(|word| word.eq_ignore_ascii_case("position")) {
            let after = index + 8;
            if bytes
                .get(after)
                .is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'-' && *byte != b'_')
            {
                let previous = css[..index].chars().rev().find(|ch| !ch.is_whitespace());
                if matches!(previous, None | Some('{') | Some(';')) {
                    let mut colon = after;
                    while colon < bytes.len() && bytes[colon].is_ascii_whitespace() {
                        colon += 1;
                    }
                    if bytes.get(colon) == Some(&b':') {
                        let mut value_start = colon + 1;
                        while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace()
                        {
                            value_start += 1;
                        }
                        let mut end = value_start;
                        let mut inner_quote = None;
                        let mut parens = 0_usize;
                        while end < bytes.len() {
                            let byte = bytes[end];
                            if let Some(active) = inner_quote {
                                if byte == active {
                                    inner_quote = None;
                                }
                                end += 1;
                                continue;
                            }
                            match byte {
                                b'"' | b'\'' => inner_quote = Some(byte),
                                b'(' => parens += 1,
                                b')' => parens = parens.saturating_sub(1),
                                b';' | b'}' if parens == 0 => break,
                                _ => {}
                            }
                            end += 1;
                        }
                        if !safe_position(&css[value_start..end]) {
                            out.push_str(&css[copied..index]);
                            copied = end;
                            if bytes.get(end) == Some(&b';') {
                                copied += 1;
                            }
                        }
                        index = copied.max(end);
                        continue;
                    }
                }
            }
        }
        index += 1;
    }
    out.push_str(&css[copied..]);
    out
}

fn safe_position(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    let head = lower.split('!').next().unwrap_or_default().trim();
    matches!(
        head,
        "static" | "relative" | "absolute" | "inherit" | "initial" | "unset" | "revert"
    )
}

/// The opening quote at `index`, when the byte there starts a CSS string.
fn css_quote(input: &str, index: usize) -> Option<char> {
    match input.as_bytes().get(index)? {
        b'"' => Some('"'),
        b'\'' => Some('\''),
        _ => None,
    }
}

/// Offset of the `(` for a function whose argument may be a URL string.
fn url_argument_function(input: &str) -> Option<usize> {
    const FUNCTIONS: &[&str] = &["-webkit-image-set", "image-set", "src", "image"];
    let mut earliest: Option<usize> = None;
    for name in FUNCTIONS {
        let mut from = 0;
        while from < input.len() {
            let Some(offset) = find_ascii_case_insensitive(&input[from..], name) else {
                break;
            };
            let start = from + offset;
            let open = start + name.len();
            if function_boundary(input, start) && input.as_bytes().get(open) == Some(&b'(') {
                earliest = Some(earliest.map_or(open, |current| current.min(open)));
                break;
            }
            from = open;
        }
    }
    earliest
}

/// A function name starts where the preceding byte cannot continue an identifier.
fn function_boundary(input: &str, start: usize) -> bool {
    match start.checked_sub(1) {
        None => true,
        Some(index) => {
            let byte = input.as_bytes()[index];
            !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') || byte >= 0x80)
        }
    }
}

fn skip_until(input: &str, from: usize, byte: char) -> usize {
    match input[from..].find(byte) {
        Some(offset) => from + offset + 1,
        None => input.len(),
    }
}

/// Index after the `)` matching the `(` at `open`, honouring quoted arguments.
fn function_end(input: &str, open: usize) -> usize {
    let bytes = input.as_bytes();
    let mut index = open + 1;
    let mut quote = None;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(open_quote) => {
                if byte == open_quote {
                    quote = None;
                }
            }
            None => match byte {
                b'"' | b'\'' => quote = Some(byte),
                b')' => return index + 1,
                _ => {}
            },
        }
        index += 1;
    }
    bytes.len()
}

/// Prefix every rule with the item scope and drop at-rules that can escape it.
fn scope_css(input: &str, scope: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let Some(offset) = input[index..].find('{') else {
            break;
        };
        let open = index + offset;
        let selector = input[index..open].trim();
        if selector.starts_with('@') {
            index = skip_block(bytes, open);
            continue;
        }
        let Some(offset) = input[open + 1..].find('}') else {
            break;
        };
        let close = open + 1 + offset;
        let body = &input[open + 1..close];
        let scoped = selector
            .split(',')
            .filter_map(|part| {
                let part = part.trim();
                if part.is_empty() {
                    return None;
                }
                if targets_page_root(part) {
                    // `html` / `body` / `:root` rules belong to the host page,
                    // not to the item. Mapping them onto the scope would let
                    // `body{display:none}` hide the item itself.
                    return None;
                }
                Some(format!("{scope} {part}"))
            })
            .collect::<Vec<_>>()
            .join(",");
        if !scoped.is_empty() {
            out.push_str(&scoped);
            out.push('{');
            out.push_str(body);
            out.push('}');
        }
        index = close + 1;
    }
    out
}

/// Offset just past the block that starts at `open`.
fn skip_block(bytes: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    let mut index = open;
    while index < bytes.len() {
        match bytes[index] {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return index + 1;
                }
            }
            _ => {}
        }
        index += 1;
    }
    bytes.len()
}

/// Whether a selector's first compound targets the host page root.
fn targets_page_root(selector: &str) -> bool {
    let first = selector
        .split(|character: char| character.is_whitespace() || matches!(character, '>' | '+' | '~'))
        .next()
        .unwrap_or_default();
    first.eq_ignore_ascii_case("html")
        || first.eq_ignore_ascii_case("body")
        || first.eq_ignore_ascii_case(":root")
}

#[cfg(test)]
mod tests {
    #[test]
    fn css_rules_are_scoped_to_their_item() {
        let out = crate::sanitize::sanitize_fragment_scoped(
            "<style>body{color:red;display:none} .x{POSITION : FIXED;color:red}@page{size:A4}</style><b>x</b>",
            "[data-item-id=\"a\"]",
        );
        assert!(out.contains("[data-item-id=\"a\"] .x{color:red}"), "{out}");
        assert!(!out.contains("position"), "{out}");
        assert!(!out.contains("body"), "{out}");
        assert!(!out.to_ascii_lowercase().contains("fixed"), "{out}");
        assert!(!out.contains("@page"), "{out}");
    }

    #[test]
    fn inline_styles_cannot_pin_or_stick_the_item() {
        let out = crate::sanitize::sanitize_fragment_scoped(
            "<div style=\"POSITION:FIXED;top:10mm;colour:red\">x</div><span style=\"position : sticky\">y</span>",
            "[data-item-id=\"a\"]",
        );
        assert!(!out.contains('\''), "{out}");
        assert!(!out.to_ascii_lowercase().contains("fixed"), "{out}");
        assert!(!out.to_ascii_lowercase().contains("sticky"), "{out}");
    }

    #[test]
    fn svg_styles_are_scoped_and_page_rules_dropped() {
        let svg = sanitize_svg(
            "<svg><style>body{display:none} .a{fill:red}@import url(http://x/a.css)</style><rect class=\"a\" fill=\"url(http://x/b.png)\"/></svg>",
            Some("[data-item-id=\"a\"]"),
        );
        assert!(!svg.contains("display:none"), "{svg}");
        assert!(!svg.contains("http://"), "{svg}");
        assert!(svg.contains("[data-item-id=\"a\"] .a{fill:red}"), "{svg}");
    }

    use super::{sanitize_fragment, sanitize_svg};

    #[test]
    fn strips_script_elements_and_their_content() {
        let html =
            sanitize_fragment("<p>ok</p><script>alert('x')</script><script src=\"http://x\">");
        assert_eq!(html, "<p>ok</p>");
        assert!(!html.contains("<script"));
    }

    #[test]
    fn drops_handler_attributes_and_javascript_urls() {
        let html = sanitize_fragment(
            "<a href=\"javascript:alert(1)\" onclick=\"alert(2)\" OnMouseOver=\"x\">link</a>",
        );
        assert_eq!(html, "<a>link</a>");
    }

    #[test]
    fn drops_external_sources_and_keeps_inline_payloads() {
        let html = sanitize_fragment(
            "<img src=\"http://x/a.png\"><img src=\"data:image/png;base64,AA==\"><a href=\"#top\">t</a>",
        );
        assert!(!html.contains("http://"));
        assert!(html.contains("data:image/png;base64,AA=="));
        assert!(html.contains("href=\"#top\""));
    }

    #[test]
    fn drops_link_meta_and_embedding_elements() {
        let html = sanitize_fragment(
            "<link rel=\"stylesheet\" href=\"http://x/a.css\"><meta http-equiv=\"refresh\" content=\"0\"><iframe src=\"http://x\"></iframe>",
        );
        assert!(!html.contains("link"));
        assert!(!html.contains("meta"));
        assert!(!html.contains("iframe"));
    }

    #[test]
    fn removes_css_imports_and_external_urls_but_keeps_data_urls() {
        let html = sanitize_fragment(
            "<style>@import url(http://x/a.css); .a{background:url(http://x/b.png)} .b{background:url(data:image/png;base64,AA==)}</style>",
        );
        assert!(!html.contains("http://"));
        assert!(html.contains("data:image/png;base64,AA=="));
    }

    #[test]
    fn css_escapes_cannot_hide_a_url_function() {
        let html = sanitize_fragment("<style>.a{background:u\\72l(http://x/b.png)}</style>");
        assert!(!html.contains("http://"));
    }

    #[test]
    fn sanitises_inline_svg() {
        let svg = sanitize_svg(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" onload=\"x\"><script>alert(1)</script><rect width=\"10\" height=\"10\"/></svg>",
            None,
        );
        assert!(svg.starts_with("<svg"));
        assert!(!svg.contains("script"));
        assert!(!svg.contains("onload"));
        assert!(svg.contains("<rect"));
    }

    #[test]
    fn text_references_are_decoded_and_markup_is_reescaped() {
        let html = sanitize_fragment(
            "<p>&amp; &lt; &gt; &quot; &apos; &#65; &#x4e2d; &unknown; &nbsp;</p>",
        );
        assert_eq!(
            html,
            "<p>&amp; &lt; &gt; \" ' A \u{4e2d} &unknown; &nbsp;</p>"
        );
    }

    #[test]
    fn references_cannot_smuggle_a_tag() {
        let html = sanitize_fragment("&lt;script&gt;alert(1)&lt;/script&gt;&#60;img src=x&#62;");
        assert_eq!(
            html,
            "&lt;script&gt;alert(1)&lt;/script&gt;&lt;img src=x&gt;"
        );
        assert!(!html.contains('<'));
    }

    #[test]
    fn mixed_case_tags_are_normalised_and_filtered() {
        let html =
            sanitize_fragment("<ScRiPt>alert(1)</sCrIpT><DiV OnClick=\"alert(2)\">kept</DiV>");
        assert_eq!(html, "<div>kept</div>");
        assert!(!html.to_ascii_lowercase().contains("script"));
        assert!(!html.to_ascii_lowercase().contains("onclick"));
    }

    #[test]
    fn quoted_and_backticked_payloads_are_dropped() {
        let html = sanitize_fragment(
            "<a href=\"javascript:alert(3)\" onclick='alert(4)'>link</a><img src=`javascript:alert(5)`>",
        );
        assert_eq!(html, "<a>link</a><img>");
        assert!(!html.contains("javascript:"));
    }

    #[test]
    fn an_unclosed_script_consumes_the_rest() {
        let html = sanitize_fragment("<p>before</p><script>alert(6)");
        assert_eq!(html, "<p>before</p>");
        assert!(!html.contains("alert"));
    }

    #[test]
    fn an_unterminated_tag_is_dropped_with_its_tail() {
        let html = sanitize_fragment("<p>before</p><script");
        assert_eq!(html, "<p>before</p>");
    }

    #[test]
    fn javascript_inside_a_comment_is_removed_with_the_comment() {
        let html = sanitize_fragment("<!-- javascript:alert(7) --><p>after</p>");
        assert_eq!(html, "<p>after</p>");
        assert!(!html.contains("javascript:"));
    }

    #[test]
    fn entity_encoded_javascript_urls_are_dropped() {
        let html = sanitize_fragment(
            "<a href=\"&#106;avascript:alert(8)\">a</a><a href=\"jav&#x61;script:alert(9)\">b</a><p>safe</p>",
        );
        assert_eq!(html, "<a>a</a><a>b</a><p>safe</p>");
        assert!(!html.contains("javascript:"));
    }

    #[test]
    fn a_nested_svg_script_is_removed() {
        let html = sanitize_fragment(
            "<svg><script>alert(10)</script><rect width=\"10\" height=\"10\"/><circle r=\"2\"/></svg>",
        );
        assert_eq!(
            html,
            "<svg><rect width=\"10\" height=\"10\"/><circle r=\"2\"/></svg>"
        );
        assert!(!html.contains("script"));
    }

    #[test]
    fn overlong_attributes_and_scripts_do_not_panic() {
        let padding = "a".repeat(200_000);
        let input = format!(
            "<div title=\"{padding}\" onclick=\"x\">kept</div><script>{padding}</script>after"
        );
        let html = sanitize_fragment(&input);
        assert!(html.starts_with("<div title=\""));
        assert!(html.contains(">kept</div>"));
        assert!(html.ends_with("after"));
        assert!(!html.contains("onclick"));
        assert!(!html.contains("<script"));
    }
    #[test]
    fn url_strings_inside_image_functions_cannot_load_resources() {
        let html = sanitize_fragment(
            "<style>.a{background-image:image-set(\"//example.invalid/a.png\" 1x)} @font-face{src:\"http://example.invalid/f.woff2\"}</style>",
        );
        assert!(!html.contains("example.invalid"));
        assert!(!html.contains("http://"));
        assert!(!html.contains("//"));
    }

    #[test]
    fn image_functions_keep_inline_data_strings_and_plain_content() {
        let html = sanitize_fragment(
            "<style>.a{background-image:image-set(\"data:image/png;base64,AA==\" 1x)} .b{content:\"a/b\"}</style>",
        );
        assert!(html.contains("data:image/png;base64,AA=="));
        assert!(html.contains("content:\"a/b\""));
    }
}
