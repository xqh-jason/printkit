//! Handshake and message envelopes.
//!
//! The wire adds a `"type"` discriminator (`hello`, `welcome`, `rejected`,
//! `event`) and the `"event"` name for pushed events; the payload types below
//! describe everything after that discriminator. Response-only types accept
//! unknown fields: a newer engine may add fields without breaking an old client.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::code::{ErrorCode, WarningCode};

/// Protocol major version understood by this build.
pub const PROTOCOL_VERSION: u32 = 1;

/// A token scope. A `print` token never upgrades to `admin`.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The printing scope: `doc.*`, `job.*`, `printers.*`, `preview.*`
    /// and `records.*`.
    Print,
    /// On top of `print`: `raw.send`.
    Raw,
    /// On top of `print`: `trace.*` and `logs.export`.
    Admin,
}

/// A method family announced in `welcome.capabilities`.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityFamily {
    /// The removed `template.*` method family.
    ///
    /// The server no longer sends this value; it remains only so an old
    /// `Welcome` frame can still be parsed by a current client.
    #[deprecated(note = "template.* methods were removed in S2; the server sends no capability")]
    Template,
}

/// A failure body. `detail` is free form; for `E_SCHEMA` it carries `pointer`.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorBody {
    /// One [`ErrorCode`] spelling, for example `E_SCHEMA`.
    pub code: ErrorCode,
    /// Human-readable failure text.
    pub message: String,
    /// Optional free-form context; `E_SCHEMA` carries `pointer`, a full queue
    /// carries `queueMax` and `queuePos`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(type = "unknown", optional))]
    pub detail: Option<Value>,
}

/// A warning inside a successful response.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WarningBody {
    /// One [`WarningCode`] spelling.
    pub code: WarningCode,
    /// Human-readable warning text.
    pub message: String,
    /// Optional free-form context; absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(type = "unknown", optional))]
    pub detail: Option<Value>,
}

/// Client handshake.
#[cfg_attr(
    feature = "typescript",
    derive(ts_rs::TS),
    ts(export_to = "types.ts", optional_fields)
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
#[schemars(transform = crate::envelope::hello_frame_schema)]
pub struct Hello {
    /// Wire discriminator. The server strips it before deserialising, so the
    /// Rust payload defaults it; clients and the published schema require it.
    #[serde(rename = "type", default = "hello_frame_type")]
    #[cfg_attr(feature = "typescript", ts(rename = "type", type = "\"hello\""))]
    pub r#type: String,
    /// Client's protocol major version; must equal [`PROTOCOL_VERSION`].
    pub protocol: u32,
    /// Client SDK name and version, free form.
    pub sdk: String,
    /// Omitted by clients that print without a token: a local script, or a
    /// whitelisted browser. Such a session only receives the `print` scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Self-reported origin; must match the WebSocket `Origin` header when the
    /// client is a browser. Scripts omit it and authenticate by token alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

fn hello_frame_type() -> String {
    "hello".to_owned()
}

/// Server handshake reply.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Welcome {
    /// Server's [`PROTOCOL_VERSION`].
    pub protocol: u32,
    /// Engine version string, currently the crate version.
    pub engine: String,
    /// Scopes actually granted to this session.
    pub granted: Vec<Scope>,
    /// Optional method families this build implements.
    pub capabilities: Vec<CapabilityFamily>,
    /// Active plugin ids whose method families this build implements.
    ///
    /// Additive and optional on the wire: an older server omits the field and
    /// an older client ignores it. Clients that predate plugin ids fall back to
    /// `capabilities`.
    #[serde(default, rename = "pluginFamilies")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub plugin_families: Option<Vec<String>>,
    /// Non-fatal handshake warnings; empty when there are none.
    pub warnings: Vec<WarningCode>,
}

/// Server refusal during handshake.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Rejected {
    /// Why the handshake failed; `detail.traceId` links to `printkit logs
    /// --trace`.
    pub error: ErrorBody,
}

/// One request. `id` pairs it with the response.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct Request {
    /// Client-chosen correlation id, echoed on the response.
    pub id: String,
    /// Wire method name from [`METHODS`](crate::METHODS).
    pub method: String,
    /// Method parameters; `{}` when the method takes none.
    #[serde(default)]
    #[cfg_attr(feature = "typescript", ts(type = "unknown"))]
    pub params: Value,
}

