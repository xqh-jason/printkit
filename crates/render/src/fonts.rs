//! Bundled font faces and coverage checks.
//!
//! The two families ship as subset WOFF2 files built by
//! `scripts/subset-fonts.sh`. They are compiled into the binary and inlined as
//! base64 data URIs, so the renderer never resolves a file, a network URL or a
//! system font. A document that names any other family gets a
//! `W_FONT_UNKNOWN` warning and is laid out with the default bundled face
//! instead of silently falling back to whatever the host platform supplies.
//!
//! Coverage is checked against the manifest generated next to each subset; the
//! manifests differ per face because the monospace face carries Latin only.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::LazyLock;

use printkit_core::{Document, Item, Validated};

/// Family used when a text item does not resolve to an available face and as
/// the inherited face of tables and other flow content.
///
/// This is the name table of the bundled subset, renamed away from the OFL
/// Reserved Font Name "Source" by `scripts/subset-fonts.sh`.
pub(crate) const DEFAULT_FAMILY: &str = "printkit Han Sans SC";

/// Document-level family name used by `document-v1` defaults before the OFL
/// rename. It stays accepted so existing documents keep resolving, but it is
/// never written into CSS or into an embedded font: `resolved_family` maps it
/// to [`DEFAULT_FAMILY`].
const LEGACY_FAMILY: &str = "Source Han Sans SC";

/// Family used by barcode text and monospace content.
pub(crate) const MONO_FAMILY: &str = "Noto Sans Mono";

const SOURCE_HAN_SANS_SC_WOFF2: &[u8] =
    include_bytes!("../../../assets/fonts/SourceHanSansSC-Regular-subset.woff2");
const SOURCE_HAN_SANS_SC_COVERAGE: &str =
    include_str!("../../../assets/fonts/SourceHanSansSC-Regular-subset.charset.txt");
const NOTO_SANS_MONO_WOFF2: &[u8] =
    include_bytes!("../../../assets/fonts/NotoSansMono-Regular-subset.woff2");
const NOTO_SANS_MONO_COVERAGE: &str =
    include_str!("../../../assets/fonts/NotoSansMono-Regular-subset.charset.txt");

// Base64 and coverage sets cost a few milliseconds and several megabytes; they
// are built once per process and shared by every render.
static SOURCE_HAN_SANS_SC_BASE64: LazyLock<String> =
    LazyLock::new(|| encode_base64(SOURCE_HAN_SANS_SC_WOFF2));
static SOURCE_HAN_SANS_SC_CHARSET: LazyLock<HashSet<char>> =
    LazyLock::new(|| SOURCE_HAN_SANS_SC_COVERAGE.chars().collect());
static NOTO_SANS_MONO_BASE64: LazyLock<String> =
    LazyLock::new(|| encode_base64(NOTO_SANS_MONO_WOFF2));
static NOTO_SANS_MONO_CHARSET: LazyLock<HashSet<char>> =
    LazyLock::new(|| NOTO_SANS_MONO_COVERAGE.chars().collect());

struct BundledFace {
    family: &'static str,
    base64: &'static LazyLock<String>,
    charset: &'static LazyLock<HashSet<char>>,
}

static BUNDLED: [BundledFace; 2] = [
    BundledFace {
        family: DEFAULT_FAMILY,
        base64: &SOURCE_HAN_SANS_SC_BASE64,
        charset: &SOURCE_HAN_SANS_SC_CHARSET,
    },
    BundledFace {
        family: MONO_FAMILY,
        base64: &NOTO_SANS_MONO_BASE64,
        charset: &NOTO_SANS_MONO_CHARSET,
    },
];

impl BundledFace {
    fn covers(&self, character: char) -> bool {
        self.charset.contains(&character)
    }
}

fn bundled(family: &str) -> Option<&'static BundledFace> {
    if family == LEGACY_FAMILY {
        return Some(&BUNDLED[0]);
    }
    BUNDLED.iter().find(|face| face.family == family)
}

