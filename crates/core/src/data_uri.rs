//! Base64 data URI payloads.
//!
//! The document layer checks the shape of a data URI (prefix, `;base64`
//! marker, padding) before anything decodes it; this module only turns the
//! payload into bytes, so preflight and the engine share one decoder.

use base64::Engine as _;

/// Decode the payload of a base64 data URI.
///
/// Returns `None` when the value is not a base64 data URI or the payload does
/// not decode.
pub fn decode_data_uri(value: &str) -> Option<Vec<u8>> {
    let rest = value.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    if !meta.ends_with(";base64") {
        return None;
    }
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_payload_of_a_base64_data_uri() {
        assert_eq!(
            decode_data_uri("data:application/pdf;base64,JVBERi0="),
            Some(b"%PDF-".to_vec())
        );
    }

    #[test]
    fn rejects_a_uri_that_is_not_base64() {
        assert_eq!(decode_data_uri("data:text/plain,hello"), None);
        assert_eq!(decode_data_uri("JVBERi0="), None);
    }
}
