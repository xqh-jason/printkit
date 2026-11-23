#![deny(missing_docs)]
#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used))]

//! Built-in records plugin: read-only record queries and a usage flush hook.
//!
//! The crate depends only on `printkit-plugin-api`; the host passes the
//! `RecordStore`/`UsageSink` traits in through [`bind`].

use std::collections::BTreeMap;
use std::sync::Arc;

use printkit_plugin_api::{
    ApiVersion, Kind, Manifest, Permissions, PluginId, Provides, RecordPage, RecordQuery,
    RecordRow, RecordScope, RecordStats, RecordStore, RecordSummary, Requires, UiPart, UsageSink,
};

/// WebSocket methods owned by this plugin.
pub const METHODS: &[&str] = &[
    "records.list",
    "records.stats",
    "records.summary",
    "records.export",
];

/// `(method, path pattern)` routes owned by this plugin.
pub const ADMIN_ROUTES: &[(&str, &str)] = &[
    ("GET", "records/list"),
    ("GET", "records/stats"),
    ("GET", "records/summary"),
    ("GET", "records/export.csv"),
];

/// CLI subcommands owned by this plugin.
pub const CLI: &[&str] = &["stats"];

/// Store tables whose rows this plugin owns.
pub const TABLES: &[&str] = &["print_record"];

/// The plugin manifest.
pub fn manifest() -> Manifest {
    Manifest {
        id: PluginId::new("records").expect("a literal plugin id"),
        name: "Records".to_owned(),
        sub: "Usage records, statistics and CSV export".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        api: ApiVersion::new("^1").expect("a literal API range"),
        kind: Kind::Builtin,
        description: "Usage records, statistics and CSV export".to_owned(),
        requires: Requires {
            plugins: Vec::new(),
            services: vec!["records.store".to_owned(), "records.usage".to_owned()],
        },
        provides: Provides::default(),
        essential: false,
        ui: [("business", "records")]
            .into_iter()
            .map(|(window, page)| UiPart {
                window: window.to_owned(),
                page: page.to_owned(),
                titles: BTreeMap::new(),
                entry: None,
            })
            .collect(),
        permissions: Permissions::default(),
        config: None,
        name_key: Some("plugins.records.name".to_owned()),
    }
}

/// The domain object bound to host services.
#[derive(Clone)]
pub struct RecordsDomain {
    store: Arc<dyn RecordStore>,
    usage: Arc<dyn UsageSink>,
}

/// Bind the plugin to the host services.
pub fn bind(store: Arc<dyn RecordStore>, usage: Arc<dyn UsageSink>) -> RecordsDomain {
    RecordsDomain { store, usage }
}

impl RecordsDomain {
    /// List one page of records.
    pub fn list(&self, query: &RecordQuery) -> Result<RecordPage, String> {
        self.store.list(query)
    }

    /// Aggregate one group dimension.
    pub fn stats(&self, query: &RecordQuery, group: &str) -> Result<Vec<RecordStats>, String> {
        self.store.stats(query, group)
    }

    /// KPI cards for one query.
    pub fn summary(&self, query: &RecordQuery) -> Result<RecordSummary, String> {
        self.store.summary(query)
    }

    /// CSV export bytes for one query.
    pub fn csv(&self, query: &RecordQuery) -> Result<Vec<u8>, String> {
        self.store.csv(query)
    }

    /// Ask the shared usage writer to drain queued rows.
    pub fn flush_usage(&self) {
        self.usage.flush();
    }
}

/// Scope helper: admin adapters construct [`RecordScope::All`].
pub fn admin_scope() -> RecordScope {
    RecordScope::All
}

/// Scope helper: a token bound to `origin` constructs [`RecordScope::Origin`].
pub fn origin_scope(origin: impl Into<String>) -> RecordScope {
    RecordScope::Origin(origin.into())
}

/// Convenience mapping used by adapters that already hold a row.
pub fn row_page(rows: Vec<RecordRow>, next_cursor: Option<i64>) -> RecordPage {
    RecordPage {
        records: rows,
        next_cursor,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use printkit_plugin_api::{RecordScope, UsageRow};

    use super::*;

    struct FakeStore;
    impl RecordStore for FakeStore {
        fn list(&self, query: &RecordQuery) -> Result<RecordPage, String> {
            let scope = match &query.scope {
                RecordScope::All => "all",
                RecordScope::Origin(origin) => origin.as_str(),
            };
            Ok(RecordPage {
                records: vec![RecordRow {
                    id: 1,
                    seq: 1,
                    at: "2026-01-01T00:00:00Z".to_owned(),
                    origin: scope.to_owned(),
                    template: None,
                    tags: Vec::new(),
                    printer: None,
                    pages: 1,
                    copies: 1,
                    status: "ok".to_owned(),
                    dur_ms: 1,
                    error_code: None,
                    trace_id: "trace".to_owned(),
                }],
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

    struct FakeUsage;
    impl UsageSink for FakeUsage {
        fn record(&self, _row: UsageRow) {}
        fn flush(&self) {}
    }

    #[test]
    fn bind_keeps_the_scope_unchanged() {
        let domain = bind(Arc::new(FakeStore), Arc::new(FakeUsage));
        let query = RecordQuery {
            scope: RecordScope::Origin("https://a.example".to_owned()),
            from: None,
            to: None,
            tags: Vec::new(),
            printer: None,
            status: None,
            cursor: None,
            limit: 10,
        };
        let page = domain.list(&query).unwrap();
        assert_eq!(page.records[0].origin, "https://a.example");
        domain.flush_usage();
    }

    #[test]
    fn manifest_requires_both_services() {
        let manifest = manifest();
        assert_eq!(
            manifest.requires.services,
            vec!["records.store".to_owned(), "records.usage".to_owned()]
        );
        assert_eq!(manifest.ui[0].page, "records");
    }
}
