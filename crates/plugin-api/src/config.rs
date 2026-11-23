//! The small configuration schema a plugin may expose.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// A property type accepted by [`ConfigSchema`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigValueKind {
    /// Any string; `enum` may narrow it.
    String,
    /// A JSON number.
    Number,
    /// A JSON integer.
    Integer,
    /// A JSON boolean.
    Boolean,
}

/// One field of a [`ConfigSchema`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigProperty {
    /// Value type of the field.
    #[serde(rename = "type")]
    pub kind: ConfigValueKind,
    /// The only accepted values, when present.
    #[serde(default, rename = "enum", skip_serializing_if = "Option::is_none")]
    pub allowed: Option<Vec<Value>>,
    /// Value used when the user has not chosen one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// Inclusive minimum for numbers and integers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<f64>,
    /// Inclusive maximum for numbers and integers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<f64>,
    /// Human-readable label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Human-readable explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A plugin's configuration surface.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSchema {
    /// Fields shown by the plugin page, in key order.
    #[serde(default)]
    pub properties: BTreeMap<String, ConfigProperty>,
}

/// Why a configuration value does not satisfy a [`ConfigSchema`].
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ConfigError {
    /// A supplied key is not described by the schema.
    #[error("unknown configuration key: {0}")]
    UnknownKey(String),
    /// A value has the wrong JSON type.
    #[error("configuration {key} expects {expected}")]
    Type {
        /// Field name.
        key: String,
        /// Expected kind.
        expected: &'static str,
    },
    /// A value is outside the allowed set.
    #[error("configuration {key} is not one of the allowed values")]
    Enum {
        /// Field name.
        key: String,
    },
    /// A number is outside the declared range.
    #[error("configuration {key} must be between {minimum} and {maximum}")]
    Range {
        /// Field name.
        key: String,
        /// Inclusive lower bound.
        minimum: f64,
        /// Inclusive upper bound.
        maximum: f64,
    },
}

/// Validate one property value against its declaration.
fn check_property(key: &str, property: &ConfigProperty, value: &Value) -> Result<(), ConfigError> {
    match property.kind {
        ConfigValueKind::String => {
            if !value.is_string() {
                return Err(ConfigError::Type {
                    key: key.to_owned(),
                    expected: "a string",
                });
            }
        }
        ConfigValueKind::Number => {
            if !value.is_number() {
                return Err(ConfigError::Type {
                    key: key.to_owned(),
                    expected: "a number",
                });
            }
        }
        ConfigValueKind::Integer => {
            if value.as_i64().is_none() && value.as_u64().is_none() {
                return Err(ConfigError::Type {
                    key: key.to_owned(),
                    expected: "an integer",
                });
            }
        }
        ConfigValueKind::Boolean => {
            if !value.is_boolean() {
                return Err(ConfigError::Type {
                    key: key.to_owned(),
                    expected: "a boolean",
                });
            }
        }
    }
    if let Some(allowed) = &property.allowed
        && !allowed.contains(value)
    {
        return Err(ConfigError::Enum {
            key: key.to_owned(),
        });
    }
    if let Some(number) = value.as_f64()
        && (property.minimum.is_some_and(|minimum| number < minimum)
            || property.maximum.is_some_and(|maximum| number > maximum))
    {
        return Err(ConfigError::Range {
            key: key.to_owned(),
            minimum: property.minimum.unwrap_or(f64::NEG_INFINITY),
            maximum: property.maximum.unwrap_or(f64::INFINITY),
        });
    }
    Ok(())
}

/// Validate a whole configuration object.
///
/// Unknown keys and wrong types are rejected; a schema with no properties
/// accepts only an empty object.
pub fn validate_config(schema: &ConfigSchema, value: &Value) -> Result<(), ConfigError> {
    let Some(object) = value.as_object() else {
        return Err(ConfigError::Type {
            key: "<config>".to_owned(),
            expected: "an object",
        });
    };
    for (key, supplied) in object {
        let Some(property) = schema.properties.get(key) else {
            return Err(ConfigError::UnknownKey(key.clone()));
        };
        check_property(key, property, supplied)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn schema() -> ConfigSchema {
        let mut properties = BTreeMap::new();
        properties.insert(
            "currency".to_owned(),
            ConfigProperty {
                kind: ConfigValueKind::String,
                allowed: Some(vec![json!("CNY"), json!("USD")]),
                default: Some(json!("CNY")),
                minimum: None,
                maximum: None,
                title: None,
                description: None,
            },
        );
        properties.insert(
            "copies".to_owned(),
            ConfigProperty {
                kind: ConfigValueKind::Integer,
                allowed: None,
                default: Some(json!(1)),
                minimum: Some(1.0),
                maximum: Some(99.0),
                title: None,
                description: None,
            },
        );
        ConfigSchema { properties }
    }

    #[test]
    fn accepts_a_matching_object() {
        validate_config(&schema(), &json!({"currency": "USD", "copies": 3})).unwrap();
    }

    #[test]
    fn rejects_unknown_keys_types_enums_and_ranges() {
        for (value, expected) in [
            (json!({"extra": true}), "unknown"),
            (json!({"copies": "3"}), "integer"),
            (json!({"currency": "EUR"}), "allowed"),
            (json!({"copies": 100}), "between"),
        ] {
            let error = validate_config(&schema(), &value).unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn an_empty_schema_accepts_only_an_empty_object() {
        let empty = ConfigSchema::default();
        validate_config(&empty, &json!({})).unwrap();
        assert!(validate_config(&empty, &json!({"x": 1})).is_err());
    }
}
