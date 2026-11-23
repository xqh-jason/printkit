//! Plugin API ranges.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// The plugin API range a manifest declares, for example `^1` or `1`.
///
/// Only a single major version is accepted; printkit never solves a semver
/// expression. A manifest written for another major is `incompatible`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiVersion {
    raw: String,
    major: u32,
}

/// Why an API range string is not accepted.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ApiError {
    /// The range is empty.
    #[error("the plugin API range is empty")]
    Empty,
    /// The range names something other than one integer major version.
    #[error("expected \"^1\" or \"1\": {0}")]
    Shape(String),
    /// The major version is not a number.
    #[error("the plugin API major version is not a number: {0}")]
    Major(String),
}

impl ApiVersion {
    /// Parse `^1`, `1`, or the same shape with another major version.
    pub fn new(raw: &str) -> Result<Self, ApiError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ApiError::Empty);
        }
        let digits = trimmed.strip_prefix('^').unwrap_or(trimmed);
        if digits.is_empty() || !digits.chars().all(|character| character.is_ascii_digit()) {
            return Err(ApiError::Shape(trimmed.to_owned()));
        }
        let major = digits
            .parse::<u32>()
            .map_err(|_| ApiError::Major(digits.to_owned()))?;
        Ok(Self {
            raw: trimmed.to_owned(),
            major,
        })
    }

    /// The major version number.
    pub fn major(&self) -> u32 {
        self.major
    }

    /// Whether this range accepts a host at `host_major`.
    pub fn accepts(&self, host_major: u32) -> bool {
        self.major == host_major
    }
}

impl FromStr for ApiVersion {
    type Err = ApiError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl fmt::Display for ApiVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.raw)
    }
}

impl Serialize for ApiVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for ApiVersion {
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
    fn accepts_the_two_documented_spellings() {
        for (raw, major) in [("^1", 1), ("1", 1), ("^2", 2)] {
            let api = ApiVersion::new(raw).unwrap();
            assert_eq!(api.major(), major);
            assert_eq!(api.to_string(), raw);
        }
    }

    #[test]
    fn rejects_ranges_printkit_does_not_solve() {
        for raw in ["", "^1.2", ">=1", "*", "^", "one"] {
            assert!(ApiVersion::new(raw).is_err(), "{raw:?} was accepted");
        }
    }

    #[test]
    fn compatibility_is_by_major_only() {
        assert!(ApiVersion::new("^1").unwrap().accepts(1));
        assert!(!ApiVersion::new("^2").unwrap().accepts(1));
    }
}