/// The family actually written into CSS for a text item.
///
/// A face declared by `data.fontFile` is trusted even when it is not bundled:
/// the document supplies the bytes, and the family name is the one the user
/// chose for that embedded face. Everything else resolves to a bundled face;
/// the schema-v1 default name is accepted as an alias of [`DEFAULT_FAMILY`].
pub(crate) fn resolved_family(family: &str, has_font_file: bool) -> &str {
    if has_font_file {
        family
    } else {
        bundled(family).map_or(DEFAULT_FAMILY, |face| face.family)
    }
}

/// CSS for the bundled faces plus every embedded font a document declares.
///
/// Bundled rules come first and user rules last, so an embedded font with a
/// bundled family name overrides that family, as the document contract says.
pub(crate) fn face_css(document: &Document) -> String {
    let mut out = String::new();
    for face in &BUNDLED {
        let _ = write!(
            out,
            "@font-face{{font-family:\"{}\";src:url(data:font/woff2;base64,{}) format(\"woff2\");font-weight:normal;font-style:normal;}}",
            css_string(face.family),
            face.base64.as_str()
        );
    }
    let mut declared: HashMap<&str, &str> = HashMap::new();
    for page in &document.pages {
        for item in &page.items {
            if let Item::Text { style, data, .. } = item
                && let Some(file) = &data.font_file
            {
                declared.insert(style.font_family.as_str(), file.as_str());
            }
        }
    }
    let mut families: Vec<(&str, &str)> = declared.into_iter().collect();
    // HashMap iteration order is not stable; sorting keeps the HTML deterministic.
    families.sort_unstable();
    for (family, file) in families {
        let _ = write!(
            out,
            "@font-face{{font-family:\"{}\";src:url(\"{}\");}}",
            css_string(family),
            css_string(file)
        );
    }
    out
}

/// A non-fatal problem found while serialising, reported in
/// [`crate::Rendered::warnings`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderWarning {
    /// Stable warning code: `W_FONT_UNKNOWN`, `W_MISSING_GLYPH`,
    /// `W_BARCODE_QUIET_ZONE` or `W_TABLE_ROW_TOO_TALL`.
    pub code: &'static str,
    /// Human-readable detail; item- and table-level warnings name the id they
    /// concern.
    pub message: String,
}

/// `W_FONT_UNKNOWN` and `W_MISSING_GLYPH` findings for one document.
///
/// Missing characters are collected per family across the whole document
/// instead of per item; the warning names the codepoints once, which is what a
/// user needs to fix the font choice.
pub(crate) fn warnings(validated: &Validated) -> Vec<RenderWarning> {
    let document = validated.document();
    let mut unknown: BTreeSet<String> = BTreeSet::new();
    let mut missing: HashMap<&'static str, BTreeSet<char>> = HashMap::new();
    let mut check = |family: &str, has_font_file: bool, text: &str| {
        let face = if has_font_file {
            None
        } else {
            match bundled(family) {
                Some(face) => Some(face),
                None => {
                    unknown.insert(family.to_owned());
                    bundled(DEFAULT_FAMILY)
                }
            }
        };
        if let Some(face) = face {
            let uncovered: BTreeSet<char> = text
                .chars()
                .filter(|c| !c.is_whitespace() && !c.is_control() && !face.covers(*c))
                .collect();
            if !uncovered.is_empty() {
                missing.entry(face.family).or_default().extend(uncovered);
            }
        }
    };
    for page in &document.pages {
        for item in &page.items {
            match item {
                Item::Text { style, data, .. } => {
                    check(&style.font_family, data.font_file.is_some(), &data.text);
                }
                Item::Table { data, .. } => {
                    let mut text = String::new();
                    for column in &data.columns {
                        text.push_str(&column.title);
                    }
                    for row in &data.rows {
                        for value in row.values() {
                            text.push_str(value);
                        }
                    }
                    if let Some(footer) = &data.footer {
                        text.push_str(&footer.label);
                        for value in footer.values.values() {
                            text.push_str(value);
                        }
                    }
                    check(DEFAULT_FAMILY, false, &text);
                }
                Item::Barcode { style, data, .. } if style.show_text => {
                    check(MONO_FAMILY, false, &data.value);
                }
                _ => {}
            }
        }
    }

    let mut warnings: Vec<RenderWarning> = unknown
        .into_iter()
        .map(|family| RenderWarning {
            code: "W_FONT_UNKNOWN",
            message: format!(
                "font family \"{family}\" is not bundled and has no fontFile; \
                 text is laid out with {DEFAULT_FAMILY}"
            ),
        })
        .collect();
    for (family, codepoints) in missing {
        let list: Vec<String> = codepoints
            .iter()
            .map(|c| format!("U+{:04X}", *c as u32))
            .collect();
        warnings.push(RenderWarning {
            code: "W_MISSING_GLYPH",
            message: format!("{family} does not cover: {}", list.join(", ")),
        });
    }
    warnings
}

