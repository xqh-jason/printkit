//! Schema fragments for constraints the derive attributes cannot express.
//!
//! `schemars` only offers inclusive `min` / `max`, while frames, font sizes and
//! custom paper require a strictly positive value. The helpers below produce
//! the exact JSON Schema keywords so the published schema and
//! [`Document::validate`](crate::Document::validate) agree.

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};

/// A strictly positive number of millimetres.
pub fn positive_length(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = f64::json_schema(generator);
    schema.insert("exclusiveMinimum".into(), 0.0.into());
    schema
}

/// An optional strictly positive number of millimetres.
pub fn positive_length_option(generator: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "anyOf": [
            {"type": "null"},
            positive_length(generator),
        ]
    })
}

/// A strictly positive plain number.
pub fn positive_number(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = f64::json_schema(generator);
    schema.insert("exclusiveMinimum".into(), 0.0.into());
    schema
}

/// Optional tag list: at most [`MAX_TAGS`](crate::limits::MAX_TAGS) entries of
/// at most [`MAX_TAG_CHARS`](crate::limits::MAX_TAG_CHARS) characters. `length`
/// does not reach an array nested in an `Option`, so the bounds are written
/// here.
pub fn tags(_generator: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "anyOf": [
            {"type": "null"},
            {
                "type": "array",
                "maxItems": crate::limits::MAX_TAGS,
                "items": {
                    "type": "string",
                    "maxLength": crate::limits::MAX_TAG_CHARS,
                },
            },
        ]
    })
}

/// A field whose only accepted value is `null` (absent or explicit).
pub fn null_only(_generator: &mut SchemaGenerator) -> Schema {
    json_schema!({"type": "null"})
}

/// `PageSpec` conditions JSON Schema cannot attach to individual fields:
/// custom paper requires positive `width`/`height`, named paper forbids both.
pub fn page_spec_conditions(schema: &mut Schema) {
    schema.insert(
        "allOf".to_owned(),
        serde_json::json!([
            {
                "if": {"required": ["size"], "properties": {"size": {"const": "custom"}}},
                "then": {
                    "required": ["width", "height"],
                    "properties": {
                        "width": {
                            "type": "number",
                            "exclusiveMinimum": 0.0,
                            "maximum": crate::limits::MAX_PAPER_MM,
                            "format": "double"
                        },
                        "height": {
                            "type": "number",
                            "exclusiveMinimum": 0.0,
                            "maximum": crate::limits::MAX_PAPER_MM,
                            "format": "double"
                        }
                    }
                }
            },
            {
                "if": {"properties": {"size": {"not": {"const": "custom"}}}},
                "then": {"properties": {"width": {"type": "null"}, "height": {"type": "null"}}}
            },
            {
                "properties": {"background": {
                    "type": "string",
                    "pattern": "^data:image/[^,]*;base64,",
                    "maxLength": base64_uri_limit(crate::limits::MAX_RESOURCE_BYTES),
                }}
            }
        ]),
    );
}

/// Exactly one of `html` and `url` may be present.
pub fn html_data_conditions(schema: &mut Schema) {
    schema.insert(
        "allOf".to_owned(),
        serde_json::json!([
            {
                "oneOf": [
                    {"required": ["html"], "not": {"required": ["url"]}},
                    {"required": ["url"], "not": {"required": ["html"]}}
                ]
            }
        ]),
    );
}

/// Barcode value is never empty; two-dimensional codes also cap its bytes.
pub fn barcode_data_conditions(schema: &mut Schema) {
    schema.insert(
        "allOf".to_owned(),
        serde_json::json!([
            {"properties": {"value": {"minLength": 1}}},
            {
                "if": {"properties": {"kind": {"enum": ["qr", "pdf417", "datamatrix"]}}},
                "then": {"properties": {"value": {"maxLength": crate::limits::MAX_BARCODE_BYTES}}}
            }
        ]),
    );
}

/// Upper bound in characters for a base64 data URI carrying `bytes`.
fn base64_uri_limit(bytes: usize) -> usize {
    bytes.div_ceil(3).saturating_mul(4).saturating_add(256)
}

/// Image payloads: an image data URI within the resource cap.
pub fn image_data_conditions(schema: &mut Schema) {
    schema.insert(
        "allOf".to_owned(),
        serde_json::json!([{
            "properties": {"src": {
                "type": "string",
                "pattern": "^data:image/[^,]*;base64,",
                "maxLength": base64_uri_limit(crate::limits::MAX_RESOURCE_BYTES),
            }}
        }]),
    );
}

/// Embedded PDF payloads: an application/pdf data URI within the resource cap.
pub fn pdf_data_conditions(schema: &mut Schema) {
    schema.insert(
        "allOf".to_owned(),
        serde_json::json!([{
            "properties": {"src": {
                "type": "string",
                "pattern": "^data:application/pdf[^,]*;base64,",
                "maxLength": base64_uri_limit(crate::limits::MAX_RESOURCE_BYTES),
            }}
        }]),
    );
}

/// Embedded font payloads: a font data URI within the font cap.
pub fn text_data_conditions(schema: &mut Schema) {
    schema.insert(
        "allOf".to_owned(),
        serde_json::json!([{
            "properties": {"fontFile": {
                "anyOf": [
                    {"type": "null"},
                    {
                        "type": "string",
                        "pattern": "^data:(font/|application/font|application/octet-stream)[^,]*;base64,",
                        "maxLength": base64_uri_limit(crate::limits::MAX_FONT_BYTES),
                    }
                ]
            }}
        }]),
    );
}
