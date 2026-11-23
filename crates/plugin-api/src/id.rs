//! Plugin identifiers.

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

/// A plugin id: lowercase ASCII letters, digits and `-`, at most 32 characters,
/// starting with a letter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct PluginId(String);

/// Why a string is not a valid [`PluginId`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdError {
    /// The string is empty.
    #[error("a plugin id must not be empty")]
    Empty,
    /// The first character is not a lowercase ASCII letter.
    #[error("a plugin id must start with a lowercase ASCII letter: {0}")]
    FirstCharacter(String),
    /// The id is longer than 32 bytes.
    #[error("a plugin id must be at most 32 characters: {0}")]
    TooLong(String),
    /// The id contains a character outside `a-z`, `0-9` and `-`.
    #[error("a plugin id contains an unsupported character: {0}")]
    Character(String),
}

impl PluginId {
    /// Validate a raw id.
    pub fn new(raw: &str) -> Result<Self, IdError> {
        if raw.is_empty() {
            return Err(IdError::Empty);
        }
        if raw.len() > 32 {
            return Err(IdError::TooLong(raw.to_owned()));
        }
        let mut chars = raw.chars();
        match chars.next() {
            Some(first) if first.is_ascii_lowercase() => {}
            Some(_) => return Err(IdError::FirstCharacter(raw.to_owned())),
            None => return Err(IdError::Empty),
        }
        if chars.any(|character| !matches!(character, 'a'..='z' | '0'..='9' | '-')) {
            return Err(IdError::Character(raw.to_owned()));
        }
        Ok(Self(raw.to_owned()))
    }

    /// The id as written on the wire and in files.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PluginId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PluginId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::new(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_documented_shape() {
        for id in ["observability", "records", "a1", "analytics"] {
            assert_eq!(PluginId::new(id).unwrap().as_str(), id);
        }
    }

    #[test]
    fn rejects_foreign_ids() {
        for id in ["", "-x", "A", "has space", "x/y", "é", &"a".repeat(33)] {
            assert!(PluginId::new(id).is_err(), "{id:?} was accepted");
        }
    }

    #[test]
    fn deserialization_validates() {
        let id: PluginId = serde_json::from_str("\"observability\"").unwrap();
        assert_eq!(id.as_str(), "observability");
        assert!(serde_json::from_str::<PluginId>("\"../x\"").is_err());
    }
}