/// Escape a value for use inside a double-quoted CSS string.
///
/// Family names come from the document, and the data URI metadata is not
/// restricted to base64-safe characters, so both must not be able to close the
/// string or the `url()`.
pub(crate) fn css_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' | '"' | '\'' => {
                out.push('\\');
                out.push(character);
            }
            // A `<` would close the enclosing `<style>` element from inside a
            // CSS string; CSS escapes keep the value intact after parsing.
            '<' => out.push_str("\\3c "),
            '\n' | '\r' | '\u{000C}' => {}
            c if c.is_control() => {
                let _ = write!(out, "\\{:x} ", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn encode_base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied();
        let third = chunk.get(2).copied();
        let _ = write!(
            out,
            "{}{}{}{}",
            TABLE[(first >> 2) as usize] as char,
            TABLE[(((first & 0b0000_0011) << 4) | (second.unwrap_or(0) >> 4)) as usize] as char,
            match second {
                Some(second) =>
                    TABLE[(((second & 0b0000_1111) << 2) | (third.unwrap_or(0) >> 6)) as usize]
                        as char,
                None => '=',
            },
            match third {
                Some(third) => TABLE[(third & 0b0011_1111) as usize] as char,
                None => '=',
            }
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
    }

    #[test]
    fn encoded_bundled_faces_start_with_woff2_magic() {
        let source_han = decode_base64(&SOURCE_HAN_SANS_SC_BASE64);
        assert_eq!(&source_han[..4], b"wOF2");
        let mono = decode_base64(&NOTO_SANS_MONO_BASE64);
        assert_eq!(&mono[..4], b"wOF2");
    }

    #[test]
    fn unknown_family_falls_back_to_the_bundled_default() {
        assert_eq!(resolved_family("Unknown", false), DEFAULT_FAMILY);
        assert_eq!(resolved_family(DEFAULT_FAMILY, false), DEFAULT_FAMILY);
        assert_eq!(resolved_family("Embedded", true), "Embedded");
    }

    #[test]
    fn legacy_family_alias_resolves_to_the_renamed_face() {
        assert_eq!(resolved_family(LEGACY_FAMILY, false), DEFAULT_FAMILY);
        assert_eq!(bundled(LEGACY_FAMILY).unwrap().family, DEFAULT_FAMILY);
        assert_eq!(DEFAULT_FAMILY, "printkit Han Sans SC");
    }

    #[test]
    fn mono_face_does_not_cover_hanzi() {
        let mono = bundled(MONO_FAMILY).unwrap();
        assert!(!mono.covers('\u{4e2d}'));
        assert!(mono.covers('A'));
    }

    #[test]
    fn css_escapes_quotes_backslashes_and_angle_brackets() {
        assert_eq!(css_string("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(css_string("</style>"), "\\3c /style>");
    }

    fn decode_base64(value: &str) -> Vec<u8> {
        fn sextet(byte: u8) -> u8 {
            match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => 0,
            }
        }
        let payload = value.trim_end_matches('=').as_bytes();
        let mut decoded = Vec::with_capacity(payload.len() / 4 * 3);
        for chunk in payload.chunks(4) {
            let a = sextet(chunk[0]);
            let b = sextet(*chunk.get(1).unwrap_or(&b'A'));
            decoded.push(a << 2 | b >> 4);
            if let Some(third) = chunk.get(2) {
                let c = sextet(*third);
                decoded.push(b << 4 | c >> 2);
                if let Some(fourth) = chunk.get(3) {
                    decoded.push(c << 6 | sextet(*fourth));
                }
            }
        }
        decoded
    }
}
