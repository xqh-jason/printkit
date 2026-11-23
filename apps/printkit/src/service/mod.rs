//! The `serve` face: HTTP administration and the WebSocket print protocol.
//!
//! The listener always binds loopback. The data directory is owned by one
//! process at a time (`serve.lock`), and the administrator token gates `/admin`.

pub mod admin;
pub mod auth;
pub mod events;
pub mod exports;
pub mod lock;
pub mod observability;
pub mod plugins;
pub mod preview;
pub mod print_batch;
pub mod records;
pub mod ws;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use printkit_facade::{FetchPolicy, PrintJobs, PrinterBackend, RenderHost, RenderPool, Sink};
use printkit_store::Store;
use salvo::affix_state;
use salvo::http::HeaderValue;
use salvo::prelude::*;
use salvo::server::ServerHandle;
use serde_json::json;

use self::admin::Sessions;

/// Everything a handler needs. Built once before the listener starts.
pub struct AppState {
    pub data_dir: PathBuf,
    pub store: Arc<Store>,
    /// Resolved plugin registry; gates methods, routes and the sink.
    pub plugins: Arc<plugins::PluginsState>,
    pub admin_token: String,
    /// HMAC key for `/exports` capability URLs. A link is valid only for the
    /// exact canonical file and a short expiry; `None` means randomness failed,
    /// so links are refused instead of signing with a predictable key.
    pub export_secret: Option<[u8; 32]>,
    /// Set before the listener drains; new print/raw jobs are refused.
    pub draining: AtomicBool,
    /// Origins removed from the allowlist while a socket is still connected.
    pub revoked_origins: Mutex<std::collections::HashSet<String>>,
    /// The exact origins that may drive `/admin`; the browser sends one of
    /// them as the `Origin` header on every state-changing request.
    pub admin_origins: Vec<String>,
    pub sessions: Sessions,
    pub sink: Arc<dyn Sink>,
    pub pool: Arc<RenderPool>,
    pub print_backend: Arc<dyn PrinterBackend>,
    pub render_host: Arc<dyn RenderHost>,
    pub print_jobs: PrintJobs,
    pub job_events: Arc<events::JobEventHub>,
    pub previews: preview::PreviewManager,
    pub fetch: FetchPolicy,
    /// Filled after the acceptor exists, so `/admin/shutdown` can stop serving.
    pub server: Mutex<Option<ServerHandle>>,
    pub connections: Arc<AtomicUsize>,
    /// Live connections grouped by transport Origin, so one page cannot take
    /// every global slot.
    pub connection_origins: Arc<Mutex<HashMap<String, usize>>>,
    /// At most four batch requests validate/submit at once; each may hold
    /// hundreds of megabytes while its parts are prepared.
    pub batch_slots: Arc<tokio::sync::Semaphore>,
    /// Consecutive failed logins and the start of the current window.
    pub login_failures: Mutex<(u32, std::time::Instant)>,
    /// Pages known before the renderer finishes, keyed by job id. Bounded by
    /// clearing when it grows past the controlled-job limit.
    pub job_pages: Mutex<HashMap<String, u32>>,
    /// Recent HTTP/WS idempotency keys, bounded by entries and bytes and
    /// pruned on each lookup after ten minutes. See [`IdempotencyCache`].
    pub idempotency: Mutex<IdempotencyCache>,
    /// One async lock per in-flight idempotency key, so concurrent duplicates
    /// wait for the first reservation instead of printing twice.
    pub idempotency_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    #[cfg_attr(not(feature = "fake-render"), allow(dead_code))]
    pub records_retention_days: u16,
    #[cfg_attr(not(feature = "fake-render"), allow(dead_code))]
    pub records_max_rows: u32,
    pub export_ttl_hours: u16,
    /// Snapshot for `logs.export`; generated at startup from the same code as
    /// `printkit doctor --json`.
    pub doctor_report: serde_json::Value,
}

/// Fill an export-signing key, refusing a failed or all-zero result.
///
/// An all-zero key is what a silent `unwrap_or_default` would produce; it is
/// predictable, so it is treated exactly like a failed RNG read.
fn export_secret_from<E>(fill: impl FnOnce(&mut [u8; 32]) -> Result<(), E>) -> Option<[u8; 32]> {
    let mut secret = [0_u8; 32];
    fill(&mut secret).ok()?;
    (!secret.iter().all(|byte| *byte == 0)).then_some(secret)
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        data_dir: PathBuf,
        port: u16,
        store: Arc<Store>,
        plugins: Arc<plugins::PluginsState>,
        admin_token: String,
        sink: Arc<dyn Sink>,
        pool: Arc<RenderPool>,
        print_backend: Arc<dyn PrinterBackend>,
        render_host: Arc<dyn RenderHost>,
        fetch: FetchPolicy,
        records_retention_days: u16,
        records_max_rows: u32,
        export_ttl_hours: u16,
        preview_windows: u8,
        doctor_report: serde_json::Value,
    ) -> Self {
        Self {
            data_dir,
            plugins,
            store,
            admin_token,
            export_secret: export_secret_from(|secret| getrandom::fill(secret).map_err(|_| ())),
            draining: AtomicBool::new(false),
            revoked_origins: Mutex::new(std::collections::HashSet::new()),
            admin_origins: vec![
                format!("http://127.0.0.1:{port}"),
                format!("http://localhost:{port}"),
            ],
            sessions: Sessions::new(),
            sink,
            pool,
            print_backend,
            render_host,
            print_jobs: PrintJobs::new(),
            job_events: Arc::new(events::JobEventHub::new()),
            fetch,
            server: Mutex::new(None),
            connections: Arc::new(AtomicUsize::new(0)),
            batch_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            login_failures: Mutex::new((0, std::time::Instant::now())),
            job_pages: Mutex::new(HashMap::new()),
            connection_origins: Arc::new(Mutex::new(HashMap::new())),
            idempotency: Mutex::new(IdempotencyCache::new()),
            idempotency_locks: Mutex::new(HashMap::new()),
            records_retention_days,
            records_max_rows,
            export_ttl_hours,
            previews: preview::PreviewManager::new(preview_windows),
            doctor_report,
        }
    }
}

/// Stop accepting jobs and wait up to `timeout` for reserved work to finish.
///
/// Shared by `/admin/shutdown` and the SIGTERM/Ctrl-C path so both report the
/// same `drained`/`inflight` result and use the same two-second bound.
pub(crate) async fn drain_jobs(state: &AppState, timeout: Duration) -> (bool, usize) {
    state
        .draining
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let deadline = Instant::now() + timeout;
    while state.pool.inflight() > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let inflight = state.pool.inflight();
    (inflight == 0, inflight)
}

/// Build the cache key for one client-supplied idempotency string.
///
/// Transport, method and session origin are part of the key, so the same
/// string sent over HTTP and WebSocket, over two methods, or from two origins
/// can never reuse another transport's remembered job.
pub(crate) fn idempotency_key(
    transport: &str,
    method: &str,
    origin: Option<&str>,
    key: &str,
) -> String {
    format!("{transport}|{method}|{}|{key}", origin.unwrap_or("local"))
}

/// How long a remembered idempotency key answers duplicate requests.
pub(crate) const IDEMPOTENCY_TTL: Duration = Duration::from_secs(600);

/// Hard cap on remembered keys. A duplicate older than this many newer jobs
/// starts a new print instead of replaying an ancient one.
const MAX_IDEMPOTENCY_ENTRIES: usize = 1024;

/// Hard cap on the approximate bytes the cache may keep. Keys and job ids are
/// short in practice; the cap stops a client that sends 128-byte keys from
/// growing the map past a megabyte.
const MAX_IDEMPOTENCY_BYTES: usize = 1024 * 1024;

/// Fixed per-entry bookkeeping cost charged against [`MAX_IDEMPOTENCY_BYTES`].
const IDEMPOTENCY_ENTRY_OVERHEAD: usize = 64;

/// What a duplicate request is answered with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdempotencyEntry {
    pub job_id: String,
    pub doc_count: u32,
    pub page_count: u32,
}

#[derive(Debug)]
struct IdempotencySlot {
    entry: IdempotencyEntry,
    at: Instant,
    seq: u64,
    cost: usize,
}

/// A small LRU cache for idempotency keys.
///
/// Entries expire after [`IDEMPOTENCY_TTL`] and the map is capped by both
/// entry count and approximate bytes. The least recently used entry is evicted
/// first; `seq` orders entries without a second map, and stale order entries
/// are skipped when they surface.
#[derive(Debug, Default)]
pub(crate) struct IdempotencyCache {
    entries: HashMap<String, IdempotencySlot>,
    order: VecDeque<(u64, String)>,
    next_seq: u64,
    bytes: usize,
}

impl IdempotencyCache {
    /// Empty cache.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Look up a key, refreshing its recency. Expired entries are dropped
    /// first, so a caller only ever sees a live answer.
    pub(crate) fn get(&mut self, key: &str) -> Option<IdempotencyEntry> {
        self.prune_expired(Instant::now());
        let slot = self.entries.get(key)?;
        let entry = slot.entry.clone();
        self.next_seq = self.next_seq.wrapping_add(1);
        let seq = self.next_seq;
        if let Some(slot) = self.entries.get_mut(key) {
            slot.seq = seq;
        }
        self.order.push_back((seq, key.to_owned()));
        Some(entry)
    }

    /// Remember one key, evicting expired and then least-recently-used
    /// entries until both caps hold.
    pub(crate) fn insert(&mut self, key: String, entry: IdempotencyEntry) {
        let now = Instant::now();
        self.prune_expired(now);
        self.remove(&key);
        self.next_seq = self.next_seq.wrapping_add(1);
        let seq = self.next_seq;
        let cost = key
            .len()
            .saturating_add(entry.job_id.len())
            .saturating_add(IDEMPOTENCY_ENTRY_OVERHEAD);
        self.bytes = self.bytes.saturating_add(cost);
        self.entries.insert(
            key.clone(),
            IdempotencySlot {
                entry,
                at: now,
                seq,
                cost,
            },
        );
        self.order.push_back((seq, key));
        self.evict_oversize();
    }

    fn remove(&mut self, key: &str) {
        if let Some(slot) = self.entries.remove(key) {
            self.bytes = self.bytes.saturating_sub(slot.cost);
        }
    }

    fn prune_expired(&mut self, now: Instant) {
        let mut bytes = 0_usize;
        self.entries.retain(|_, slot| {
            let keep = now.saturating_duration_since(slot.at) < IDEMPOTENCY_TTL;
            if keep {
                bytes = bytes.saturating_add(slot.cost);
            }
            keep
        });
        self.bytes = bytes;
        // Expiry leaves tombstones in the order deque. Rebuild it once they
        // outnumber live entries, so a long-running service cannot grow the
        // deque through expired keys alone.
        if self.order.len() > self.entries.len().saturating_mul(4).saturating_add(64) {
            let mut live: Vec<(u64, String)> = self
                .entries
                .iter()
                .map(|(key, slot)| (slot.seq, key.clone()))
                .collect();
            live.sort_unstable_by_key(|(seq, _)| *seq);
            self.order = live.into();
        }
    }

    fn evict_oversize(&mut self) {
        while self.entries.len() > MAX_IDEMPOTENCY_ENTRIES || self.bytes > MAX_IDEMPOTENCY_BYTES {
            let Some((seq, key)) = self.order.pop_front() else {
                // No order entry can only mean the map is already empty; the
                // caps are then satisfied and the loop exits.
                break;
            };
            let stale = self.entries.get(&key).is_none_or(|slot| slot.seq != seq);
            if !stale {
                self.remove(&key);
            }
        }
    }

