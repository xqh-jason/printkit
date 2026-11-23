//! Fan-out of `job` events to the WebSocket connections of one origin.
//!
//! A job can start from a WebSocket request or from `POST /print/batch`, but
//! its progress is always delivered as `job` events. Keying the broadcast by
//! origin keeps a print token from seeing another page's jobs, matching the
//! origin isolation of `records.*`.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::broadcast;

/// How many undelivered events one connection may fall behind before the
/// channel drops the oldest. A slow client loses progress updates, never the
/// job itself.
const CAPACITY: usize = 256;

pub struct JobEventHub {
    channels: Mutex<HashMap<String, broadcast::Sender<String>>>,
}

impl JobEventHub {
    pub fn new() -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
        }
    }

    /// Subscribe one connection to its origin's event stream.
    pub fn subscribe(&self, origin: &str) -> broadcast::Receiver<String> {
        let mut channels = self
            .channels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        channels
            .entry(origin.to_owned())
            .or_insert_with(|| broadcast::channel(CAPACITY).0)
            .subscribe()
    }

    /// Publish one job event. No subscriber is not an error: the job still
    /// runs and `job.status` still answers.
    pub fn publish(&self, origin: &str, event: String) {
        let channels = self
            .channels
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(channel) = channels.get(origin) {
            let _ = channel.send(event);
        }
    }
}

impl Default for JobEventHub {
    fn default() -> Self {
        Self::new()
    }
}