/// One response: exactly one of `result` and `error` is present.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Response {
    /// Correlation id copied from the request.
    pub id: String,
    /// `true` when `error` is absent; `result` is then present.
    pub ok: bool,
    /// Method result; absent exactly when `ok` is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(type = "unknown", optional))]
    pub result: Option<Value>,
    /// Non-fatal warnings; empty when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<WarningBody>,
    /// Failure body; exactly one of `result` and `error` is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub error: Option<ErrorBody>,
}

/// Lifecycle of one job.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Accepted, waiting for a render slot; `queuePos` tells the position.
    Queued,
    /// Rendering or handing bytes to the spooler.
    Printing,
    /// Terminal: the job was submitted successfully.
    Done,
    /// Terminal: no job was submitted; `error` says why.
    Failed,
    /// Terminal: the job was cancelled before it reached the spooler.
    Cancelled,
}

/// Progress event pushed while a job runs. `status` uses the same words as
/// [`crate::JobStatus`].
#[cfg_attr(feature = "typescript", derive(ts_rs::TS), ts(export_to = "types.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct JobEvent {
    /// Job handle the event belongs to (`jobId`).
    pub job_id: String,
    /// Current phase; the wire words match [`JobState`].
    pub status: JobState,
    /// Position in the render queue while `status` is `queued`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub queue_pos: Option<u32>,
    /// Fraction of the render completed, 0.0 to 1.0.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub progress: Option<f64>,
    /// Warnings raised by a successful render; empty until `status` is `done`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<WarningBody>,
    /// Failure body once `status` is `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub error: Option<ErrorBody>,
}

/// The published schema describes the wire frame: the server strips `type`
/// before deserialising, but a client must send it. The Rust payload type
/// therefore stays without `type`, while the published frame requires it.
pub fn hello_frame_schema(schema: &mut schemars::Schema) {
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.insert("type".to_owned(), serde_json::json!({"const": "hello"}));
    }
    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        if !required.iter().any(|value| value == "type") {
            required.insert(0, Value::String("type".to_owned()));
        }
    } else {
        schema.insert("required".to_owned(), serde_json::json!(["type"]));
    }
}

#[cfg(test)]
mod tests {
    #![allow(deprecated)]
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    fn welcome() -> Welcome {
        Welcome {
            protocol: PROTOCOL_VERSION,
            engine: "0.1.0".to_owned(),
            granted: vec![Scope::Print],
            capabilities: Vec::new(),
            plugin_families: Some(vec!["observability".to_owned(), "records".to_owned()]),
            warnings: Vec::new(),
        }
    }

    #[derive(Debug, Deserialize)]
    struct OldWelcome {
        protocol: u32,
        engine: String,
        granted: Vec<Scope>,
        capabilities: Vec<CapabilityFamily>,
        warnings: Vec<WarningCode>,
    }

    #[test]
    fn a_new_welcome_reaches_an_old_client() {
        let value = serde_json::to_value(welcome()).unwrap();
        assert_eq!(value["pluginFamilies"], json!(["observability", "records"]));
        assert_eq!(value["capabilities"], json!([]));
        let parsed: OldWelcome = serde_json::from_value(value).unwrap();
        assert!(parsed.capabilities.is_empty());
        assert_eq!(parsed.protocol, PROTOCOL_VERSION);
        assert_eq!(parsed.engine, "0.1.0");
        assert_eq!(parsed.granted, vec![Scope::Print]);
        assert!(parsed.warnings.is_empty());
    }

    #[test]
    fn an_old_welcome_parses_with_no_plugin_families() {
        let old = json!({
            "protocol": 1,
            "engine": "0.1.0",
            "granted": ["print"],
            "capabilities": ["template"],
            "warnings": [],
        });
        let parsed: Welcome = serde_json::from_value(old).unwrap();
        assert_eq!(parsed.plugin_families, None);
        assert_eq!(parsed.capabilities, vec![CapabilityFamily::Template]);
    }

    #[test]
    fn capabilities_keep_the_documented_spelling() {
        assert_eq!(
            serde_json::to_value(CapabilityFamily::Template).unwrap(),
            json!("template")
        );
    }
}