    /// Number of live keys; tests assert the bound.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    /// Approximate retained bytes; tests assert the byte cap.
    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Drop per-key idempotency locks that no request holds or waits on.
///
/// The map itself keeps one strong reference; a live holder or waiter keeps a
/// second. Once a key has no user it can be recreated on demand, so retaining it
/// would only grow memory alongside the 600-second cache window.
pub(crate) fn prune_idempotency_locks(locks: &mut HashMap<String, Arc<tokio::sync::Mutex<()>>>) {
    locks.retain(|_, lock| Arc::strong_count(lock) > 1);
}

/// Decode percent escapes exactly once.
///
/// Invalid escapes and control bytes are refused: a gate must never inspect a
/// different string from the one the router or an OS path call will use.
pub(crate) fn percent_decode_once(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let high = hex_value(*bytes.get(index + 1)?)?;
                let low = hex_value(*bytes.get(index + 2)?)?;
                decoded.push(high << 4 | low);
                index += 3;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    if decoded.iter().any(|byte| *byte < 0x20 || *byte == 0x7f) {
        return None;
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Canonical path used by every gate.
///
/// Percent escapes are decoded once, repeated slashes collapse, and `.`, `..`
/// and trailing-dot segments are refused. Gates and the router must classify
/// the same request the same way, or an encoded spelling can reach a handler
/// whose gate never ran.
pub(crate) fn gate_path(raw: &str) -> Option<String> {
    let decoded = percent_decode_once(raw)?;
    let mut out = String::with_capacity(decoded.len() + 1);
    for segment in decoded.split('/') {
        if segment.is_empty() {
            continue;
        }
        if segment == "." || segment == ".." || segment.ends_with('.') {
            return None;
        }
        out.push('/');
        out.push_str(segment);
    }
    if out.is_empty() {
        out.push('/');
    }
    Some(out)
}

/// Whether a normalized path belongs to the host-guarded surface.
fn guarded_path(path: &str) -> bool {
    path == "/print"
        || path.starts_with("/print/")
        || path.starts_with("/admin")
        || path.starts_with("/records")
}

/// Control-plane routes. There is no HTML surface: every response is JSON, a
/// WebSocket frame, or a binary download.
/// Reject DNS-rebinding hosts on every authenticated/browser-facing route.
/// `/exports` keeps its own check because it is intentionally unauthenticated.
#[handler]
async fn host_guard(req: &mut Request, res: &mut Response, ctrl: &mut FlowCtrl, depot: &mut Depot) {
    // An unnormalizable path may still reach a guarded handler, so it is
    // guarded too instead of falling through the host check.
    let guarded = gate_path(req.uri().path()).is_none_or(|path| guarded_path(&path));
    if !guarded {
        return;
    }
    let Some(state) = depot.obtain::<Arc<AppState>>().ok().cloned() else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Text::Plain("service state is missing"));
        ctrl.skip_rest();
        return;
    };
    let host = req
        .headers()
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let allowed = state.admin_origins.iter().any(|origin| {
        origin
            .strip_prefix("http://")
            .is_some_and(|allowed| allowed.eq_ignore_ascii_case(host))
    });
    if !allowed {
        res.status_code(StatusCode::FORBIDDEN);
        res.render(Text::Plain("the service only answers loopback hosts"));
        ctrl.skip_rest();
    }
}

#[handler]
async fn no_store_admin(
    req: &mut Request,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
    depot: &mut Depot,
) {
    ctrl.call_next(req, depot, res).await;
    let path = gate_path(req.uri().path()).unwrap_or_default();
    if path.starts_with("/admin") || path.starts_with("/records") {
        res.headers_mut()
            .insert("cache-control", HeaderValue::from_static("no-store"));
    }
}

/// The default Salvo catcher renders HTML for 404/405. Every other failure path
/// on this service is JSON, so replace that body with the same envelope.
#[handler]
pub(crate) async fn json_catcher(
    _req: &mut Request,
    res: &mut Response,
    _ctrl: &mut FlowCtrl,
    _depot: &mut Depot,
) {
    let status = res.status_code.unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let code = match status {
        StatusCode::NOT_FOUND => "E_NOT_FOUND",
        StatusCode::METHOD_NOT_ALLOWED => "E_PROTOCOL",
        StatusCode::PAYLOAD_TOO_LARGE => "E_TOO_LARGE",
        StatusCode::TOO_MANY_REQUESTS => "E_AUTH",
        StatusCode::FORBIDDEN => "E_ORIGIN",
        StatusCode::UNAUTHORIZED => "E_AUTH",
        _ => "E_JOB_FAILED",
    };
    let message = status.canonical_reason().unwrap_or("request failed");
    res.render(Json(json!({
        "ok": false,
        "error": { "code": code, "message": message }
    })));
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .hoop(affix_state::inject(state))
        .hoop(host_guard)
        .hoop(plugins::plugin_gate)
        .hoop(no_store_admin)
        .push(Router::with_path("admin/login").post(admin::login))
        .push(Router::with_path("admin/session").get(admin::session))
        .push(Router::with_path("admin/logout").post(admin::logout))
        .push(
            Router::with_path("admin/origins")
                .get(admin::list_origins)
                .post(admin::add_origin)
                .delete(admin::remove_origin),
        )
        .push(Router::with_path("admin/tokens").post(admin::issue_token))
        .push(Router::with_path("admin/tokens/{tokenId}").delete(admin::revoke_token))
        .push(Router::with_path("admin/preview").post(admin::preview))
        .push(Router::with_path("admin/jobs/{jobId}").get(admin::job_status))
        .push(Router::with_path("admin/jobs/{jobId}/cancel").post(admin::job_cancel))
        .push(Router::with_path("admin/shutdown").post(admin::shutdown))
        .push(
            Router::with_path("admin/plugins")
                .get(plugins::list)
                .post(plugins::set),
        )
        .push(Router::with_path("admin/plugin-lease").post(plugins::lease))
        .push(Router::with_path("admin/logs/events").get(observability::logs_events))
        .push(Router::with_path("admin/logs/export").post(observability::logs_export))
        .push(Router::with_path("records/list").get(records::list_http))
        .push(Router::with_path("records/stats").get(records::stats_http))
        .push(Router::with_path("records/summary").get(records::summary_http))
        .push(Router::with_path("records/export.csv").get(records::csv_http))
        .push(
            Router::with_path("exports/{*+rest}")
                .get(exports::serve)
                .head(exports::serve),
        )
        .push(Router::with_path("print/batch").post(print_batch::receive))
        .push(Router::with_path("print").goal(print_socket))
}

#[handler]
async fn print_socket(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let state = depot.obtain::<Arc<AppState>>().ok().cloned();
    let Some(state) = state else {
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Text::Plain("service state is missing"));
        return;
    };
    ws::upgrade(req, res, state).await;
}

#[cfg(test)]
mod tests {
    use printkit_facade::{
        JobPhase, MemorySink, PoolConfig, PrinterBackend, RenderHost, RenderPool, Sink, Stage,
        TraceEvent,
    };
    use printkit_platform::test_support::{FakePrinterBackend, FakeRenderHost};
    use printkit_protocol::{
        ErrorCode, Hello, Request, Response as ProtocolResponse, Scope, WarningCode,
    };
    use salvo::test::ResponseExt;
    use serde_json::{Value, json};

    use super::auth::{LocalPrintPolicy, Session, authenticate, authenticate_with};
    use super::lock::hash_token;
    use super::ws::{Dispatch, dispatch};
    use super::*;

    #[test]
    fn unused_idempotency_locks_are_reclaimed() {
        let mut locks = HashMap::new();
        let idle = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert("idle".to_owned(), idle);
        let held = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert("held".to_owned(), Arc::clone(&held));
        prune_idempotency_locks(&mut locks);
        assert!(!locks.contains_key("idle"));
        assert!(locks.contains_key("held"));
        drop(held);
        prune_idempotency_locks(&mut locks);
        assert!(locks.is_empty());
    }

    #[test]
    fn gate_paths_decode_once_and_collapse_empty_segments() {
        assert_eq!(
            gate_path("/%61dmin/logs/events").as_deref(),
            Some("/admin/logs/events")
        );
        assert_eq!(
            gate_path("//admin//logs//events").as_deref(),
            Some("/admin/logs/events")
        );
        assert_eq!(
            gate_path("/admin/%6cogs/events").as_deref(),
            Some("/admin/logs/events")
        );
        assert_eq!(gate_path("/admin/%2flogs"), Some("/admin/logs".to_owned()));
        assert!(gate_path("/admin/./logs").is_none());
        assert!(gate_path("/admin/../admin").is_none());
        assert!(gate_path("/admin/logs./events").is_none());
        assert!(gate_path("/%zz").is_none());
        assert!(gate_path("/admin/%00").is_none());
    }

    #[test]
    fn route_ownership_sees_through_encoded_paths() {
        assert_eq!(
            super::plugins::route_owner("GET", "/%61dmin/logs/events"),
            Some("observability")
        );
        assert_eq!(
            super::plugins::route_owner("GET", "//admin//logs//events"),
            Some("observability")
        );
        assert_eq!(
            super::plugins::route_owner("GET", "/admin/../admin/logs/events"),
            None
        );
        assert_eq!(
            super::plugins::route_owner("GET", "/admin/logs./events"),
            None
        );
    }

    fn response_cookie(response: &salvo::Response, name: &str) -> Option<String> {
        response
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(|value| {
                value
                    .split(';')
                    .next()
                    .and_then(|pair| pair.strip_prefix(&format!("{name}=")))
                    .map(str::to_owned)
            })
    }

