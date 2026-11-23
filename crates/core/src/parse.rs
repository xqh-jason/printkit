//! Parsing with JSON Pointer positions.
//!
//! `serde_json` reports line and column, which is useless to a client that
//! generated its request from an object. `serde_path_to_error` records the
//! field path, and this module turns that path into a JSON Pointer so the
//! structural layer reports the same `detail.pointer` shape as the semantic
//! layer.

use serde_path_to_error::{Path, Segment};

use crate::document::Document;
use crate::validate::{E_SCHEMA, Violation};
use crate::validated::Validated;

/// A structural parse failure: wrong type, missing field, unknown field.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ParseError {
    /// JSON Pointer to the field that failed.
    pub pointer: String,
    /// The serde message, kept verbatim for diagnosis.
    pub message: String,
}

impl ParseError {
    fn from_path_error(error: serde_path_to_error::Error<serde_json::Error>, input: &[u8]) -> Self {
        let mut pointer = json_pointer(error.path());
        let message = error.into_inner().to_string();
        if let Some(field) = unknown_field(&message)
            && let Some(refined) = locate_key(input, &pointer, field)
        {
            pointer = refined;
        }
        Self { pointer, message }
    }
}

/// Parse a document without validating it.
///
/// ```
/// use printkit_core::parse;
///
/// let json = br#"{"version":1,"pages":[{"spec":{"size":"A4"},"items":[]}]}"#;
/// let document = parse(json).expect("document parses");
/// assert_eq!(document.pages.len(), 1);
///
/// let validated = document.validated().expect("document passes validation");
/// assert_eq!(validated.document().pages.len(), 1);
/// ```
pub fn parse(input: &[u8]) -> Result<Document, ParseError> {
    let mut deserializer = serde_json::Deserializer::from_slice(input);
    let document = serde_path_to_error::deserialize(&mut deserializer)
        .map_err(|error| ParseError::from_path_error(error, input))?;
    deserializer.end().map_err(|error| ParseError {
        pointer: String::new(),
        message: error.to_string(),
    })?;
    Ok(document)
}

/// Parse and validate a document in one step.
pub fn validate_bytes(input: &[u8]) -> Result<Validated, DocumentError> {
    let document = parse(input)?;
    document.validated().map_err(DocumentError::Semantic)
}

/// A document that failed either the structural or the semantic layer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DocumentError {
    /// The input is not structurally valid for the document model.
    #[error("{0}")]
    Structure(#[from] ParseError),
    /// The document parsed, but semantic validation reported violations.
    #[error("document validation failed: {}", first_message(.0))]
    Semantic(Vec<Violation>),
}

impl DocumentError {
    /// Semantic violations report their own code: a schema problem is
    /// `E_SCHEMA`, an encrypted embedded PDF is `E_PDF_ENCRYPTED`. The first
    /// violation is the one worth showing a user.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Structure(_) => E_SCHEMA,
            Self::Semantic(violations) => violations
                .first()
                .map_or(E_SCHEMA, |violation| violation.code),
        }
    }

    /// The first pointer, which is the field worth showing a user.
    pub fn pointer(&self) -> Option<&str> {
        match self {
            Self::Structure(error) => Some(&error.pointer),
            Self::Semantic(violations) => violations
                .first()
                .map(|violation| violation.pointer.as_str()),
        }
    }

    /// Every semantic violation; empty for a structural failure.
    pub fn violations(&self) -> &[Violation] {
        match self {
            Self::Structure(_) => &[],
            Self::Semantic(violations) => violations,
        }
    }
}

fn first_message(violations: &[Violation]) -> &str {
    violations
        .first()
        .map(|violation| violation.message.as_str())
        .unwrap_or("invalid document")
}

/// Render a serde path as a JSON Pointer. The root path is the empty pointer.
fn json_pointer(path: &Path) -> String {
    let mut pointer = String::new();
    for segment in path.iter() {
        match segment {
            Segment::Seq { index } => {
                pointer.push('/');
                pointer.push_str(&index.to_string());
            }
            Segment::Map { key } | Segment::Enum { variant: key } => {
                pointer.push('/');
                escape_into(&mut pointer, key);
            }
            Segment::Unknown => pointer.push_str("/?"),
        }
    }
    pointer
}

/// The key named by serde's `unknown field `name`` message.
fn unknown_field(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("unknown field `")?;
    let end = rest.find('`')?;
    Some(&rest[..end])
}

/// `serde` buffers the body of an internally tagged enum, so the tracked path
/// stops at the item. Walk the raw JSON below that node and name the first
/// object that carries the offending key. A key that is unknown to one struct
/// but spelled like a field of a sibling struct resolves to the first match in
/// document order; the message keeps the field name either way.
fn locate_key(input: &[u8], pointer: &str, key: &str) -> Option<String> {
    let root: serde_json::Value = serde_json::from_slice(input).ok()?;
    let start = node_at(&root, pointer)?;
    let mut queue = std::collections::VecDeque::from([(start, pointer.to_owned())]);
    while let Some((node, path)) = queue.pop_front() {
        match node {
            serde_json::Value::Object(map) => {
                if map.contains_key(key) {
                    let mut found = path;
                    found.push('/');
                    escape_into(&mut found, key);
                    return Some(found);
                }
                for (name, child) in map {
                    let mut child_path = path.clone();
                    child_path.push('/');
                    escape_into(&mut child_path, name);
                    queue.push_back((child, child_path));
                }
            }
            serde_json::Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    let mut child_path = path.clone();
                    child_path.push('/');
                    child_path.push_str(&index.to_string());
                    queue.push_back((child, child_path));
                }
            }
            _ => {}
        }
    }
    None
}

fn node_at<'a>(root: &'a serde_json::Value, pointer: &str) -> Option<&'a serde_json::Value> {
    let mut node = root;
    for raw in pointer.split('/').skip(1) {
        let token = raw.replace("~1", "/").replace("~0", "~");
        node = match node {
            serde_json::Value::Object(map) => map.get(&token)?,
            serde_json::Value::Array(items) => items.get(token.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(node)
}

fn escape_into(pointer: &mut String, key: &str) {
    for character in key.chars() {
        match character {
            '~' => pointer.push_str("~0"),
            '/' => pointer.push_str("~1"),
            other => pointer.push(other),
        }
    }
}
