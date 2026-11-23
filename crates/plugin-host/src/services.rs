//! Host services a built-in plugin may consume.

use std::sync::Arc;

use printkit_plugin_api::{RecordStore, UsageSink};

/// Service name a records plugin requires for reads.
pub const RECORD_STORE: &str = "records.store";
/// Service name a records plugin requires for usage writes.
pub const RECORDS_USAGE: &str = "records.usage";

/// What the host can give a plugin. Only the composition root constructs it.
#[derive(Clone, Default)]
pub struct ServiceRegistry {
    record_store: Option<Arc<dyn RecordStore>>,
    usage: Option<Arc<dyn UsageSink>>,
}

impl std::fmt::Debug for ServiceRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServiceRegistry")
            .field("record_store", &self.record_store.is_some())
            .field("usage", &self.usage.is_some())
            .finish()
    }
}

impl ServiceRegistry {
    /// An empty registry; a plugin requiring a service will not bind.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the read-only records store.
    pub fn with_record_store(mut self, store: Arc<dyn RecordStore>) -> Self {
        self.record_store = Some(store);
        self
    }

    /// Attach the single usage writer adapter.
    pub fn with_usage(mut self, usage: Arc<dyn UsageSink>) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Resolve the service names a plugin declared in `requires.services`.
    ///
    /// Returns the missing names on failure; the caller decides whether that
    /// makes the plugin pending or unavailable.
    pub fn resolve(&self, required: &[String]) -> Result<PluginServices<'_>, Vec<String>> {
        let missing: Vec<String> = required
            .iter()
            .filter(|service| match service.as_str() {
                RECORD_STORE => self.record_store.is_none(),
                RECORDS_USAGE => self.usage.is_none(),
                _ => true,
            })
            .cloned()
            .collect();
        if missing.is_empty() {
            Ok(PluginServices { registry: self })
        } else {
            Err(missing)
        }
    }
}

/// Borrowed services for one plugin binding.
#[derive(Clone, Copy)]
pub struct PluginServices<'a> {
    registry: &'a ServiceRegistry,
}

impl PluginServices<'_> {
    /// The records reader, when the host installed one.
    pub fn record_store(&self) -> Option<Arc<dyn RecordStore>> {
        self.registry.record_store.clone()
    }

    /// The usage writer adapter, when the host installed one.
    pub fn usage_sink(&self) -> Option<Arc<dyn UsageSink>> {
        self.registry.usage.clone()
    }
}

impl std::fmt::Debug for PluginServices<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginServices")
            .field("record_store", &self.record_store().is_some())
            .field("usage_sink", &self.usage_sink().is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use printkit_plugin_api::{RecordPage, RecordQuery, RecordStats, RecordStore, RecordSummary};

    use super::*;

    struct EmptyStore;
    impl RecordStore for EmptyStore {
        fn list(&self, _query: &RecordQuery) -> Result<RecordPage, String> {
            Ok(RecordPage {
                records: Vec::new(),
                next_cursor: None,
            })
        }
        fn stats(&self, _query: &RecordQuery, _group: &str) -> Result<Vec<RecordStats>, String> {
            Ok(Vec::new())
        }
        fn summary(&self, _query: &RecordQuery) -> Result<RecordSummary, String> {
            Ok(RecordSummary {
                today_sheets: 0,
                today_done_sheets: 0,
                today_done: 0,
                today_failed: 0,
                today_cancelled: 0,
                total: 0,
                failed: 0,
                cancelled: 0,
                pending: 0,
                avg_dur_ms: 0.0,
                slowest_printer: None,
                slowest_avg_dur_ms: 0.0,
            })
        }
        fn csv(&self, _query: &RecordQuery) -> Result<Vec<u8>, String> {
            Ok(Vec::new())
        }
    }

    struct NullUsage;
    impl UsageSink for NullUsage {
        fn record(&self, _row: printkit_plugin_api::UsageRow) {}
        fn flush(&self) {}
    }

    #[test]
    fn resolution_checks_each_service() {
        let empty = ServiceRegistry::new();
        assert_eq!(
            empty.resolve(&[RECORD_STORE.to_owned()]).unwrap_err(),
            vec![RECORD_STORE.to_owned()]
        );
        let registry = ServiceRegistry::new()
            .with_record_store(Arc::new(EmptyStore))
            .with_usage(Arc::new(NullUsage));
        let services = registry
            .resolve(&[RECORD_STORE.to_owned(), RECORDS_USAGE.to_owned()])
            .unwrap();
        assert!(services.record_store().is_some());
        assert!(services.usage_sink().is_some());
    }
}