    #[tokio::test]
    async fn the_origin_whitelist_supports_delete() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let base = "http://127.0.0.1:19080";
        let initial = salvo::test::TestClient::get(format!("{base}/admin/session"))
            .add_header("host", "127.0.0.1:19080", true)
            .send(router(Arc::clone(&world)))
            .await;
        let csrf = response_cookie(&initial, "pk_csrf").expect("a csrf cookie");
        let login = salvo::test::TestClient::post(format!("{base}/admin/login"))
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("origin", base, true)
            .add_header("cookie", format!("pk_csrf={csrf}"), true)
            .add_header("x-csrf-token", &csrf, true)
            .json(&json!({ "token": "admin-secret" }))
            .send(router(Arc::clone(&world)))
            .await;
        assert_eq!(login.status_code.map(|code| code.as_u16()), Some(200));
        let session = response_cookie(&login, "pk_admin").expect("an admin session");
        let cookies = format!("pk_admin={session}; pk_csrf={csrf}");
        let add = salvo::test::TestClient::post(format!("{base}/admin/origins"))
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("origin", base, true)
            .add_header("cookie", &cookies, true)
            .add_header("x-csrf-token", &csrf, true)
            .json(&json!({ "origin": "https://app.example" }))
            .send(router(Arc::clone(&world)))
            .await;
        assert_eq!(add.status_code.map(|code| code.as_u16()), Some(200));
        let remove = salvo::test::TestClient::delete(format!("{base}/admin/origins"))
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("origin", base, true)
            .add_header("cookie", &cookies, true)
            .add_header("x-csrf-token", &csrf, true)
            .json(&json!({ "origin": "https://app.example" }))
            .send(router(Arc::clone(&world)))
            .await;
        assert_eq!(remove.status_code.map(|code| code.as_u16()), Some(200));
        assert!(world.store.origins().unwrap().is_empty());
        let again = salvo::test::TestClient::delete(format!("{base}/admin/origins"))
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("origin", base, true)
            .add_header("cookie", &cookies, true)
            .add_header("x-csrf-token", &csrf, true)
            .json(&json!({ "origin": "https://app.example" }))
            .send(router(Arc::clone(&world)))
            .await;
        assert_eq!(again.status_code.map(|code| code.as_u16()), Some(200));
    }

    struct NoPrinterBackend(FakePrinterBackend);

    impl PrinterBackend for NoPrinterBackend {
        fn printers(
            &self,
        ) -> Result<Vec<printkit_platform::PrinterInfo>, printkit_platform::PlatformError> {
            self.0.printers()
        }
        fn papers(
            &self,
            printer: &str,
        ) -> Result<Vec<printkit_platform::PaperInfo>, printkit_platform::PlatformError> {
            self.0.papers(printer)
        }
        fn submit(
            &self,
            job: &printkit_platform::PrintJob,
        ) -> Result<printkit_platform::SubmittedJob, printkit_platform::PlatformError> {
            let name = job
                .printer
                .clone()
                .unwrap_or_else(|| "fake-printer".to_owned());
            Err(printkit_platform::PlatformError::PrinterNotFound(name))
        }
        fn prints_pdf(&self) -> bool {
            true
        }
        fn submit_raw(
            &self,
            job: &printkit_platform::RawJob,
        ) -> Result<printkit_platform::SubmittedJob, printkit_platform::PlatformError> {
            self.0.submit_raw(job)
        }
        fn cancel_submitted(
            &self,
            job: &printkit_platform::SubmittedJob,
        ) -> Result<(), printkit_platform::PlatformError> {
            self.0.cancel_submitted(job)
        }
        fn cancel_submitted_supported(&self) -> bool {
            true
        }
    }

    async fn admin_cookies(world: &Arc<AppState>, base: &str) -> String {
        let initial = salvo::test::TestClient::get(format!("{base}/admin/session"))
            .add_header("host", "127.0.0.1:19080", true)
            .send(router(Arc::clone(world)))
            .await;
        let csrf = response_cookie(&initial, "pk_csrf").expect("a csrf cookie");
        let login = salvo::test::TestClient::post(format!("{base}/admin/login"))
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("origin", base, true)
            .add_header("cookie", format!("pk_csrf={csrf}"), true)
            .add_header("x-csrf-token", &csrf, true)
            .json(&json!({ "token": "admin-secret" }))
            .send(router(Arc::clone(world)))
            .await;
        let session = response_cookie(&login, "pk_admin").expect("an admin session");
        format!("pk_admin={session}; pk_csrf={csrf}")
    }

    #[tokio::test]
    async fn one_pdf_batch_is_passthrough_and_carries_no_merge_warning() {
        let temp = tempfile::tempdir().unwrap();
        let backend = FakePrinterBackend::new();
        let world = state_with_backend(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
            Arc::new(backend.clone()),
        );
        let base = "http://127.0.0.1:19080";
        let cookies = admin_cookies(&world, base).await;
        let pdf = include_bytes!("../../../../tests/baseline/table-multipage.pdf");
        let mut body =
            b"--b\r\nContent-Type: application/json\r\n\r\n{\"copies\":1}\r\n--b\r\nContent-Type: application/pdf\r\n\r\n"
                .to_vec();
        body.extend_from_slice(pdf);
        body.extend_from_slice(b"\r\n--b--\r\n");
        let mut batch = salvo::test::TestClient::post(format!("{base}/print/batch"))
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("content-type", "multipart/mixed; boundary=b", true)
            .bytes(body)
            .send(router(Arc::clone(&world)))
            .await;
        assert_eq!(batch.status_code.map(|code| code.as_u16()), Some(200));
        let value: Value = batch.take_json().await.unwrap();
        let job_id = value["jobId"].as_str().expect("a job id").to_owned();

        let mut terminal = None;
        for _ in 0..100 {
            let mut reply = salvo::test::TestClient::get(format!("{base}/admin/jobs/{job_id}"))
                .add_header("host", "127.0.0.1:19080", true)
                .add_header("cookie", &cookies, true)
                .send(router(Arc::clone(&world)))
                .await;
            assert_eq!(reply.status_code.map(|code| code.as_u16()), Some(200));
            let status: Value = reply.take_json().await.unwrap();
            if status["status"] == json!("done") {
                terminal = Some(status);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let terminal = terminal.expect("the passthrough job finished");
        assert_eq!(
            terminal["warnings"],
            json!([]),
            "one PDF must not report a merge warning: {terminal}"
        );
        assert_eq!(
            terminal["pages"],
            json!(printkit_pdf::page_count(pdf).unwrap()),
            "{terminal}"
        );
        let submitted = backend.submitted();
        assert_eq!(submitted.len(), 1);
        assert_eq!(
            submitted[0].pdf.as_ref(),
            pdf,
            "the spooler must receive the original bytes unchanged"
        );
    }

    #[tokio::test]
    async fn two_pdf_batch_reports_the_metadata_merge_warning() {
        let temp = tempfile::tempdir().unwrap();
        let backend = FakePrinterBackend::new();
        let world = state_with_backend(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
            Arc::new(backend.clone()),
        );
        let base = "http://127.0.0.1:19080";
        let cookies = admin_cookies(&world, base).await;
        let pdf = include_bytes!("../../../../tests/baseline/table-multipage.pdf");
        let mut body =
            b"--b\r\nContent-Type: application/json\r\n\r\n{\"copies\":1}\r\n--b\r\nContent-Type: application/pdf\r\n\r\n"
                .to_vec();
        body.extend_from_slice(pdf);
        body.extend_from_slice(b"\r\n--b\r\nContent-Type: application/pdf\r\n\r\n");
        body.extend_from_slice(pdf);
        body.extend_from_slice(b"\r\n--b--\r\n");
        let mut batch = salvo::test::TestClient::post(format!("{base}/print/batch"))
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("content-type", "multipart/mixed; boundary=b", true)
            .bytes(body)
            .send(router(Arc::clone(&world)))
            .await;
        assert_eq!(batch.status_code.map(|code| code.as_u16()), Some(200));
        let value: Value = batch.take_json().await.unwrap();
        let job_id = value["jobId"].as_str().expect("a job id").to_owned();

        let mut terminal = None;
        for _ in 0..100 {
            let mut reply = salvo::test::TestClient::get(format!("{base}/admin/jobs/{job_id}"))
                .add_header("host", "127.0.0.1:19080", true)
                .add_header("cookie", &cookies, true)
                .send(router(Arc::clone(&world)))
                .await;
            assert_eq!(reply.status_code.map(|code| code.as_u16()), Some(200));
            let status: Value = reply.take_json().await.unwrap();
            if status["status"] == json!("done") {
                terminal = Some(status);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let terminal = terminal.expect("the merged job finished");
        let warnings = terminal["warnings"].as_array().expect("a warnings array");
        assert!(
            warnings
                .iter()
                .any(|warning| warning["code"] == json!("W_PDF_MERGE_METADATA_LOST")),
            "{terminal}"
        );
        assert_eq!(
            terminal["pages"],
            json!(2 * printkit_pdf::page_count(pdf).unwrap()),
            "{terminal}"
        );
    }

    #[tokio::test]
    async fn admin_job_status_tracks_a_batch_and_reports_unknown_ids() {
        let temp = tempfile::tempdir().unwrap();
        let backend = FakePrinterBackend::new();
        let world = state_with_backend(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
            Arc::new(backend.clone()),
        );
        let base = "http://127.0.0.1:19080";
        let initial = salvo::test::TestClient::get(format!("{base}/admin/session"))
            .add_header("host", "127.0.0.1:19080", true)
            .send(router(Arc::clone(&world)))
            .await;
        let csrf = response_cookie(&initial, "pk_csrf").expect("a csrf cookie");
        let login = salvo::test::TestClient::post(format!("{base}/admin/login"))
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("origin", base, true)
            .add_header("cookie", format!("pk_csrf={csrf}"), true)
            .add_header("x-csrf-token", &csrf, true)
            .json(&json!({ "token": "admin-secret" }))
            .send(router(Arc::clone(&world)))
            .await;
        let session = response_cookie(&login, "pk_admin").expect("an admin session");
        let cookies = format!("pk_admin={session}; pk_csrf={csrf}");
        let document = include_str!("../../../../tests/fixtures/minimal.json");
        let body = format!(
            "--b\r\nContent-Type: application/json\r\n\r\n{{}}\r\n--b\r\nContent-Type: application/json\r\n\r\n{document}\r\n--b--\r\n"
        );
        let mut batch = salvo::test::TestClient::post(format!("{base}/print/batch"))
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("content-type", "multipart/mixed; boundary=b", true)
            .bytes(body.into_bytes())
            .send(router(Arc::clone(&world)))
            .await;
        assert_eq!(batch.status_code.map(|code| code.as_u16()), Some(200));
        let value: Value = batch.take_json().await.unwrap();
        let job_id = value["jobId"].as_str().expect("a job id").to_owned();

        let mut terminal = None;
        for _ in 0..100 {
            let mut reply = salvo::test::TestClient::get(format!("{base}/admin/jobs/{job_id}"))
                .add_header("host", "127.0.0.1:19080", true)
                .add_header("cookie", &cookies, true)
                .send(router(Arc::clone(&world)))
                .await;
            assert_eq!(reply.status_code.map(|code| code.as_u16()), Some(200));
            let status: Value = reply.take_json().await.unwrap();
            if matches!(
                status["status"].as_str(),
                Some("done" | "failed" | "cancelled")
            ) {
                terminal = Some(status);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let terminal = terminal.expect("the job reached a terminal state");
        assert_eq!(terminal["status"], json!("done"), "{terminal}");

        let missing = salvo::test::TestClient::get(format!("{base}/admin/jobs/job-missing"))
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("cookie", &cookies, true)
            .send(router(Arc::clone(&world)))
            .await;
        assert_eq!(missing.status_code.map(|code| code.as_u16()), Some(404));
    }

    #[tokio::test]
    async fn admin_job_status_reports_a_submit_failure() {
        let temp = tempfile::tempdir().unwrap();
        let world = state_with_backend(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
            Arc::new(NoPrinterBackend(FakePrinterBackend::new())),
        );
        let base = "http://127.0.0.1:19080";
        let initial = salvo::test::TestClient::get(format!("{base}/admin/session"))
            .add_header("host", "127.0.0.1:19080", true)
            .send(router(Arc::clone(&world)))
            .await;
        let csrf = response_cookie(&initial, "pk_csrf").expect("a csrf cookie");
        let login = salvo::test::TestClient::post(format!("{base}/admin/login"))
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("origin", base, true)
            .add_header("cookie", format!("pk_csrf={csrf}"), true)
            .add_header("x-csrf-token", &csrf, true)
            .json(&json!({ "token": "admin-secret" }))
            .send(router(Arc::clone(&world)))
            .await;
        let session = response_cookie(&login, "pk_admin").expect("an admin session");
        let cookies = format!("pk_admin={session}; pk_csrf={csrf}");
        let document = include_str!("../../../../tests/fixtures/minimal.json");
        let body = format!(
            "--b\r\nContent-Type: application/json\r\n\r\n{{\"printer\":\"missing\"}}\r\n--b\r\nContent-Type: application/json\r\n\r\n{document}\r\n--b--\r\n"
        );
        let mut batch = salvo::test::TestClient::post(format!("{base}/print/batch"))
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("content-type", "multipart/mixed; boundary=b", true)
            .bytes(body.into_bytes())
            .send(router(Arc::clone(&world)))
            .await;
        assert_eq!(batch.status_code.map(|code| code.as_u16()), Some(200));
        let value: Value = batch.take_json().await.unwrap();
        let job_id = value["jobId"].as_str().expect("a job id").to_owned();
        let mut failed = None;
        for _ in 0..100 {
            let mut reply = salvo::test::TestClient::get(format!("{base}/admin/jobs/{job_id}"))
                .add_header("host", "127.0.0.1:19080", true)
                .add_header("cookie", &cookies, true)
                .send(router(Arc::clone(&world)))
                .await;
            assert_eq!(reply.status_code.map(|code| code.as_u16()), Some(200));
            let status: Value = reply.take_json().await.unwrap();
            if status["status"] == json!("failed") {
                failed = Some(status);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let failed = failed.expect("the job failed");
        assert_eq!(
            failed["error"]["code"],
            json!("E_PRINTER_NOT_FOUND"),
            "{failed}"
        );
    }

    #[tokio::test]
    async fn a_foreign_host_is_refused_for_an_encoded_admin_path() {
        let temp = tempfile::tempdir().unwrap();
        let world = core_state(temp.path());
        let response = salvo::test::TestClient::get("http://127.0.0.1:19080/%61dmin/logs/events")
            .add_header("host", "evil.example", true)
            .send(router(world))
            .await;
        assert_eq!(response.status_code.map(|code| code.as_u16()), Some(403));
    }

    #[tokio::test]
    async fn an_encoded_admin_route_cannot_skip_the_plugin_gate() {
        let temp = tempfile::tempdir().unwrap();
        let world = core_state(temp.path());
        let response = salvo::test::TestClient::get("http://127.0.0.1:19080/%61dmin/logs/events")
            .add_header("host", "127.0.0.1:19080", true)
            .send(router(world))
            .await;
        assert_eq!(
            response.status_code.map(|code| code.as_u16()),
            Some(404),
            "observability is off under the core preset"
        );
    }

    #[tokio::test]
    async fn draining_refuses_http_batches() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        world
            .draining
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let response = salvo::test::TestClient::post("http://127.0.0.1:19080/print/batch")
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("content-type", "multipart/mixed; boundary=b", true)
            .bytes(Vec::new())
            .send(router(world))
            .await;
        assert_eq!(
            response.status_code.map(|code| code.as_u16()),
            Some(503),
            "a draining service must refuse new batches"
        );
    }

    #[tokio::test]
    async fn drain_jobs_waits_for_inflight_then_reports_the_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let job = world
            .pool
            .reserve_external(Box::new(|_, _| {}))
            .expect("a reserved job");
        let started = Instant::now();
        let (drained, inflight) = drain_jobs(&world, Duration::from_millis(80)).await;
        assert!(!drained);
        assert_eq!(inflight, 1);
        assert!(
            started.elapsed() >= Duration::from_millis(70),
            "the drain must wait for its timeout"
        );
        drop(job);
        let (drained, inflight) = drain_jobs(&world, Duration::from_secs(1)).await;
        assert!(drained);
        assert_eq!(inflight, 0);
    }

    #[test]
    fn a_failed_or_zero_export_secret_is_refused() {
        assert!(export_secret_from(|_| Err::<(), _>(())).is_none());
        assert!(
            export_secret_from(|_| Ok::<(), ()>(())).is_none(),
            "an all-zero key is as predictable as a failed read"
        );
        let secret = export_secret_from(|secret| {
            secret[0] = 1;
            Ok::<(), ()>(())
        })
        .expect("a filled key");
        assert_eq!(secret[0], 1);
    }

    #[test]
    fn idempotency_keys_are_namespaced_by_transport_method_and_origin() {
        let http = idempotency_key("http", "print/batch", Some("https://a.example"), "same");
        let ws = idempotency_key("ws", "doc.print", Some("https://a.example"), "same");
        assert_ne!(http, ws, "HTTP and WS must not share a key");
        assert_ne!(
            idempotency_key("ws", "doc.print", Some("https://a.example"), "same"),
            idempotency_key("ws", "doc.print", Some("https://b.example"), "same"),
            "origins must not share a key"
        );
        assert_ne!(
            idempotency_key("ws", "doc.print", None, "same"),
            idempotency_key("ws", "doc.printBatch", None, "same"),
            "methods must not share a key"
        );
        assert_eq!(
            idempotency_key("ws", "doc.print", None, "same"),
            "ws|doc.print|local|same"
        );
        assert_eq!(
            idempotency_key("http", "print/batch", None, "same"),
            "http|print/batch|local|same"
        );
    }

    fn idempotency_entry(index: usize) -> IdempotencyEntry {
        IdempotencyEntry {
            job_id: format!("job-{index}"),
            doc_count: 1,
            page_count: 1,
        }
    }

    #[test]
    fn idempotency_cache_is_bounded_and_evicts_the_oldest() {
        let mut cache = IdempotencyCache::new();
        for index in 0..MAX_IDEMPOTENCY_ENTRIES + 40 {
            cache.insert(format!("key-{index}"), idempotency_entry(index));
        }
        assert_eq!(cache.len(), MAX_IDEMPOTENCY_ENTRIES);
        assert!(cache.get("key-0").is_none(), "the oldest key survived");
        assert!(
            cache.get("key-39").is_none(),
            "entries past the cap pushed out the wrong end"
        );
        let newest = format!("key-{}", MAX_IDEMPOTENCY_ENTRIES + 39);
        assert_eq!(cache.get(&newest).unwrap().job_id, "job-1063");
        assert!(cache.bytes() <= MAX_IDEMPOTENCY_BYTES);
    }

    #[test]
    fn idempotency_cache_get_refreshes_recency() {
        let mut cache = IdempotencyCache::new();
        for index in 0..MAX_IDEMPOTENCY_ENTRIES {
            cache.insert(format!("key-{index}"), idempotency_entry(index));
        }
        assert_eq!(cache.get("key-0").unwrap().job_id, "job-0");
        cache.insert("key-new".to_owned(), idempotency_entry(999));
        assert!(
            cache.get("key-0").is_some(),
            "a key that was just read must not be the next eviction"
        );
        assert!(
            cache.get("key-1").is_none(),
            "the least recently used key should be evicted"
        );
        assert!(cache.len() <= MAX_IDEMPOTENCY_ENTRIES);
    }

    #[test]
    fn idempotency_cache_enforces_its_byte_cap() {
        let mut cache = IdempotencyCache::new();
        let padding = "p".repeat(4096);
        for index in 0..600 {
            cache.insert(
                format!("key-{index}-{padding}"),
                IdempotencyEntry {
                    job_id: padding.clone(),
                    doc_count: 1,
                    page_count: 1,
                },
            );
        }
        assert!(cache.bytes() <= MAX_IDEMPOTENCY_BYTES);
        assert!(cache.len() < 600, "the byte cap never evicted an entry");
    }

    #[test]
    fn idempotency_cache_expires_after_ten_minutes() {
        let mut cache = IdempotencyCache::new();
        cache.insert("key".to_owned(), idempotency_entry(1));
        cache.prune_expired(Instant::now() + IDEMPOTENCY_TTL + Duration::from_secs(1));
        assert_eq!(cache.len(), 0);
        assert!(cache.get("key").is_none());
    }

    #[test]
    fn idempotency_cache_stays_bounded_under_concurrent_inserts() {
        let cache = Arc::new(Mutex::new(IdempotencyCache::new()));
        let mut workers = Vec::new();
        for worker in 0..8 {
            let cache = Arc::clone(&cache);
            workers.push(std::thread::spawn(move || {
                for index in 0..500 {
                    cache
                        .lock()
                        .unwrap()
                        .insert(format!("w{worker}-key-{index}"), idempotency_entry(index));
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let cache = cache.lock().unwrap();
        assert!(cache.len() <= MAX_IDEMPOTENCY_ENTRIES);
        assert!(cache.bytes() <= MAX_IDEMPOTENCY_BYTES);
    }

    fn pool(host: &FakeRenderHost) -> Arc<RenderPool> {
        let host = Arc::new(host.clone());
        RenderPool::new(
            PoolConfig::new(1, 8, std::time::Duration::from_secs(10)),
            Arc::new(move || host.clone() as Arc<dyn RenderHost>),
        )
    }

    fn state(
        data_dir: &std::path::Path,
        sink: Arc<dyn Sink>,
        pool: Arc<RenderPool>,
    ) -> Arc<AppState> {
        state_with_backend(data_dir, sink, pool, Arc::new(FakePrinterBackend::new()))
    }

    fn core_state(data_dir: &std::path::Path) -> Arc<AppState> {
        let store = Arc::new(Store::open(data_dir).unwrap());
        let plugins = Arc::new(plugins::PluginsState::core_for_tests(data_dir));
        Arc::new(AppState::new(
            data_dir.to_path_buf(),
            19080,
            store,
            plugins,
            "admin-secret".to_owned(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
            Arc::new(FakePrinterBackend::new()),
            Arc::new(FakeRenderHost::new()),
            FetchPolicy::default(),
            90,
            5000,
            24,
            8,
            serde_json::json!({}),
        ))
    }

    fn state_with_backend(
        data_dir: &std::path::Path,
        sink: Arc<dyn Sink>,
        pool: Arc<RenderPool>,
        backend: Arc<dyn PrinterBackend>,
    ) -> Arc<AppState> {
        let store = Arc::new(Store::open(data_dir).unwrap());
        let plugins = Arc::new(plugins::PluginsState::all_for_tests(data_dir));
        Arc::new(AppState::new(
            data_dir.to_path_buf(),
            19080,
            store,
            plugins,
            "admin-secret".to_owned(),
            sink,
            pool,
            backend,
            Arc::new(FakeRenderHost::new()),
            FetchPolicy::default(),
            90,
            5000,
            24,
            8,
            serde_json::json!({}),
        ))
    }

    fn state_with_private_fetch(
        data_dir: &std::path::Path,
        sink: Arc<dyn Sink>,
        pool: Arc<RenderPool>,
    ) -> Arc<AppState> {
        let store = Arc::new(Store::open(data_dir).unwrap());
        let plugins = Arc::new(plugins::PluginsState::all_for_tests(data_dir));
        Arc::new(AppState::new(
            data_dir.to_path_buf(),
            19080,
            store,
            plugins,
            "admin-secret".to_owned(),
            sink,
            pool,
            Arc::new(FakePrinterBackend::new()),
            Arc::new(FakeRenderHost::new()),
            FetchPolicy {
                allow_private_networks: true,
                ..FetchPolicy::default()
            },
            90,
            5000,
            24,
            8,
            serde_json::json!({}),
        ))
    }

    /// Dispatch one request and settle a reserved job, for tests that only
    /// observe the response.
    async fn reply(world: &Arc<AppState>, session: &Session, request: Request) -> ProtocolResponse {
        match dispatch(world, session, request).await {
            Dispatch::Reply(response) => response,
            Dispatch::Start { response, start } => {
                start();
                response
            }
            Dispatch::Subscribe { .. } => panic!("the request did not ask for a subscription"),
        }
    }

    /// A world whose render host is also kept by the test, so window state can
    /// be inspected and the Print control can be pressed.
    fn state_with_host(
        data_dir: &std::path::Path,
        sink: Arc<dyn Sink>,
        host: &FakeRenderHost,
    ) -> Arc<AppState> {
        let store = Arc::new(Store::open(data_dir).unwrap());
        let plugins = Arc::new(plugins::PluginsState::all_for_tests(data_dir));
        Arc::new(AppState::new(
            data_dir.to_path_buf(),
            19080,
            store,
            plugins,
            "admin-secret".to_owned(),
            sink,
            pool(host),
            Arc::new(FakePrinterBackend::new()),
            Arc::new(host.clone()),
            FetchPolicy::default(),
            90,
            5000,
            24,
            8,
            serde_json::json!({}),
        ))
    }

    fn minimal_document() -> Value {
        serde_json::from_str(include_str!("../../../../tests/fixtures/minimal.json")).unwrap()
    }

    fn preview_request(id: &str, method: &str, params: Value) -> Request {
        Request {
            id: id.to_owned(),
            method: method.to_owned(),
            params,
        }
    }

    fn session() -> Session {
        Session {
            connection: 1,
            scopes: vec![Scope::Print],
            origin: Some("https://app.example".to_owned()),
        }
    }

    fn raw_session() -> Session {
        Session {
            connection: 2,
            scopes: vec![Scope::Raw],
            origin: Some("https://app.example".to_owned()),
        }
    }

    #[tokio::test]
    async fn an_http_batch_failure_is_not_remembered_and_retry_runs() {
        let temp = tempfile::tempdir().unwrap();
        let backend = FakePrinterBackend::new();
        let world = state_with_backend(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
            Arc::new(backend.clone()),
        );
        let document = include_str!("../../../../tests/fixtures/minimal.json");
        let failure = "--b\r\nContent-Type: application/json\r\n\r\nnot json\r\n--b\r\nContent-Type: application/json\r\n\r\n{}\r\n--b--\r\n".to_owned();
        let success = format!(
            "--b\r\nContent-Type: application/json\r\n\r\n{{}}\r\n--b\r\nContent-Type: application/json\r\n\r\n{document}\r\n--b--\r\n"
        );
        let key = idempotency_key("http", "print/batch", None, "retry-me");

        let first = salvo::test::TestClient::post("http://127.0.0.1:19080/print/batch")
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("content-type", "multipart/mixed; boundary=b", true)
            .add_header("idempotency-key", "retry-me", true)
            .bytes(failure.into_bytes())
            .send(router(Arc::clone(&world)))
            .await;
        assert_eq!(first.status_code.map(|code| code.as_u16()), Some(400));
        assert!(
            world.idempotency.lock().unwrap().get(&key).is_none(),
            "an HTTP error must not be cached"
        );

        let second = salvo::test::TestClient::post("http://127.0.0.1:19080/print/batch")
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("content-type", "multipart/mixed; boundary=b", true)
            .add_header("idempotency-key", "retry-me", true)
            .bytes(success.clone().into_bytes())
            .send(router(Arc::clone(&world)))
            .await;
        assert_eq!(second.status_code.map(|code| code.as_u16()), Some(200));
        assert!(
            world.idempotency.lock().unwrap().get(&key).is_some(),
            "the successful retry must be remembered"
        );
        for _ in 0..50 {
            if !backend.submitted().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            backend.submitted().len(),
            1,
            "the retry after a failure must really submit one job"
        );

        let third = salvo::test::TestClient::post("http://127.0.0.1:19080/print/batch")
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("content-type", "multipart/mixed; boundary=b", true)
            .add_header("idempotency-key", "retry-me", true)
            .bytes(success.into_bytes())
            .send(router(Arc::clone(&world)))
            .await;
        assert_eq!(third.status_code.map(|code| code.as_u16()), Some(200));
        assert_eq!(
            backend.submitted().len(),
            1,
            "the third duplicate must not submit another job"
        );
    }

    #[tokio::test]
    async fn an_oversized_ws_request_id_is_refused_before_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let session = Session {
            connection: 1,
            scopes: vec![Scope::Print],
            origin: None,
        };
        let long = "x".repeat(129);
        let request = Request {
            id: "1".to_owned(),
            method: "doc.print".to_owned(),
            params: serde_json::json!({ "version": 1, "requestId": long }),
        };
        let response = match dispatch(&world, &session, request).await {
            Dispatch::Reply(response) => response,
            _ => panic!("an invalid requestId must be refused before dispatch"),
        };
        assert!(!response.ok);
        let error = response.error.expect("a refusal carries an error");
        assert_eq!(error.code, ErrorCode::Schema);
        assert!(
            error.message.contains("requestId"),
            "the message names the field: {}",
            error.message
        );
        let key = idempotency_key("ws", "doc.print", None, &"x".repeat(129));
        assert!(
            world.idempotency.lock().unwrap().get(&key).is_none(),
            "a refused key must not be remembered"
        );
    }

    #[tokio::test]
    async fn dispatch_does_not_cache_before_the_connection_writes() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let session = Session {
            connection: 1,
            scopes: vec![Scope::Print],
            origin: None,
        };
        let mut document: Value =
            serde_json::from_str(include_str!("../../../../tests/fixtures/minimal.json")).unwrap();
        document["requestId"] = json!("dup");
        let dispatched = dispatch(
            &world,
            &session,
            Request {
                id: "1".to_owned(),
                method: "doc.print".to_owned(),
                params: document,
            },
        )
        .await;
        assert!(
            matches!(dispatched, Dispatch::Start { .. }),
            "a valid document must reserve a job"
        );
        let key = idempotency_key("ws", "doc.print", None, "dup");
        assert!(
            world.idempotency.lock().unwrap().get(&key).is_none(),
            "dispatch must not cache before the response is written and started"
        );
        // Dropping the start closure abandons the reservation, exactly as a
        // failed socket write does.
        drop(dispatched);
    }

    #[tokio::test]
    async fn an_http_batch_with_document_copies_is_refused_with_a_pointer() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let mut document: Value =
            serde_json::from_str(include_str!("../../../../tests/fixtures/minimal.json")).unwrap();
        document["output"] = json!({ "copies": 3 });
        let document = serde_json::to_string(&document).unwrap();
        let body = format!(
            "--b\r\nContent-Type: application/json\r\n\r\n{{\"printer\":\"fake-printer\"}}\r\n--b\r\nContent-Type: application/json\r\n\r\n{document}\r\n--b--\r\n"
        );
        let mut response = salvo::test::TestClient::post("http://127.0.0.1:19080/print/batch")
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("content-type", "multipart/mixed; boundary=b", true)
            .bytes(body.into_bytes())
            .send(router(world))
            .await;
        assert_eq!(
            response.status_code.map(|code| code.as_u16()),
            Some(400),
            "a per-document copies batch must be a schema error"
        );
        let value: Value = response.take_json().await.unwrap();
        assert_eq!(value["error"]["code"], json!("E_SCHEMA"));
        assert_eq!(
            value["error"]["detail"]["pointer"],
            json!("/documents/0/output/copies")
        );
    }

    #[tokio::test]
    async fn a_ws_batch_with_document_copies_is_refused_with_a_pointer() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let mut document: Value =
            serde_json::from_str(include_str!("../../../../tests/fixtures/minimal.json")).unwrap();
        document["output"] = json!({ "copies": 3 });
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "batch".to_owned(),
                method: "doc.printBatch".to_owned(),
                params: json!({ "docs": [document] }),
            },
        )
        .await;
        let response = match dispatched {
            Dispatch::Reply(response) => response,
            _ => panic!("a per-document copies batch must be refused"),
        };
        let error = response.error.expect("a refusal carries an error");
        assert_eq!(error.code, ErrorCode::Schema);
        assert_eq!(
            error
                .detail
                .as_ref()
                .and_then(|detail| detail.get("pointer")),
            Some(&json!("/documents/0/output/copies"))
        );
    }

    fn shaped_pdf(pages: usize, acro_form: bool) -> Vec<u8> {
        let mut document = lopdf::Document::with_version("1.7");
        let pages_id = document.new_object_id();
        let mut kids = Vec::with_capacity(pages);
        for index in 0..pages {
            let content = document.add_object(lopdf::Stream::new(
                lopdf::Dictionary::new(),
                format!("BT /F1 12 Tf 72 720 Td (page {index}) Tj ET").into_bytes(),
            ));
            let mut page = lopdf::Dictionary::new();
            page.set("Type", "Page");
            page.set("Parent", pages_id);
            page.set(
                "MediaBox",
                lopdf::Object::Array(vec![0.into(), 0.into(), 595.into(), 842.into()]),
            );
            page.set("Contents", content);
            kids.push(document.add_object(page).into());
        }
        let mut pages_node = lopdf::Dictionary::new();
        pages_node.set("Type", "Pages");
        pages_node.set("Kids", lopdf::Object::Array(kids));
        pages_node.set("Count", i64::try_from(pages).unwrap());
        document
            .objects
            .insert(pages_id, lopdf::Object::Dictionary(pages_node));
        let catalog_id = document.add_object(lopdf::Object::Dictionary({
            let mut catalog = lopdf::Dictionary::new();
            catalog.set("Type", "Catalog");
            catalog.set("Pages", pages_id);
            if acro_form {
                let mut form = lopdf::Dictionary::new();
                form.set("Fields", lopdf::Object::Array(Vec::new()));
                catalog.set("AcroForm", form);
            }
            catalog
        }));
        document
            .trailer
            .set("Root", lopdf::Object::Reference(catalog_id));
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    fn zero_page_pdf() -> Vec<u8> {
        let mut document = lopdf::Document::with_version("1.4");
        let mut pages = lopdf::Dictionary::new();
        pages.set("Type", "Pages");
        pages.set("Kids", lopdf::Object::Array(Vec::new()));
        pages.set("Count", 0i64);
        let pages_id = document.add_object(lopdf::Object::Dictionary(pages));
        let mut catalog = lopdf::Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", lopdf::Object::Reference(pages_id));
        let catalog_id = document.add_object(lopdf::Object::Dictionary(catalog));
        document
            .trailer
            .set("Root", lopdf::Object::Reference(catalog_id));
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    #[tokio::test]
    async fn a_single_pdf_batch_passes_collate_to_the_spooler() {
        let temp = tempfile::tempdir().unwrap();
        let backend = FakePrinterBackend::new();
        let world = state_with_backend(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
            Arc::new(backend.clone()),
        );
        let pdf = include_bytes!("../../../../tests/baseline/table-multipage.pdf");
        let mut body = Vec::new();
        body.extend_from_slice(
            b"--b\r\nContent-Type: application/json\r\n\r\n{\"copies\":2,\"collate\":false}\r\n",
        );
        body.extend_from_slice(b"--b\r\nContent-Type: application/pdf\r\n\r\n");
        body.extend_from_slice(pdf);
        body.extend_from_slice(b"\r\n--b--\r\n");
        let response = salvo::test::TestClient::post("http://127.0.0.1:19080/print/batch")
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("content-type", "multipart/mixed; boundary=b", true)
            .bytes(body)
            .send(router(world))
            .await;
        assert_eq!(response.status_code.map(|code| code.as_u16()), Some(200));
        for _ in 0..100 {
            if !backend.submitted().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let jobs = backend.submitted();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].copies, 2);
        assert!(
            !jobs[0].collate,
            "meta.collate=false must reach the spooler"
        );
    }

    fn pdf_params(pdfs: &[Vec<u8>], copies: Option<u32>, collate: Option<bool>) -> Value {
        let encode = |bytes: &[u8]| {
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
        };
        let parts: Vec<Value> = pdfs
            .iter()
            .map(|pdf| json!({ "bytes": encode(pdf) }))
            .collect();
        let mut params = json!({ "pdfs": parts });
        if let Some(copies) = copies {
            params["copies"] = json!(copies);
        }
        if let Some(collate) = collate {
            params["collate"] = json!(collate);
        }
        params
    }

    #[tokio::test]
    async fn a_single_pdf_batch_ws_passes_through_without_metadata_warnings() {
        let temp = tempfile::tempdir().unwrap();
        let backend = FakePrinterBackend::new();
        let world = state_with_backend(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
            Arc::new(backend.clone()),
        );
        let pdf = shaped_pdf(2, true);
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "pdf-batch".to_owned(),
                method: "doc.printPdfBatch".to_owned(),
                params: pdf_params(std::slice::from_ref(&pdf), Some(2), Some(false)),
            },
        )
        .await;
        let (response, start) = match dispatched {
            Dispatch::Start { response, start } => (response, start),
            _ => panic!("a single PDF batch must reserve a job"),
        };
        assert!(response.ok, "{:?}", response.error);
        assert!(response.warnings.is_empty(), "{:?}", response.warnings);
        assert_eq!(response.result.as_ref().unwrap()["pages"], json!(2));
        start();
        for _ in 0..100 {
            if !backend.submitted().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let jobs = backend.submitted();
        assert_eq!(jobs[0].pdf, bytes::Bytes::from(pdf));
        assert_eq!(jobs[0].copies, 2);
        assert!(!jobs[0].collate);
    }

    #[tokio::test]
    async fn a_multi_pdf_batch_ws_merges_and_warns_about_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let parts = [shaped_pdf(1, true), shaped_pdf(1, true)];
        let mut received = world.job_events.subscribe("https://app.example");
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "pdf-batch".to_owned(),
                method: "doc.printPdfBatch".to_owned(),
                params: pdf_params(&parts, Some(1), Some(true)),
            },
        )
        .await;
        let (response, start) = match dispatched {
            Dispatch::Start { response, start } => (response, start),
            _ => panic!("a multi PDF batch must reserve a job"),
        };
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(response.result.as_ref().unwrap()["pages"], json!(2));
        assert!(
            response.warnings.is_empty(),
            "the response must not duplicate the done warning: {:?}",
            response.warnings
        );
        start();
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let text = received.recv().await.expect("a job event");
                let value: Value = serde_json::from_str(&text).unwrap();
                if matches!(value["status"].as_str(), Some("done" | "failed")) {
                    return value;
                }
            }
        })
        .await
        .expect("the batch reached a terminal state");
        assert_eq!(event["status"], json!("done"), "{event}");
        assert!(
            event["warnings"].as_array().is_some_and(|warnings| warnings
                .iter()
                .any(|warning| { warning["code"] == json!("W_PDF_MERGE_METADATA_LOST") })),
            "the done event must carry the metadata warning: {event}"
        );
        assert!(WarningCode::PdfMergeMetadataLost.as_str() == "W_PDF_MERGE_METADATA_LOST");
    }

    #[tokio::test]
    async fn a_pdf_batch_request_id_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let pdf = shaped_pdf(1, false);
        let bytes = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &pdf);
        let request = |id: &str| Request {
            id: id.to_owned(),
            method: "doc.printPdfBatch".to_owned(),
            params: json!({
                "pdfs": [{ "bytes": bytes }],
                "requestId": "pdf-batch-dup",
            }),
        };
        let first = deliver_request(&world, request("one")).await;
        let (first_id, first_pages) = match first {
            Dispatch::Start { response, .. } => {
                let result = response.result.unwrap();
                (
                    result["jobId"].as_str().unwrap().to_owned(),
                    result["pages"].as_u64().unwrap(),
                )
            }
            _ => panic!("the first PDF batch must reserve a job"),
        };
        let second = deliver_request(&world, request("two")).await;
        let (second_id, second_pages) = match second {
            Dispatch::Reply(response) => {
                let result = response.result.unwrap();
                (
                    result["jobId"].as_str().unwrap().to_owned(),
                    result["pages"]
                        .as_u64()
                        .expect("duplicate result has pages"),
                )
            }
            _ => panic!("the duplicate must reuse the job"),
        };
        assert_eq!(first_id, second_id);
        assert_eq!(
            first_pages, second_pages,
            "pages must match the first result"
        );
        assert_eq!(second_pages, 1);
    }

    #[tokio::test]
    async fn a_ws_pdf_batch_parses_each_pdf_once() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let parts = [shaped_pdf(1, false), shaped_pdf(1, true)];
        printkit_pdf::reset_parse_count();
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "pdf-batch".to_owned(),
                method: "doc.printPdfBatch".to_owned(),
                params: pdf_params(&parts, Some(1), None),
            },
        )
        .await;
        assert!(matches!(dispatched, Dispatch::Start { .. }));
        assert_eq!(
            printkit_pdf::parse_count(),
            2,
            "each PDF must be parsed exactly once end to end"
        );
    }

    #[tokio::test]
    async fn a_ws_pdf_batch_over_the_frame_limit_is_too_large() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let oversized = "A".repeat(printkit_core::limits::MAX_WS_BASE64_BYTES + 1);
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "pdf-batch".to_owned(),
                method: "doc.printPdfBatch".to_owned(),
                params: json!({ "pdfs": [{ "bytes": oversized }] }),
            },
        )
        .await;
        let response = match dispatched {
            Dispatch::Reply(response) => response,
            _ => panic!("an oversized frame must be refused"),
        };
        let error = response.error.expect("a refusal carries an error");
        assert_eq!(error.code, ErrorCode::TooLarge);
        assert!(
            error.message.contains("WebSocket limit"),
            "{}",
            error.message
        );
    }

    #[tokio::test]
    async fn a_pdf_batch_part_with_copies_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let pdf = shaped_pdf(1, false);
        for copies in [0_u32, 2] {
            let encode = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &pdf);
            let dispatched = dispatch(
                &world,
                &session(),
                Request {
                    id: "pdf-batch".to_owned(),
                    method: "doc.printPdfBatch".to_owned(),
                    params: json!({ "pdfs": [{ "bytes": encode, "copies": copies }] }),
                },
            )
            .await;
            let response = match dispatched {
                Dispatch::Reply(response) => response,
                _ => panic!("per-part copies must be refused"),
            };
            assert_eq!(response.error.unwrap().code, ErrorCode::Schema);
        }
    }

    #[tokio::test]
    async fn a_pdf_batch_over_the_page_ceiling_is_too_large() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let pdf = shaped_pdf(2001, false);
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "pdf-batch".to_owned(),
                method: "doc.printPdfBatch".to_owned(),
                params: pdf_params(&[pdf], Some(1), None),
            },
        )
        .await;
        let response = match dispatched {
            Dispatch::Reply(response) => response,
            _ => panic!("2001 pages must be refused"),
        };
        assert_eq!(response.error.unwrap().code, ErrorCode::TooLarge);
    }

    #[tokio::test]
    async fn a_ws_print_pdf_zero_page_error_points_at_pdf_pages() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let bytes =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, zero_page_pdf());
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "pdf".to_owned(),
                method: "doc.printPdf".to_owned(),
                params: json!({ "bytes": bytes }),
            },
        )
        .await;
        let response = match dispatched {
            Dispatch::Reply(response) => response,
            _ => panic!("a zero-page PDF must be refused before reserving"),
        };
        let error = response.error.expect("a refusal carries an error");
        assert_eq!(error.code, ErrorCode::Schema);
        assert_eq!(
            error
                .detail
                .as_ref()
                .and_then(|detail| detail.get("pointer")),
            Some(&json!("/pdf/pages"))
        );
    }

    #[tokio::test]
    async fn a_single_pdf_batch_zero_page_error_points_at_pdf_pages() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let pdf = zero_page_pdf();
        let mut body = Vec::new();
        body.extend_from_slice(b"--b\r\nContent-Type: application/json\r\n\r\n{}\r\n");
        body.extend_from_slice(b"--b\r\nContent-Type: application/pdf\r\n\r\n");
        body.extend_from_slice(&pdf);
        body.extend_from_slice(b"\r\n--b--\r\n");
        let mut response = salvo::test::TestClient::post("http://127.0.0.1:19080/print/batch")
            .add_header("host", "127.0.0.1:19080", true)
            .add_header("content-type", "multipart/mixed; boundary=b", true)
            .bytes(body)
            .send(router(world))
            .await;
        assert_eq!(response.status_code.map(|code| code.as_u16()), Some(400));
        let value: Value = response.take_json().await.unwrap();
        assert_eq!(value["error"]["code"], json!("E_SCHEMA"));
        assert_eq!(value["error"]["detail"]["pointer"], json!("/pdf/pages"));
    }

    #[tokio::test]
    async fn draining_refuses_new_print_jobs() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        world
            .draining
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "1".to_owned(),
                method: "doc.print".to_owned(),
                params: minimal_document(),
            },
        )
        .await;
        let response = match dispatched {
            Dispatch::Reply(response) => response,
            _ => panic!("a draining service must not reserve a job"),
        };
        assert_eq!(
            response.error.map(|error| error.code),
            Some(ErrorCode::Unsupported)
        );
    }

    #[tokio::test]
    async fn a_removed_origin_cannot_print_on_an_existing_session() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        world
            .revoked_origins
            .lock()
            .unwrap()
            .insert("https://app.example".to_owned());
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "1".to_owned(),
                method: "doc.print".to_owned(),
                params: minimal_document(),
            },
        )
        .await;
        let response = match dispatched {
            Dispatch::Reply(response) => response,
            _ => panic!("a removed origin must not reserve a job"),
        };
        assert_eq!(
            response.error.map(|error| error.code),
            Some(ErrorCode::Origin)
        );
    }

    #[tokio::test]
    async fn a_failed_ws_print_is_not_remembered() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let session = Session {
            connection: 1,
            scopes: vec![Scope::Print],
            origin: None,
        };
        let request = printkit_protocol::Request {
            id: "1".to_owned(),
            method: "doc.print".to_owned(),
            params: serde_json::json!({ "version": 99, "requestId": "dup" }),
        };
        let result = dispatch(&world, &session, request).await;
        assert!(
            matches!(result, Dispatch::Reply(_)),
            "an invalid document must fail before a job starts"
        );
        let key = idempotency_key("ws", "doc.print", None, "dup");
        assert!(
            world.idempotency.lock().unwrap().get(&key).is_none(),
            "a failed print must remain retryable"
        );
    }

    #[tokio::test]
    async fn handshakes_grant_print_without_a_token_and_trace_refusals() {
        let temp = tempfile::tempdir().unwrap();
        let sink = Arc::new(MemorySink::default());
        let world = state(temp.path(), sink.clone(), pool(&FakeRenderHost::new()));
        world.store.add_origin("https://app.example").unwrap();
        world
            .store
            .insert_token(
                "https://app.example",
                &hash_token("pk_live"),
                "print",
                false,
            )
            .unwrap();

        let wrong_origin = Hello {
            r#type: "hello".to_owned(),
            protocol: 1,
            sdk: "probe".to_owned(),
            token: Some("pk_live".to_owned()),
            origin: Some("https://evil.example".to_owned()),
        };
        let first = authenticate(&world, &wrong_origin, Some("https://evil.example"))
            .await
            .unwrap_err();
        assert_eq!(first.code, ErrorCode::Origin);

        // A whitelisted browser and a local script print without a token.
        let browser_without_token = Hello {
            r#type: "hello".to_owned(),
            protocol: 1,
            sdk: "probe".to_owned(),
            token: None,
            origin: Some("https://app.example".to_owned()),
        };
        let second = authenticate(&world, &browser_without_token, Some("https://app.example"))
            .await
            .unwrap();
        assert_eq!(second.scopes, vec![Scope::Print]);
        assert_eq!(second.origin.as_deref(), Some("https://app.example"));

        let local_without_token = Hello {
            r#type: "hello".to_owned(),
            protocol: 1,
            sdk: "probe".to_owned(),
            token: None,
            origin: None,
        };
        let third = authenticate(&world, &local_without_token, None)
            .await
            .unwrap();
        assert_eq!(third.scopes, vec![Scope::Print]);
        assert_eq!(third.origin, None);

        // The refusal path stays covered through the test-only policy.
        let refused = authenticate_with(
            &world,
            &browser_without_token,
            Some("https://app.example"),
            LocalPrintPolicy::Token,
        )
        .await
        .unwrap_err();
        assert_eq!(refused.code, ErrorCode::Auth);

        let wrong_protocol = Hello {
            r#type: "hello".to_owned(),
            protocol: 2,
            sdk: "probe".to_owned(),
            token: Some("pk_live".to_owned()),
            origin: Some("https://app.example".to_owned()),
        };
        let fifth = authenticate(&world, &wrong_protocol, Some("https://app.example"))
            .await
            .unwrap_err();
        assert_eq!(fifth.code, ErrorCode::Protocol);

        // A print-only session never reaches raw.send.
        let raw = reply(
            &world,
            &third,
            Request {
                id: "raw".to_owned(),
                method: "raw.send".to_owned(),
                params: json!({ "printer": "any", "bytes": "" }),
            },
        )
        .await;
        assert!(!raw.ok);
        assert_eq!(
            raw.error.as_ref().map(|error| error.code),
            Some(ErrorCode::Auth)
        );

        let events = sink.events();
        assert_eq!(events.len(), 5);
        for event in &events {
            assert_eq!(event.stage, Stage::Auth);
        }
        assert!(!events[0].ok);
        assert_eq!(events[0].code.as_deref(), Some("E_ORIGIN"));
        assert!(events[1].ok);
        assert!(events[2].ok);
        assert!(!events[3].ok);
        assert_eq!(events[3].code.as_deref(), Some("E_AUTH"));
        assert!(!events[4].ok);
        assert_eq!(events[4].code.as_deref(), Some("E_PROTOCOL"));
        assert!(events[0].origin.is_some());
        assert_eq!(events[1].origin.as_deref(), Some("https://app.example"));
        assert_eq!(events[2].origin, None);
        assert!(events[3].origin.is_some());
        assert!(
            events[0]
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("evil.example")
        );
    }

    #[tokio::test]
    async fn export_pdf_confines_writes_and_skips_rendering_on_escape() {
        let temp = tempfile::tempdir().unwrap();
        let fake = FakeRenderHost::new();
        let world = state(temp.path(), Arc::new(MemorySink::default()), pool(&fake));
        world.store.add_origin("https://app.example").unwrap();
        let document: Value =
            serde_json::from_str(include_str!("../../../../tests/fixtures/minimal.json")).unwrap();

        let escape = reply(
            &world,
            &session(),
            Request {
                id: "escape".to_owned(),
                method: "doc.exportPdf".to_owned(),
                params: json!({ "document": document, "path": "../../escape.pdf" }),
            },
        )
        .await;
        assert!(!escape.ok);
        assert_eq!(
            escape.error.as_ref().map(|error| error.code),
            Some(ErrorCode::Schema)
        );
        assert_eq!(fake.calls(), 0);

        let inside = reply(
            &world,
            &session(),
            Request {
                id: "inside".to_owned(),
                method: "doc.exportPdf".to_owned(),
                params: json!({ "document": document, "path": "unit/out.pdf" }),
            },
        )
        .await;
        assert!(inside.ok);
        assert_eq!(fake.calls(), 1);
        let result = inside.result.unwrap();
        assert_eq!(result["path"], json!("exports/unit/out.pdf"));
        assert!(temp.path().join("exports/unit/out.pdf").is_file());
    }

    #[tokio::test]
    async fn a_print_token_cannot_call_raw_send() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let response = reply(
            &world,
            &session(),
            Request {
                id: "raw".to_owned(),
                method: "raw.send".to_owned(),
                params: json!({ "printer": "any", "bytes": "" }),
            },
        )
        .await;
        assert!(!response.ok);
        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(ErrorCode::Auth)
        );

        let unknown = reply(
            &world,
            &session(),
            Request {
                id: "unknown".to_owned(),
                method: "doc.unsupported".to_owned(),
                params: Value::Null,
            },
        )
        .await;
        assert_eq!(
            unknown.error.as_ref().map(|error| error.code),
            Some(ErrorCode::Unsupported)
        );
    }

    /// A raw-scope token is not enough: the destination itself must accept raw
    /// bytes. Nothing is queued and nothing is submitted, so a refusal cannot
    /// be mistaken for a graphic print of the control codes.
    #[tokio::test]
    async fn raw_send_refuses_a_destination_without_raw_support() {
        use base64::Engine as _;

        let temp = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakePrinterBackend::new());
        let world = state_with_backend(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
            backend.clone(),
        );
        let response = reply(
            &world,
            &raw_session(),
            Request {
                id: "raw".to_owned(),
                method: "raw.send".to_owned(),
                params: json!({
                    "printer": "fake-printer",
                    "bytes": base64::engine::general_purpose::STANDARD.encode(b"\x1b@label"),
                }),
            },
        )
        .await;
        assert!(!response.ok);
        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(ErrorCode::RawUnsupported)
        );
        assert!(backend.raw_submissions().is_empty());
        assert!(backend.submitted().is_empty());
    }

    #[tokio::test]
    async fn raw_bytes_reach_the_backend_unchanged() {
        use base64::Engine as _;

        let temp = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakePrinterBackend::raw_capable());
        let world = state_with_backend(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
            backend.clone(),
        );
        let payload = b"\x1b@\x1dV\x00printkit-raw\xff\xfe\n";
        let response = reply(
            &world,
            &raw_session(),
            Request {
                id: "raw".to_owned(),
                method: "raw.send".to_owned(),
                params: json!({
                    "printer": "fake-printer",
                    "bytes": base64::engine::general_purpose::STANDARD.encode(payload),
                }),
            },
        )
        .await;
        assert!(response.ok);
        let job_id = response.result.unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_owned();
        let job = printkit_facade::JobId::from(job_id.as_str());
        for _ in 0..200 {
            if world.pool.status(&job).map(|status| status.phase) == Some(JobPhase::Done) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            world.pool.status(&job).map(|status| status.phase),
            Some(JobPhase::Done)
        );
        let submitted = backend.raw_submissions();
        assert_eq!(submitted.len(), 1);
        assert_eq!(submitted[0].bytes.as_ref(), payload);
        assert_eq!(submitted[0].printer.as_deref(), Some("fake-printer"));
        let mut records = Vec::new();
        for _ in 0..200 {
            records = world
                .store
                .list_print_records(&printkit_store::RecordFilter {
                    limit: 10,
                    ..printkit_store::RecordFilter::default()
                })
                .unwrap();
            if !records.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].pages, 1);
    }

    #[tokio::test]
    async fn sys_info_reports_the_render_worker_count() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let response = reply(
            &world,
            &session(),
            Request {
                id: "info".to_owned(),
                method: "sys.info".to_owned(),
                params: Value::Null,
            },
        )
        .await;
        assert!(response.ok);
        let result = response.result.unwrap();
        assert_eq!(result["platform"], json!("macos"));
        assert_eq!(result["capabilities"]["renderWorkers"], json!(1));
        let keys: Vec<&str> = result["capabilities"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys.len(), 10, "{keys:?}");
        assert_eq!(result["capabilities"]["duplex"], json!(false));
        assert_eq!(result["capabilities"]["paperSource"], json!(false));
        assert_eq!(result["capabilities"]["preview"], json!(true));
        assert_eq!(result["capabilities"]["rasterFallback"], json!(false));
        assert_eq!(result["capabilities"]["raw"], json!(true));
        assert_eq!(result["capabilities"]["batch"], json!(true));
        assert_eq!(result["capabilities"]["pdfPassthrough"], json!(true));
        assert_eq!(result["capabilities"]["cancelSubmitted"], json!(true));
        assert_eq!(result["capabilities"]["cancelRaw"], json!(true));
    }

    #[tokio::test]
    async fn job_status_rejects_an_unknown_id() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let response = reply(
            &world,
            &session(),
            Request {
                id: "status".to_owned(),
                method: "job.status".to_owned(),
                params: json!({ "jobId": "job-does-not-exist" }),
            },
        )
        .await;
        assert!(!response.ok);
        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(ErrorCode::NotFound)
        );
    }

    #[cfg(feature = "fake-render")]
    #[tokio::test]
    async fn job_status_and_cancel_are_scoped_to_the_reserving_origin() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let document: Value =
            serde_json::from_str(include_str!("../../../../tests/fixtures/minimal.json")).unwrap();
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "print".to_owned(),
                method: "doc.print".to_owned(),
                params: document,
            },
        )
        .await;
        let (response, start) = match dispatched {
            Dispatch::Reply(_) => panic!("doc.print must reserve a job"),
            Dispatch::Subscribe { .. } => panic!("doc.print does not subscribe"),
            Dispatch::Start { response, start } => (response, start),
        };
        assert!(response.ok, "{:?}", response.error);
        let job_id = response.result.as_ref().unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_owned();

        let other = Session {
            connection: 3,
            scopes: vec![Scope::Print],
            origin: Some("https://other.example".to_owned()),
        };
        let status = reply(
            &world,
            &other,
            Request {
                id: "status".to_owned(),
                method: "job.status".to_owned(),
                params: json!({ "jobId": job_id }),
            },
        )
        .await;
        assert_eq!(
            status.error.as_ref().map(|error| error.code),
            Some(ErrorCode::NotFound)
        );
        let cancel = reply(
            &world,
            &other,
            Request {
                id: "cancel".to_owned(),
                method: "job.cancel".to_owned(),
                params: json!({ "jobId": job_id }),
            },
        )
        .await;
        assert_eq!(
            cancel.error.as_ref().map(|error| error.code),
            Some(ErrorCode::NotFound)
        );
        // The owning origin still cancels before the job starts.
        let own_cancel = reply(
            &world,
            &session(),
            Request {
                id: "cancel-own".to_owned(),
                method: "job.cancel".to_owned(),
                params: json!({ "jobId": job_id }),
            },
        )
        .await;
        assert!(own_cancel.ok, "{:?}", own_cancel.error);
        start();
    }

    #[cfg(feature = "fake-render")]
    #[tokio::test]
    async fn a_mock_print_walks_queued_printing_done() {
        let temp = tempfile::tempdir().unwrap();
        let host = FakeRenderHost::new().with_delay(std::time::Duration::from_millis(30));
        let world = state(temp.path(), Arc::new(MemorySink::default()), pool(&host));
        let mut received = world.job_events.subscribe("https://app.example");
        let document: Value =
            serde_json::from_str(include_str!("../../../../tests/fixtures/minimal.json")).unwrap();
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "print".to_owned(),
                method: "doc.print".to_owned(),
                params: document,
            },
        )
        .await;
        let (response, start) = match dispatched {
            Dispatch::Reply(_) => panic!("doc.print must reserve a job"),
            Dispatch::Subscribe { .. } => panic!("doc.print does not subscribe"),
            Dispatch::Start { response, start } => (response, start),
        };
        assert!(response.ok, "{:?}", response.error);
        let job_id = response.result.as_ref().unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(host.calls(), 0, "the job starts only after activation");
        start();

        let mut statuses = Vec::new();
        while let Ok(text) = received.recv().await {
            let event: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(event["type"], json!("event"));
            assert_eq!(event["event"], json!("job"));
            assert_eq!(event["jobId"], json!(job_id));
            let status = event["status"].as_str().unwrap().to_owned();
            statuses.push(status.clone());
            if status == "done" || status == "failed" {
                break;
            }
        }
        assert_eq!(statuses, vec!["queued", "printing", "done"]);
        assert_eq!(host.calls(), 1);
    }

    #[cfg(feature = "fake-render")]
    #[tokio::test]
    async fn a_print_job_reports_estimated_table_warnings_on_done_and_status() {
        let temp = tempfile::tempdir().unwrap();
        // The default test double cannot measure rows, which is exactly the
        // degraded path whose warning must reach the client.
        let host = FakeRenderHost::new();
        let world = state(temp.path(), Arc::new(MemorySink::default()), pool(&host));
        let mut received = world.job_events.subscribe("https://app.example");
        let document = json!({
            "version": 1,
            "pages": [{
                "spec": {"size": "A4"},
                "items": [{
                    "id": "t",
                    "type": "table",
                    "frame": {"x": 20.0, "y": 20.0, "width": 170.0, "height": 200.0},
                    "data": {
                        "columns": [{"key": "a", "title": "A", "width": "auto"}],
                        "rows": [{"a": "x"}]
                    }
                }]
            }]
        });
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "print".to_owned(),
                method: "doc.print".to_owned(),
                params: document,
            },
        )
        .await;
        let (response, start) = match dispatched {
            Dispatch::Start { response, start } => (response, start),
            Dispatch::Reply(_) | Dispatch::Subscribe { .. } => {
                panic!("doc.print must reserve a job")
            }
        };
        assert!(response.ok, "{:?}", response.error);
        let job_id = response.result.as_ref().unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_owned();
        start();

        let mut done_warnings = None;
        while let Ok(text) = received.recv().await {
            let event: Value = serde_json::from_str(&text).unwrap();
            if event["status"] == json!("done") {
                done_warnings = Some(event["warnings"].clone());
                break;
            }
        }
        let done_warnings = done_warnings.expect("the job never reported done");
        assert!(
            done_warnings
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning["code"] == json!("W_TABLE_HEIGHTS_ESTIMATED")),
            "{done_warnings}"
        );

        let status = reply(
            &world,
            &session(),
            Request {
                id: "status".to_owned(),
                method: "job.status".to_owned(),
                params: json!({ "jobId": job_id }),
            },
        )
        .await;
        assert!(status.ok, "{:?}", status.error);
        let status = status.result.unwrap();
        assert!(
            status["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning["code"] == json!("W_TABLE_HEIGHTS_ESTIMATED")),
            "{status}"
        );
    }

    #[cfg(feature = "fake-render")]
    #[tokio::test]
    async fn a_job_cancelled_while_queued_never_reaches_the_spooler() {
        let temp = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakePrinterBackend::new());
        let host = FakeRenderHost::new();
        let sink = Arc::new(MemorySink::default());
        let world = state_with_backend(temp.path(), sink.clone(), pool(&host), backend.clone());
        let mut received = world.job_events.subscribe("https://app.example");
        let document: Value =
            serde_json::from_str(include_str!("../../../../tests/fixtures/minimal.json")).unwrap();
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "print".to_owned(),
                method: "doc.print".to_owned(),
                params: document,
            },
        )
        .await;
        let (response, start) = match dispatched {
            Dispatch::Reply(_) => panic!("doc.print must reserve a job"),
            Dispatch::Subscribe { .. } => panic!("doc.print does not subscribe"),
            Dispatch::Start { response, start } => (response, start),
        };
        assert!(response.ok, "{:?}", response.error);
        let job_id = response.result.as_ref().unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_owned();

        let cancelled = reply(
            &world,
            &session(),
            Request {
                id: "cancel".to_owned(),
                method: "job.cancel".to_owned(),
                params: json!({ "jobId": job_id }),
            },
        )
        .await;
        assert!(cancelled.ok, "{:?}", cancelled.error);
        assert_eq!(cancelled.result, Some(json!({})));
        start();

        let text = received.recv().await.unwrap();
        let event: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(event["jobId"], json!(job_id));
        assert_eq!(event["status"], json!("cancelled"));
        assert_eq!(host.calls(), 0, "a cancelled job must not render");
        assert!(
            backend.submitted().is_empty(),
            "a cancelled job must not reach the spooler"
        );
        let status = reply(
            &world,
            &session(),
            Request {
                id: "status".to_owned(),
                method: "job.status".to_owned(),
                params: json!({ "jobId": job_id }),
            },
        )
        .await;
        assert_eq!(status.result.unwrap()["status"], json!("cancelled"));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && !sink
                .events()
                .iter()
                .any(|event| event.stage == Stage::Result)
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let result = sink
            .events()
            .into_iter()
            .find(|event| event.stage == Stage::Result)
            .expect("a queued cancellation must still close the trace");
        assert_eq!(result.detail.as_deref(), Some("cancelled"));
    }

    #[cfg(feature = "fake-render")]
    #[tokio::test]
    async fn a_pdf_job_cancelled_while_queued_emits_one_cancelled_event() {
        use base64::Engine as _;
        use printkit_facade::RenderSpec;

        let temp = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakePrinterBackend::new());
        let sink = Arc::new(MemorySink::default());
        let world = state_with_backend(
            temp.path(),
            sink.clone(),
            pool(&FakeRenderHost::new()),
            backend.clone(),
        );
        let mut received = world.job_events.subscribe("https://app.example");
        let source = FakeRenderHost::new();
        let pdf = futures::executor::block_on(RenderHost::render_to_pdf(
            &source,
            "",
            &RenderSpec::new(210.0, 297.0),
        ))
        .unwrap();
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "pdf".to_owned(),
                method: "doc.printPdf".to_owned(),
                params: json!({
                    "bytes": base64::engine::general_purpose::STANDARD.encode(&pdf),
                }),
            },
        )
        .await;
        let (response, start) = match dispatched {
            Dispatch::Reply(_) => panic!("doc.printPdf must reserve a job"),
            Dispatch::Subscribe { .. } => panic!("doc.printPdf does not subscribe"),
            Dispatch::Start { response, start } => (response, start),
        };
        assert!(response.ok, "{:?}", response.error);
        let job_id = response.result.as_ref().unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_owned();

        let cancelled = reply(
            &world,
            &session(),
            Request {
                id: "cancel".to_owned(),
                method: "job.cancel".to_owned(),
                params: json!({ "jobId": job_id }),
            },
        )
        .await;
        assert!(cancelled.ok, "{:?}", cancelled.error);
        assert_eq!(cancelled.result, Some(json!({})));
        start();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && !sink
                .events()
                .iter()
                .any(|event| event.stage == Stage::Result)
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let result = sink
            .events()
            .into_iter()
            .find(|event| event.stage == Stage::Result)
            .expect("the cancelled job never finished");
        assert_eq!(result.detail.as_deref(), Some("cancelled"));
        assert!(
            backend.submitted().is_empty(),
            "a cancelled job must not reach the spooler"
        );

        let text = received.recv().await.unwrap();
        let event: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(event["status"], json!("cancelled"));
        assert!(received.try_recv().is_err(), "the cancel event repeated");
    }

    #[cfg(feature = "fake-render")]
    #[tokio::test]
    async fn cancelling_a_submitted_job_retracts_it_from_the_spooler() {
        let temp = tempfile::tempdir().unwrap();
        let backend = Arc::new(FakePrinterBackend::new());
        let world = state_with_backend(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
            backend.clone(),
        );
        let mut received = world.job_events.subscribe("https://app.example");
        let document: Value =
            serde_json::from_str(include_str!("../../../../tests/fixtures/minimal.json")).unwrap();
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "print".to_owned(),
                method: "doc.print".to_owned(),
                params: document,
            },
        )
        .await;
        let (response, start) = match dispatched {
            Dispatch::Reply(_) => panic!("doc.print must reserve a job"),
            Dispatch::Subscribe { .. } => panic!("doc.print does not subscribe"),
            Dispatch::Start { response, start } => (response, start),
        };
        assert!(response.ok, "{:?}", response.error);
        let job_id = response.result.as_ref().unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_owned();
        start();
        let finished = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Ok(text) = received.recv().await {
                let event: Value = serde_json::from_str(&text).unwrap();
                if event["status"] == json!("done") {
                    break;
                }
            }
        })
        .await;
        assert!(finished.is_ok(), "the job never finished");

        let cancelled = reply(
            &world,
            &session(),
            Request {
                id: "cancel".to_owned(),
                method: "job.cancel".to_owned(),
                params: json!({ "jobId": job_id }),
            },
        )
        .await;
        assert!(cancelled.ok, "{:?}", cancelled.error);
        assert_eq!(cancelled.result, Some(json!({})));
        let retracted = backend.cancelled();
        assert_eq!(retracted.len(), 1, "the spooler job was not retracted");
        assert_eq!(retracted[0].job_id, Some(1));
    }

    #[cfg(feature = "fake-render")]
    #[tokio::test]
    async fn a_batch_trace_reports_one_render_stage_with_totals() {
        use printkit_facade::{FieldKey, Scalar};

        let temp = tempfile::tempdir().unwrap();
        let sink = Arc::new(MemorySink::default());
        let world = state(temp.path(), sink.clone(), pool(&FakeRenderHost::new()));
        let document: Value =
            serde_json::from_str(include_str!("../../../../tests/fixtures/minimal.json")).unwrap();
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "batch".to_owned(),
                method: "doc.printBatch".to_owned(),
                params: json!({ "docs": [document.clone(), document] }),
            },
        )
        .await;
        let (response, start) = match dispatched {
            Dispatch::Reply(_) => panic!("doc.printBatch must reserve a job"),
            Dispatch::Subscribe { .. } => panic!("doc.printBatch does not subscribe"),
            Dispatch::Start { response, start } => (response, start),
        };
        assert!(response.ok, "{:?}", response.error);
        start();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && !sink
                .events()
                .iter()
                .any(|event| event.stage == Stage::Result)
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let recorded = sink.events();
        let renders: Vec<_> = recorded
            .iter()
            .filter(|event| event.stage == Stage::Render)
            .collect();
        assert_eq!(renders.len(), 1, "{recorded:?}");
        assert_eq!(renders[0].pages, Some(2));
        assert!(matches!(
            renders[0].fields.get(&FieldKey::HtmlMs),
            Some(Scalar::Int(_))
        ));
        assert!(matches!(
            renders[0].fields.get(&FieldKey::PdfMs),
            Some(Scalar::Int(_))
        ));
        assert!(recorded.iter().any(|event| event.stage == Stage::Result));
    }

    #[tokio::test]
    async fn out_of_range_copies_and_empty_batches_are_schema_errors() {
        use base64::Engine as _;

        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let document: Value =
            serde_json::from_str(include_str!("../../../../tests/fixtures/minimal.json")).unwrap();
        for (method, params) in [
            (
                "doc.printPdf",
                json!({
                    "bytes": base64::engine::general_purpose::STANDARD.encode("%PDF-1.4"),
                    "copies": 0,
                }),
            ),
            (
                "doc.printBatch",
                json!({ "docs": [document.clone()], "copies": 0 }),
            ),
            ("doc.printBatch", json!({ "docs": [] })),
        ] {
            let response = reply(
                &world,
                &session(),
                Request {
                    id: "reject".to_owned(),
                    method: method.to_owned(),
                    params,
                },
            )
            .await;
            assert!(!response.ok, "{method} accepted the parameters");
            assert_eq!(
                response.error.as_ref().map(|error| error.code),
                Some(ErrorCode::Schema),
                "{method}"
            );
        }
    }
    #[tokio::test]
    async fn preview_relays_render_warnings() {
        let temp = tempfile::tempdir().unwrap();
        let host = FakeRenderHost::new();
        let world = state_with_host(temp.path(), Arc::new(MemorySink::default()), &host);
        let mut document = minimal_document();
        document["pages"][0]["items"][0]["style"] = json!({ "fontFamily": "No Such Face" });
        let opened = reply(
            &world,
            &session(),
            preview_request("open", "doc.preview", document),
        )
        .await;
        assert!(opened.ok, "{:?}", opened.error);
        assert_eq!(opened.warnings.len(), 1, "{:?}", opened.warnings);
        assert_eq!(opened.warnings[0].code.as_str(), "W_FONT_UNKNOWN");
    }

    #[tokio::test]
    async fn a_refused_preview_window_fails_its_result_stage() {
        let temp = tempfile::tempdir().unwrap();
        let host = FakeRenderHost::new().without_preview();
        let sink = Arc::new(MemorySink::default());
        let world = state_with_host(temp.path(), sink.clone(), &host);
        let denied = reply(
            &world,
            &session(),
            preview_request("open", "doc.preview", minimal_document()),
        )
        .await;
        assert_eq!(
            denied.error.as_ref().map(|error| error.code),
            Some(ErrorCode::Unsupported)
        );
        let stages: Vec<_> = sink.events().iter().map(|event| event.stage).collect();
        assert_eq!(
            stages,
            vec![
                Stage::Recv,
                Stage::Auth,
                Stage::Validate,
                Stage::Render,
                Stage::Result,
            ]
        );
        let result = sink.events().pop().unwrap();
        assert!(!result.ok, "a refused window reported a successful result");
        assert_eq!(result.code.as_deref(), Some("E_UNSUPPORTED"));
    }

    #[tokio::test]
    async fn preview_windows_replace_export_and_close() {
        use base64::Engine as _;

        let temp = tempfile::tempdir().unwrap();
        let host = FakeRenderHost::new();
        let world = state_with_host(temp.path(), Arc::new(MemorySink::default()), &host);
        let session = session();
        let opened = reply(
            &world,
            &session,
            preview_request("p1", "doc.preview", minimal_document()),
        )
        .await;
        assert!(opened.ok, "{:?}", opened.error);
        let window = opened.result.as_ref().unwrap()["windowId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(host.preview_windows(), 1);

        let replaced = reply(
            &world,
            &session,
            preview_request("p2", "doc.preview", minimal_document()),
        )
        .await;
        assert!(replaced.ok, "{:?}", replaced.error);
        assert_eq!(
            replaced.result.as_ref().unwrap()["windowId"],
            json!(window),
            "the same session must refresh its own window"
        );
        assert_eq!(host.preview_windows(), 1);

        let png = reply(
            &world,
            &session,
            preview_request(
                "p3",
                "preview.pages",
                json!({ "windowId": window, "pageIndex": 0, "dpi": 150 }),
            ),
        )
        .await;
        assert!(png.ok, "{:?}", png.error);
        let encoded = png.result.as_ref().unwrap()["png"].as_str().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));

        let intruder = Session {
            connection: 77,
            scopes: vec![Scope::Print],
            origin: Some("https://other.example".to_owned()),
        };
        for (id, method) in [("p3b", "preview.pages"), ("p3c", "preview.close")] {
            let params = if method == "preview.pages" {
                json!({ "windowId": window, "pageIndex": 0, "dpi": 150 })
            } else {
                json!({ "windowId": window })
            };
            let stolen = reply(&world, &intruder, preview_request(id, method, params)).await;
            assert_eq!(
                stolen.error.as_ref().map(|error| error.code),
                Some(ErrorCode::NotFound),
                "{method} accepted another session's window"
            );
        }
        assert_eq!(host.preview_windows(), 1);

        let outside = reply(
            &world,
            &session,
            preview_request(
                "p4",
                "preview.pages",
                json!({ "windowId": window, "pageIndex": 1, "dpi": 150 }),
            ),
        )
        .await;
        assert_eq!(
            outside.error.as_ref().map(|error| error.code),
            Some(ErrorCode::Schema)
        );

        let closed = reply(
            &world,
            &session,
            preview_request("p5", "preview.close", json!({ "windowId": window })),
        )
        .await;
        assert!(closed.ok, "{:?}", closed.error);
        assert_eq!(host.preview_windows(), 0);

        for (id, method) in [("p6", "preview.pages"), ("p7", "preview.close")] {
            let params = if method == "preview.pages" {
                json!({ "windowId": window, "pageIndex": 0, "dpi": 150 })
            } else {
                json!({ "windowId": window })
            };
            let gone = reply(&world, &session, preview_request(id, method, params)).await;
            assert_eq!(
                gone.error.as_ref().map(|error| error.code),
                Some(ErrorCode::NotFound),
                "{method} accepted a closed window"
            );
        }
    }

    #[tokio::test]
    async fn ninth_preview_window_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let host = FakeRenderHost::new();
        let sink = Arc::new(MemorySink::default());
        let world = state_with_host(temp.path(), sink.clone(), &host);
        for index in 0..8_u64 {
            let owner = Session {
                connection: 100 + index,
                scopes: vec![Scope::Print],
                origin: Some("https://app.example".to_owned()),
            };
            let opened = reply(
                &world,
                &owner,
                preview_request("open", "doc.preview", minimal_document()),
            )
            .await;
            assert!(opened.ok, "{:?}", opened.error);
        }
        assert_eq!(host.preview_windows(), 8);

        let ninth = Session {
            connection: 999,
            scopes: vec![Scope::Print],
            origin: Some("https://app.example".to_owned()),
        };
        let rejected = reply(
            &world,
            &ninth,
            preview_request("open", "doc.preview", minimal_document()),
        )
        .await;
        assert_eq!(
            rejected.error.as_ref().map(|error| error.code),
            Some(ErrorCode::TooLarge)
        );
        assert_eq!(host.preview_windows(), 8);
        let events = sink.events();
        let refused = &events[events.len() - 2..];
        assert_eq!(refused[0].stage, Stage::Recv);
        assert_eq!(refused[1].stage, Stage::Result);
        assert!(!refused[1].ok, "the refused preview logged no failure");
        assert_eq!(refused[1].code.as_deref(), Some("E_TOO_LARGE"));
    }

    #[tokio::test]
    async fn preview_print_control_reserves_a_print_job() {
        let temp = tempfile::tempdir().unwrap();
        let host = FakeRenderHost::new();
        let world = state_with_host(temp.path(), Arc::new(MemorySink::default()), &host);
        let session = session();
        let opened = reply(
            &world,
            &session,
            preview_request("open", "doc.preview", minimal_document()),
        )
        .await;
        assert!(opened.ok, "{:?}", opened.error);
        let window = opened.result.as_ref().unwrap()["windowId"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap();

        let mut events = world.job_events.subscribe("https://app.example");
        assert!(host.press_preview_print(printkit_platform::WindowId::new(window)));
        let mut finished = false;
        for _ in 0..32 {
            let received = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("the print control produced no job event")
                .expect("the job channel closed");
            let value: Value = serde_json::from_str(&received).unwrap();
            if value["event"] == json!("job") && value["status"] == json!("done") {
                finished = true;
                break;
            }
        }
        assert!(finished, "the job never reported done");
        assert!(host.calls() >= 1, "the print control skipped rendering");
    }

    #[tokio::test]
    async fn preview_without_a_gui_session_is_unsupported() {
        let temp = tempfile::tempdir().unwrap();
        let host = FakeRenderHost::new().without_preview();
        let world = state_with_host(temp.path(), Arc::new(MemorySink::default()), &host);
        let info = reply(
            &world,
            &session(),
            Request {
                id: "info".to_owned(),
                method: "sys.info".to_owned(),
                params: Value::Null,
            },
        )
        .await;
        assert!(info.ok, "{:?}", info.error);
        assert_eq!(
            info.result.unwrap()["capabilities"]["preview"],
            json!(false),
            "a headless host must not advertise preview"
        );

        let denied = reply(
            &world,
            &session(),
            preview_request("open", "doc.preview", minimal_document()),
        )
        .await;
        assert_eq!(
            denied.error.as_ref().map(|error| error.code),
            Some(ErrorCode::Unsupported)
        );
    }

    struct SlowSink {
        inner: MemorySink,
        delay: std::time::Duration,
    }

    impl Sink for SlowSink {
        fn emit(&self, event: &TraceEvent) {
            std::thread::sleep(self.delay);
            self.inner.emit(event);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_preparation_does_not_block_sys_info() {
        let temp = tempfile::tempdir().unwrap();
        let sink: Arc<dyn Sink> = Arc::new(SlowSink {
            inner: MemorySink::default(),
            delay: std::time::Duration::from_millis(150),
        });
        let world = state(temp.path(), sink, pool(&FakeRenderHost::new()));
        let document = minimal_document();
        let mut prints = Vec::new();
        for index in 0..8 {
            let world = Arc::clone(&world);
            let document = document.clone();
            prints.push(tokio::spawn(async move {
                dispatch(
                    &world,
                    &session(),
                    Request {
                        id: format!("print-{index}"),
                        method: "doc.print".to_owned(),
                        params: document,
                    },
                )
                .await
            }));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let started = std::time::Instant::now();
        let info = reply(
            &world,
            &session(),
            Request {
                id: "info".to_owned(),
                method: "sys.info".to_owned(),
                params: Value::Null,
            },
        )
        .await;
        assert!(info.ok, "{:?}", info.error);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "sys.info waited {:?}",
            started.elapsed()
        );
        for print in prints {
            let _ = print.await;
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_url_fetch_does_not_block_sys_info() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for _ in 0..8 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buffer = [0u8; 1024];
                let _ = socket.read(&mut buffer);
                std::thread::sleep(std::time::Duration::from_millis(100));
                let body = "<html><body>slow fixture</body></html>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes());
            }
        });

        let temp = tempfile::tempdir().unwrap();
        let world = state_with_private_fetch(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let source = include_str!("../../../../tests/fixtures/url.json")
            .replace("http://127.0.0.1:8899", &format!("http://127.0.0.1:{port}"));
        let document: Value = serde_json::from_str(&source).unwrap();
        let mut prints = Vec::new();
        for index in 0..8 {
            let world = Arc::clone(&world);
            let document = document.clone();
            prints.push(tokio::spawn(async move {
                dispatch(
                    &world,
                    &session(),
                    Request {
                        id: format!("slow-{index}"),
                        method: "doc.print".to_owned(),
                        params: document,
                    },
                )
                .await
            }));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let started = std::time::Instant::now();
        let info = reply(
            &world,
            &session(),
            Request {
                id: "info".to_owned(),
                method: "sys.info".to_owned(),
                params: Value::Null,
            },
        )
        .await;
        assert!(info.ok, "{:?}", info.error);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "sys.info waited {:?}",
            started.elapsed()
        );
        for print in prints {
            let _ = print.await;
        }
        let _ = server.join();
    }

    /// Mirror the WebSocket loop: lock the key, dispatch, then remember the
    /// job as if the response had been written and `start()` had run.
    async fn deliver_request(world: &Arc<AppState>, request: Request) -> Dispatch {
        let session = session();
        let key = super::ws::request_idempotency_key(&session, &request.method, &request.params)
            .ok()
            .flatten();
        let lock = key.as_ref().map(|key| {
            let mut locks = world
                .idempotency_locks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            prune_idempotency_locks(&mut locks);
            Arc::clone(
                locks
                    .entry(key.clone())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
            )
        });
        let _guard = match lock {
            Some(lock) => Some(lock.lock_owned().await),
            None => None,
        };
        let dispatched = dispatch(world, &session, request).await;
        if let Dispatch::Start { response, .. } = &dispatched {
            super::ws::remember_idempotent(&world.idempotency, key, response);
        }
        dispatched
    }

    #[tokio::test]
    async fn duplicate_request_id_returns_the_same_job() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let mut document = minimal_document();
        document["requestId"] = json!("same-key");
        let first = deliver_request(
            &world,
            Request {
                id: "one".to_owned(),
                method: "doc.print".to_owned(),
                params: document.clone(),
            },
        )
        .await;
        let first_id = match first {
            Dispatch::Start { response, .. } => response.result.unwrap()["jobId"]
                .as_str()
                .unwrap()
                .to_owned(),
            _ => panic!("the first print did not reserve a job"),
        };
        let second = deliver_request(
            &world,
            Request {
                id: "two".to_owned(),
                method: "doc.print".to_owned(),
                params: document,
            },
        )
        .await;
        let second_id = match second {
            Dispatch::Reply(response) => response.result.unwrap()["jobId"]
                .as_str()
                .unwrap()
                .to_owned(),
            _ => panic!("the duplicate did not reuse the job"),
        };
        assert_eq!(first_id, second_id);
    }

    #[tokio::test]
    async fn concurrent_same_request_id_reserves_once() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let mut document = minimal_document();
        document["requestId"] = json!("race-key");
        let call = |id: &str| {
            let world = Arc::clone(&world);
            let document = document.clone();
            let id = id.to_owned();
            async move {
                deliver_request(
                    &world,
                    Request {
                        id,
                        method: "doc.print".to_owned(),
                        params: document,
                    },
                )
                .await
            }
        };
        let (first, second) = tokio::join!(call("a"), call("b"));
        let job_id = |dispatch: Dispatch| match dispatch {
            Dispatch::Start { response, .. } | Dispatch::Reply(response) => {
                response.result.unwrap()["jobId"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            }
            _ => panic!("print did not return a job id"),
        };
        assert_eq!(job_id(first), job_id(second));
    }

    #[tokio::test]
    async fn doc_print_status_reports_pages() {
        let temp = tempfile::tempdir().unwrap();
        let world = state(
            temp.path(),
            Arc::new(MemorySink::default()),
            pool(&FakeRenderHost::new()),
        );
        let dispatched = dispatch(
            &world,
            &session(),
            Request {
                id: "print".to_owned(),
                method: "doc.print".to_owned(),
                params: minimal_document(),
            },
        )
        .await;
        let Dispatch::Start { response, .. } = dispatched else {
            panic!("doc.print must reserve a job");
        };
        let job_id = response.result.unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_owned();
        let status = reply(
            &world,
            &session(),
            Request {
                id: "status".to_owned(),
                method: "job.status".to_owned(),
                params: json!({ "jobId": job_id }),
            },
        )
        .await;
        assert_eq!(status.result.unwrap()["pages"].as_u64(), Some(1));
    }
}
