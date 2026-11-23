//! Trace identifiers.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Crockford base32, as used by ULIDs.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const TIME_BITS: u32 = 48;
/// Sub-millisecond time, process id and a per-process counter.
const RANDOM_BITS: u32 = 80;
const SUB_MILLIS_BITS: u32 = 20;
const PID_BITS: u32 = 22;
const COUNTER_BITS: u32 = RANDOM_BITS - SUB_MILLIS_BITS - PID_BITS;

/// A ULID-shaped identifier: 26 Crockford base32 characters, time prefix first,
/// so ids sort by creation order as plain strings.
///
/// Below the millisecond the id carries the sub-millisecond time, the process
/// id and a process-wide counter, so two runs started in the same millisecond
/// (`serve` plus a CLI call) cannot collide, and ids from one process are
/// strictly increasing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TraceId(String);

impl TraceId {
    /// Creates an id from the current time, this process and a per-process counter.
    ///
    /// The 26-character result sorts by creation order as a plain string, and ids
    /// from one process stay strictly increasing even within the same millisecond.
    /// The value is derived from time and the process id rather than randomness,
    /// so it is an identifier and not a secret.
    pub fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let millis = u128::from(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX));
        let sub_millis = u128::from(elapsed.subsec_micros()) & ((1 << SUB_MILLIS_BITS) - 1);
        let pid = u128::from(std::process::id()) & ((1 << PID_BITS) - 1);
        let counter =
            u128::from(COUNTER.fetch_add(1, Ordering::Relaxed)) & ((1 << COUNTER_BITS) - 1);
        let value = (millis << RANDOM_BITS)
            | (sub_millis << (PID_BITS + COUNTER_BITS))
            | (pid << COUNTER_BITS)
            | counter;

        let length = (TIME_BITS + RANDOM_BITS).div_ceil(5) as usize;
        let mut id = String::with_capacity(length);
        for index in (0..length).rev() {
            let digit = ((value >> (index * 5)) & 0x1f) as usize;
            id.push(char::from(ALPHABET[digit]));
        }
        Self(id)
    }

    /// Borrows the 26-character textual form, for example to use as a map key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TraceId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_ulid_shaped_and_increasing() {
        let first = TraceId::new();
        let second = TraceId::new();
        assert_eq!(first.as_str().len(), 26);
        assert!(
            first
                .as_str()
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        );
        assert!(first < second, "{first} should sort before {second}");
    }
}
