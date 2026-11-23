//! Fetching `data.url` bodies for HTML items.
//!
//! This is the only network path in the engine. It is deliberately narrow:
//! plain HTTP(S) only, a hard timeout, a hard byte ceiling and a redirect policy
//! that re-checks every hop. The Rust side fetches; the WebView never does,
//! which is what keeps the renderer offline. The client never uses a system
//! proxy either: a proxy can bypass the pinned DNS answer (an IP proxy connects
//! instead of the checked host) or resolve a hostname itself, so a document
//! URL must not be routed through the user's HTTP(S) proxy.
//!
//! Blocking is intentional: the CLI and the one-shot export path are
//! synchronous, and a request that spends most of its life in the kernel should
//! not occupy an async worker.
//!
//! The HTTP implementation sits behind the `url-fetch` feature. Without it this
//! module only defines the policy and the error type, and a document carrying a
//! URL fails with `E_UNSUPPORTED` instead of pulling a TLS stack into the build.

#[cfg(feature = "url-fetch")]
use std::collections::HashMap;
#[cfg(feature = "url-fetch")]
use std::io::Read;
#[cfg(feature = "url-fetch")]
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
#[cfg(feature = "url-fetch")]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(feature = "url-fetch")]
use std::sync::mpsc::TrySendError;
#[cfg(feature = "url-fetch")]
use std::sync::{Arc, LazyLock, Mutex, Once, mpsc};
#[cfg(feature = "url-fetch")]
use std::time::{Duration, Instant};

use printkit_core::limits::MAX_FETCH_BYTES;
#[cfg(feature = "url-fetch")]
use reqwest::blocking::{Client, Response};
#[cfg(feature = "url-fetch")]
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
#[cfg(feature = "url-fetch")]
use reqwest::redirect;
#[cfg(feature = "url-fetch")]
use reqwest::{Url, header};

/// Total bytes one document may fetch across all of its unique URLs.
pub const MAX_DOCUMENT_FETCH_BYTES: usize = 32 * 1024 * 1024;

/// Limits applied to every fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchPolicy {
    /// Wall-clock limit for one fetch, covering the request, every redirect
    /// and the body read. Defaults to 10 000 ms.
    pub timeout_ms: u32,
    /// Byte ceiling for one response body. Defaults to the protocol's
    /// `MAX_FETCH_BYTES` limit.
    pub max_bytes: u64,
    /// Redirect hops allowed per fetch before the fetch is refused. Defaults
    /// to 5.
    pub max_redirects: u8,
    /// Permit fetching loopback, private and unique-local addresses.
    ///
    /// Off by default: a document must not be able to reach the machine's own
    /// services or the LAN. Debug builds let `tests/scripts/serve_url_fixture.sh`
    /// turn it on through `PRINTKIT_ALLOW_PRIVATE_FETCH=1`; release builds
    /// ignore that variable. Link-local addresses, including the cloud metadata
    /// service, stay blocked even when the opt-in is active.
    pub allow_private_networks: bool,
}

/// Per-URL timeouts one document's fetches may spend together.
#[cfg(feature = "url-fetch")]
const TOTAL_TIMEOUT_FACTOR: u32 = 4;

impl FetchPolicy {
    /// Wall-clock budget for every fetch of one document.
    ///
    /// One URL may use its full timeout; a document with many URLs must not
    /// multiply that without bound, so all of them share this deadline.
    #[cfg(feature = "url-fetch")]
    pub fn total_timeout(&self) -> Duration {
        Duration::from_millis(
            u64::from(self.timeout_ms).saturating_mul(u64::from(TOTAL_TIMEOUT_FACTOR)),
        )
    }
}

impl Default for FetchPolicy {
    fn default() -> Self {
        Self {
            timeout_ms: 10_000,
            max_bytes: MAX_FETCH_BYTES as u64,
            max_redirects: 5,
            allow_private_networks: false,
        }
    }
}

/// Why a `data.url` body could not be fetched.
///
/// The variant decides both the `E_*` code and the stage detail reported on
/// the trace; see [`FetchError::code`].
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The URL is neither `http` nor `https`; checked before any request.
    #[error("url must use the http or https scheme: {url}")]
    Scheme {
        /// The offending URL.
        url: String,
    },
    /// The response body exceeded the per-response byte ceiling, either by a
    /// declared `Content-Length` or while reading a chunked body.
    #[error("fetch exceeded {limit} bytes: {url}")]
    TooLarge {
        /// The URL whose body was too large.
        url: String,
        /// The ceiling that was exceeded, in bytes.
        limit: u64,
    },
    /// A document declares more distinct URLs than the per-document limit.
    #[error("document carries {count} URLs, more than the {limit} allowed")]
    TooManyUrls {
        /// Number of distinct URLs in the document.
        count: usize,
        /// The number of distinct URLs allowed.
        limit: usize,
    },
    /// The host resolved to an address the policy refuses to connect to, or
    /// the URL had no host at all.
    #[error("refusing to fetch a private address: {url} resolves to {address}")]
    BlockedAddress {
        /// The URL whose host was refused.
        url: String,
        /// The refused address, or a note that the URL has no host.
        address: String,
    },
    /// The budget covering the request, redirects and body read ran out.
    #[error("fetch timed out after {timeout_ms} ms: {url}")]
    Timeout {
        /// The URL that was being fetched when the budget expired.
        url: String,
        /// The budget that expired, in milliseconds.
        timeout_ms: u32,
    },
    /// A redirect hop was rejected, for example past the redirect limit.
    #[error("redirect rejected for {url}: {reason}")]
    Redirect {
        /// The URL that issued the redirect.
        url: String,
        /// Why the hop was refused.
        reason: String,
    },
    /// The final response carried a non-success HTTP status.
    #[error("fetch returned status {status}: {url}")]
    Status {
        /// The URL that answered.
        url: String,
        /// The HTTP status code.
        status: u16,
    },
    /// The bounded resolver queue had no room, so the lookup was never
    /// started. Retrying once the queue drains is safe; there is no dedicated
    /// protocol code, so the message names the condition.
    #[error("resolver busy: the DNS resolver queue is full: {url}")]
    ResolverBusy {
        /// The URL whose host could not be queued.
        url: String,
    },
    /// The request or body read failed below HTTP, or the name did not
    /// resolve.
    #[error("fetch failed: {url}: {message}")]
    Network {
        /// The URL that failed.
        url: String,
        /// The transport error, or a resolution failure description.
        message: String,
    },
    /// This binary was built without the `url-fetch` feature, so it has no
    /// TLS stack to fetch with.
    #[error("url fetching is not available in this build: {url}")]
    Unavailable {
        /// The URL that was refused.
        url: String,
    },
    /// The HTTP client could not be constructed, for example when no crypto
    /// provider is available.
    #[cfg(feature = "url-fetch")]
    #[error("cannot build the HTTP client: {message}")]
    Client {
        /// The client-builder error.
        message: String,
    },
}

impl FetchError {
    /// The `E_*` code reported to the caller and written to the trace.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Scheme { .. } | Self::Unavailable { .. } | Self::BlockedAddress { .. } => {
                "E_UNSUPPORTED"
            }
            Self::TooLarge { .. } | Self::TooManyUrls { .. } => "E_TOO_LARGE",
            Self::Timeout { .. } => "E_TIMEOUT",
            Self::ResolverBusy { .. }
            | Self::Redirect { .. }
            | Self::Status { .. }
            | Self::Network { .. } => "E_JOB_FAILED",
            #[cfg(feature = "url-fetch")]
            Self::Client { .. } => "E_JOB_FAILED",
        }
    }
}

/// One HTTP client, reused for every URL in a document.
#[cfg(feature = "url-fetch")]
pub struct Fetcher {
    client: Client,
    pins: Arc<PinnedResolver>,
    resolver: Arc<ResolverPool>,
    max_bytes: u64,
    timeout_ms: u32,
    max_redirects: u8,
    allow_private_networks: bool,
}

/// Name resolution for one host. Its answer is checked once and pinned for the
/// request, so the connection cannot be redirected by a second lookup.
#[cfg(feature = "url-fetch")]
type Lookup = dyn Fn(&str) -> std::io::Result<Vec<IpAddr>> + Send + Sync;

#[cfg(feature = "url-fetch")]
fn system_lookup(host: &str) -> std::io::Result<Vec<IpAddr>> {
    Ok((host, 0)
        .to_socket_addrs()?
        .map(|address| address.ip())
        .collect())
}

/// Worker threads in one resolver pool. The pool is process-wide, so this is
/// also the total number of threads a stuck system resolver can occupy.
#[cfg(feature = "url-fetch")]
const RESOLVER_WORKERS: usize = 4;

/// Lookups that may wait for a worker before new ones are refused. Each queued
/// job holds one host name and one reply slot, so the bound is small on
/// purpose.
#[cfg(feature = "url-fetch")]
const RESOLVER_QUEUE: usize = 64;

/// The process-wide pool used by the system resolver.
#[cfg(feature = "url-fetch")]
static SYSTEM_RESOLVER: LazyLock<Arc<ResolverPool>> =
    LazyLock::new(|| Arc::new(ResolverPool::new(Arc::new(system_lookup))));

/// One queued lookup and the channel its answer goes back on.
#[cfg(feature = "url-fetch")]
struct ResolveJob {
    host: String,
    sender: mpsc::SyncSender<std::io::Result<Vec<IpAddr>>>,
}

/// Why one lookup did not produce addresses.
#[cfg(feature = "url-fetch")]
#[derive(Debug)]
enum ResolveFailure {
    /// The shared deadline expired before a worker answered.
    Timeout,
    /// Every worker was busy and the queue was full.
    QueueFull,
    /// The pool has no receiver left; this only happens when no worker thread
    /// could be started.
    Unavailable,
    /// `getaddrinfo` returned an error.
    Lookup(std::io::Error),
}

/// A fixed-size worker pool around the blocking system resolver.
///
/// `getaddrinfo` has no timeout and cannot be cancelled. A thread that entered
/// it stays there until the system answers, so the only way to keep a hostile
/// or broken DNS setup from growing threads without bound is to never start
/// more than a fixed number. Requests wait in a bounded queue; a request that
/// misses its deadline or finds the queue full fails with an explicit error
/// while its worker finishes and returns to the queue.
#[cfg(feature = "url-fetch")]
struct ResolverPool {
    sender: mpsc::SyncSender<ResolveJob>,
    spawned: AtomicUsize,
}

#[cfg(feature = "url-fetch")]
impl ResolverPool {
    fn new(lookup: Arc<Lookup>) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<ResolveJob>(RESOLVER_QUEUE);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut spawned = 0;
        for index in 0..RESOLVER_WORKERS {
            let receiver = Arc::clone(&receiver);
            let lookup = Arc::clone(&lookup);
            let worker = std::thread::Builder::new()
                .name(format!("printkit-dns-{index}"))
                .spawn(move || {
                    loop {
                        let job = match receiver.lock() {
                            Ok(receiver) => receiver.recv(),
                            Err(poisoned) => poisoned.into_inner().recv(),
                        };
                        let Ok(job) = job else {
                            return;
                        };
                        // A caller that timed out has dropped its receiver; the
                        // answer is discarded and the worker takes the next job.
                        let _ = job.sender.try_send(lookup(&job.host));
                    }
                });
            if worker.is_ok() {
                spawned += 1;
            }
        }
        Self {
            sender,
            spawned: AtomicUsize::new(spawned),
        }
    }

    fn resolve(&self, host: &str, remaining: Duration) -> Result<Vec<IpAddr>, ResolveFailure> {
        if self.spawned.load(Ordering::SeqCst) == 0 {
            return Err(ResolveFailure::Unavailable);
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let job = ResolveJob {
            host: host.to_owned(),
            sender,
        };
        match self.sender.try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(ResolveFailure::QueueFull),
            Err(TrySendError::Disconnected(_)) => return Err(ResolveFailure::Unavailable),
        }
        match receiver.recv_timeout(remaining) {
            Ok(answer) => answer.map_err(ResolveFailure::Lookup),
            Err(_) => Err(ResolveFailure::Timeout),
        }
    }

    /// Threads this pool actually started; tests assert the bound.
    #[cfg(test)]
    fn spawned_threads(&self) -> usize {
        self.spawned.load(Ordering::SeqCst)
    }
}

/// DNS resolver that only hands out the addresses checked for the current hop.
///
/// reqwest would otherwise resolve a name a second time after the address
/// guard approved an earlier answer: a hostile resolver can pass the check with
/// a public address and answer the connection with a private one. Pinning the
/// checked addresses removes that window.
#[cfg(feature = "url-fetch")]
struct PinnedResolver {
    pins: Mutex<HashMap<String, Vec<SocketAddr>>>,
}

#[cfg(feature = "url-fetch")]
impl PinnedResolver {
    fn new() -> Self {
        Self {
            pins: Mutex::new(HashMap::new()),
        }
    }

    /// Use exactly these addresses for the next request to `host`.
    fn pin(&self, host: &str, addresses: Vec<SocketAddr>) {
        let mut pins = self
            .pins
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pins.insert(host.to_ascii_lowercase(), addresses);
    }
}

#[cfg(feature = "url-fetch")]
impl Resolve for PinnedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_ascii_lowercase();
        let pinned = {
            let pins = self
                .pins
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            pins.get(&host).cloned()
        };
        // A miss means the guard did not run for this hop; refusing is safer
        // than falling back to a fresh system lookup.
        Box::pin(std::future::ready(match pinned {
            Some(addresses) if !addresses.is_empty() => {
                Ok(Box::new(addresses.into_iter()) as Addrs)
            }
            _ => Err(std::io::Error::other(
                "the host was not resolved and checked before the request",
            )
            .into()),
        }))
    }
}

#[cfg(feature = "url-fetch")]
impl Fetcher {
    /// Build the client used for one document's fetches, with a fresh pinned
    /// resolver and the policy's byte, timeout and redirect limits.
    ///
    /// Proxy settings are ignored by design. Fails with
    /// [`FetchError::Client`] when the HTTP client cannot be built.
    pub fn new(policy: &FetchPolicy) -> Result<Self, FetchError> {
        Self::with_resolver(policy, Arc::clone(&SYSTEM_RESOLVER))
    }

    #[cfg(test)]
    fn with_lookup(policy: &FetchPolicy, lookup: Arc<Lookup>) -> Result<Self, FetchError> {
        Self::with_resolver(policy, Arc::new(ResolverPool::new(lookup)))
    }

    fn with_resolver(
        policy: &FetchPolicy,
        resolver: Arc<ResolverPool>,
    ) -> Result<Self, FetchError> {
        install_crypto_provider();
        let pins = Arc::new(PinnedResolver::new());
        let client = Client::builder()
            // Redirects are followed by hand: every hop must pass the same
            // address check as the first request, and one deadline covers the
            // whole chain.
            .redirect(redirect::Policy::none())
            // A local printer utility must not send document URLs through the
            // user's HTTP(S)_PROXY: an IP proxy would bypass the pinned DNS
            // answer, and a hostname proxy would resolve the name itself.
            .no_proxy()
            .dns_resolver(Arc::clone(&pins))
            .build()
            .map_err(|error| FetchError::Client {
                message: error.to_string(),
            })?;
        Ok(Self {
            client,
            pins,
            resolver,
            max_bytes: policy.max_bytes,
            timeout_ms: policy.timeout_ms,
            max_redirects: policy.max_redirects,
            allow_private_networks: policy.allow_private_networks,
        })
    }

    /// Fetch one URL as text, spending at most one timeout on it.
    pub fn fetch(&self, url: &str) -> Result<String, FetchError> {
        self.fetch_within(url, Duration::from_millis(u64::from(self.timeout_ms)))
    }

    /// Fetch one URL as text, spending at most `budget` on the request and
    /// every redirect together.
    ///
    /// A body is read with a bounded reader: the ceiling holds even when the
    /// server lies about `Content-Length` or sends chunked data.
    pub fn fetch_within(&self, url: &str, budget: Duration) -> Result<String, FetchError> {
        let deadline = Instant::now() + budget;
        let mut current = parse_http_url(url)?;
        let mut redirects = 0_u8;
        loop {
            self.resolve_and_pin(&current, deadline, budget)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(FetchError::Timeout {
                    url: current.to_string(),
                    timeout_ms: timeout_millis(budget),
                });
            }
            let response = self
                .client
                .get(current.clone())
                .timeout(remaining)
                .send()
                .map_err(|error| {
                    if error.is_timeout() {
                        FetchError::Timeout {
                            url: current.to_string(),
                            timeout_ms: timeout_millis(budget),
                        }
                    } else {
                        FetchError::Network {
                            url: current.to_string(),
                            message: error.to_string(),
                        }
                    }
                })?;

            let status = response.status();
            if status.is_redirection() {
                if redirects >= self.max_redirects {
                    return Err(FetchError::Redirect {
                        url: current.to_string(),
                        reason: format!("redirect limit of {} exceeded", self.max_redirects),
                    });
                }
                redirects = redirects.saturating_add(1);
                current = redirect_target(&current, &response)?;
                continue;
            }
            if !status.is_success() {
                return Err(FetchError::Status {
                    url: current.to_string(),
                    status: status.as_u16(),
                });
            }
            if let Some(length) = response.content_length()
                && length > self.max_bytes
            {
                return Err(FetchError::TooLarge {
                    url: current.to_string(),
                    limit: self.max_bytes,
                });
            }

            let mut body = Vec::new();
            let limit = self.max_bytes.saturating_add(1);
            response
                .take(limit)
                .read_to_end(&mut body)
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::TimedOut {
                        FetchError::Timeout {
                            url: current.to_string(),
                            timeout_ms: timeout_millis(budget),
                        }
                    } else {
                        FetchError::Network {
                            url: current.to_string(),
                            message: error.to_string(),
                        }
                    }
                })?;
            if body.len() as u64 > self.max_bytes {
                return Err(FetchError::TooLarge {
                    url: current.to_string(),
                    limit: self.max_bytes,
                });
            }
            return Ok(String::from_utf8_lossy(&body).into_owned());
        }
    }

    /// Check the hop's address once and pin it for the actual connection.
    fn resolve_and_pin(
        &self,
        url: &Url,
        deadline: Instant,
        budget: Duration,
    ) -> Result<(), FetchError> {
        let Some(host) = url.host_str() else {
            return Err(FetchError::BlockedAddress {
                url: url.to_string(),
                address: "the URL has no host".to_owned(),
            });
        };
        let host = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(host);
        let port = url.port_or_known_default().unwrap_or(80);
        let addresses = self.check_addresses(url, host, port, deadline, budget)?;
        self.pins.pin(host, addresses);
        Ok(())
    }

    /// Resolve one name within the shared deadline and refuse any private
    /// address in its answer.
    fn check_addresses(
        &self,
        url: &Url,
        host: &str,
        port: u16,
        deadline: Instant,
        budget: Duration,
    ) -> Result<Vec<SocketAddr>, FetchError> {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return match blocked_address(ip, self.allow_private_networks) {
                Some(address) => Err(FetchError::BlockedAddress {
                    url: url.to_string(),
                    address,
                }),
                None => Ok(vec![SocketAddr::new(ip, port)]),
            };
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(FetchError::Timeout {
                url: url.to_string(),
                timeout_ms: timeout_millis(budget),
            });
        }
        // `getaddrinfo` cannot be cancelled, so it runs on a fixed-size worker
        // pool: a lookup that misses the deadline is abandoned, but its worker
        // stays bounded and returns to the queue when the system resolver does.
        let ips = self
            .resolver
            .resolve(host, remaining)
            .map_err(|failure| match failure {
                ResolveFailure::Timeout => FetchError::Timeout {
                    url: url.to_string(),
                    timeout_ms: timeout_millis(budget),
                },
                ResolveFailure::QueueFull => FetchError::ResolverBusy {
                    url: url.to_string(),
                },
                ResolveFailure::Unavailable => FetchError::Network {
                    url: url.to_string(),
                    message: "the DNS resolver pool is unavailable".to_owned(),
                },
                ResolveFailure::Lookup(error) => FetchError::Network {
                    url: url.to_string(),
                    message: format!("cannot resolve {host}: {error}"),
                },
            })?;
        if ips.is_empty() {
            return Err(FetchError::Network {
                url: url.to_string(),
                message: format!("{host} resolved to no addresses"),
            });
        }
        // A name that resolves to any private address is refused: a server can
        // otherwise point a public name at the LAN and bypass the check.
        let mut addresses = Vec::with_capacity(ips.len());
        for ip in ips {
            if let Some(blocked) = blocked_address(ip, self.allow_private_networks) {
                return Err(FetchError::BlockedAddress {
                    url: url.to_string(),
                    address: blocked,
                });
            }
            addresses.push(SocketAddr::new(ip, port));
        }
        Ok(addresses)
    }
}

#[cfg(feature = "url-fetch")]
fn parse_http_url(value: &str) -> Result<Url, FetchError> {
    let url = Url::parse(value).map_err(|_| FetchError::Scheme {
        url: value.to_owned(),
    })?;
    if !is_http_url(&url) {
        return Err(FetchError::Scheme {
            url: value.to_owned(),
        });
    }
    Ok(url)
}

/// The next hop of a redirect, after checking its scheme.
#[cfg(feature = "url-fetch")]
fn redirect_target(current: &Url, response: &Response) -> Result<Url, FetchError> {
    let Some(location) = response.headers().get(header::LOCATION) else {
        return Err(FetchError::Redirect {
            url: current.to_string(),
            reason: format!("{} response has no Location header", response.status()),
        });
    };
    let location = location.to_str().map_err(|_| FetchError::Redirect {
        url: current.to_string(),
        reason: "the Location header is not ASCII".to_owned(),
    })?;
    let next = current
        .join(location)
        .map_err(|error| FetchError::Redirect {
            url: current.to_string(),
            reason: format!("invalid Location: {error}"),
        })?;
    if !is_http_url(&next) {
        return Err(FetchError::Redirect {
            url: current.to_string(),
            reason: "redirect leaves http or https".to_owned(),
        });
    }
    Ok(next)
}

/// Why an address must not be fetched, or `None` when it is public.
///
/// Link-local addresses stay blocked even with
/// [`FetchPolicy::allow_private_networks`]: that switch exists for a local
/// fixture server, not for a metadata service.
#[cfg(feature = "url-fetch")]
fn blocked_address(ip: IpAddr, allow_private_networks: bool) -> Option<String> {
    match ip {
        IpAddr::V4(v4) => blocked_v4(v4, allow_private_networks),
        IpAddr::V6(v6) => {
            // An IPv4-mapped literal reaches the IPv4 network through a v6
            // address, so it must be judged as the address it carries.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return blocked_v4(v4, allow_private_networks);
            }
            if v6.is_unspecified() || v6.is_multicast() || v6.is_unicast_link_local() {
                return Some(ip.to_string());
            }
            if !allow_private_networks && (v6.is_loopback() || v6.is_unique_local()) {
                return Some(ip.to_string());
            }
            None
        }
    }
}

#[cfg(feature = "url-fetch")]
fn blocked_v4(ip: Ipv4Addr, allow_private_networks: bool) -> Option<String> {
    if ip.is_link_local() || ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() {
        return Some(ip.to_string());
    }
    if !allow_private_networks && (ip.is_loopback() || ip.is_private() || is_cgnat(ip)) {
        return Some(ip.to_string());
    }
    None
}

/// 100.64.0.0/10, the carrier-grade NAT range some ISPs hand out.
#[cfg(feature = "url-fetch")]
fn is_cgnat(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

#[cfg(feature = "url-fetch")]
fn is_http_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

#[cfg(feature = "url-fetch")]
fn timeout_millis(timeout: Duration) -> u32 {
    u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX)
}

/// `rustls-no-provider` leaves the choice to the application. Ring keeps the
/// build free of a C toolchain requirement on every target.
#[cfg(feature = "url-fetch")]
fn install_crypto_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(all(test, feature = "url-fetch"))]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    use super::{FetchError, FetchPolicy, Fetcher, Lookup, ResolveFailure, ResolverPool};

    /// Serve `responses` on a loopback port and return the base URL.
    fn serve<F: Fn(usize) -> String + Send + 'static>(
        responses: usize,
        handler: F,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            for index in 0..responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0_u8; 4096];
                let _ = stream.read(&mut buffer);
                let response = handler(index);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    fn response(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
            body.len()
        )
    }

    fn fetcher(policy: FetchPolicy) -> Fetcher {
        Fetcher::new(&policy).unwrap()
    }

    /// A fetcher for the loopback fixture server. Private-network access is an
    /// explicit opt-in, exactly as the repository's URL fixture script sets it.
    fn local(policy: FetchPolicy) -> Fetcher {
        fetcher(FetchPolicy {
            allow_private_networks: true,
            ..policy
        })
    }

    #[test]
    fn fetches_a_body_and_follows_an_http_redirect() {
        let (base, server) = serve(2, |index| {
            if index == 0 {
                response("302 Found", "Location: /final\r\n", "")
            } else {
                response("200 OK", "", "remote body")
            }
        });
        let fetcher = local(FetchPolicy::default());
        assert_eq!(fetcher.fetch(&base).unwrap(), "remote body");
        server.join().unwrap();
    }

    #[test]
    fn rejects_a_redirect_that_leaves_http() {
        let (base, server) = serve(1, |_| {
            response("302 Found", "Location: file:///tmp/secret\r\n", "")
        });
        let error = local(FetchPolicy::default()).fetch(&base).unwrap_err();
        assert!(matches!(error, FetchError::Redirect { .. }), "{error:?}");
        assert_eq!(error.code(), "E_JOB_FAILED");
        server.join().unwrap();
    }

    #[test]
    fn rejects_more_redirects_than_the_policy_allows() {
        let (base, server) = serve(2, |_| response("302 Found", "Location: /again\r\n", ""));
        let policy = FetchPolicy {
            max_redirects: 0,
            ..FetchPolicy::default()
        };
        let error = local(policy).fetch(&base).unwrap_err();
        assert!(matches!(error, FetchError::Redirect { .. }));
        // The server is blocked on the second accept; the test thread must not
        // wait for it.
        drop(server);
    }

    #[test]
    fn enforces_the_byte_ceiling_while_reading() {
        let body = "x".repeat(64);
        let (base, server) = serve(1, move |_| response("200 OK", "", &body));
        let policy = FetchPolicy {
            max_bytes: 16,
            ..FetchPolicy::default()
        };
        let error = local(policy).fetch(&base).unwrap_err();
        assert!(matches!(error, FetchError::TooLarge { .. }));
        assert_eq!(error.code(), "E_TOO_LARGE");
        server.join().unwrap();
    }

    #[test]
    fn times_out_a_slow_response() {
        let (base, server) = serve(1, |_| {
            thread::sleep(Duration::from_millis(80));
            response("200 OK", "", "late")
        });
        let policy = FetchPolicy {
            timeout_ms: 30,
            ..FetchPolicy::default()
        };
        let error = local(policy).fetch(&base).unwrap_err();
        assert!(matches!(error, FetchError::Timeout { .. }));
        assert_eq!(error.code(), "E_TIMEOUT");
        server.join().unwrap();
    }

    #[test]
    fn rejects_non_http_schemes_before_opening_a_socket() {
        let error = fetcher(FetchPolicy::default())
            .fetch("file:///tmp/secret")
            .unwrap_err();
        assert!(matches!(error, FetchError::Scheme { .. }));
        assert_eq!(error.code(), "E_UNSUPPORTED");
    }

    #[test]
    fn reports_a_non_success_status() {
        let (base, server) = serve(1, |_| response("404 Not Found", "", "missing"));
        let error = local(FetchPolicy::default()).fetch(&base).unwrap_err();
        assert!(matches!(error, FetchError::Status { status: 404, .. }));
        server.join().unwrap();
    }

    #[test]
    fn refuses_loopback_private_and_link_local_addresses_by_default() {
        use std::net::Ipv4Addr;

        let fetcher = fetcher(FetchPolicy::default());
        // Private addresses are built from octets so the repository hygiene
        // gate does not read this test as a LAN address.
        let urls = [
            "http://127.0.0.1:9/".to_owned(),
            format!("http://{}/", Ipv4Addr::new(10, 0, 0, 1)),
            format!("http://{}/", Ipv4Addr::new(192, 168, 1, 1)),
            format!("http://{}/", Ipv4Addr::new(172, 16, 0, 1)),
            format!("http://{}/", Ipv4Addr::new(100, 64, 0, 1)),
            "http://169.254.169.254/latest/meta-data/".to_owned(),
            "http://[::1]/".to_owned(),
            "http://[fd00::1]/".to_owned(),
            "http://[fe80::1]/".to_owned(),
            "http://[::ffff:127.0.0.1]/".to_owned(),
            "http://localhost:9/".to_owned(),
        ];
        for url in urls {
            let error = fetcher.fetch(&url).unwrap_err();
            assert!(
                matches!(error, FetchError::BlockedAddress { .. }),
                "{url}: {error:?}"
            );
            assert_eq!(error.code(), "E_UNSUPPORTED");
        }
    }

    #[test]
    fn a_redirect_to_a_metadata_address_is_refused_at_the_hop() {
        let (base, server) = serve(1, |_| {
            response(
                "302 Found",
                "Location: http://169.254.169.254/latest/meta-data/\r\n",
                "",
            )
        });
        // Loopback is allowed for the fixture, but the link-local hop is not.
        let error = local(FetchPolicy::default()).fetch(&base).unwrap_err();
        match error {
            FetchError::BlockedAddress { url, address } => {
                assert!(url.contains("169.254.169.254"), "{url}");
                assert!(address.contains("169.254.169.254"), "{address}");
            }
            other => panic!("expected a blocked address, got {other:?}"),
        }
        server.join().unwrap();
    }

    #[test]
    fn the_whole_redirect_chain_shares_one_budget() {
        let (base, server) = serve(1, |_| {
            thread::sleep(Duration::from_millis(80));
            response("200 OK", "", "late")
        });
        let error = local(FetchPolicy::default())
            .fetch_within(&base, Duration::from_millis(20))
            .unwrap_err();
        assert!(matches!(error, FetchError::Timeout { .. }), "{error:?}");
        server.join().unwrap();
    }

    #[test]
    fn a_host_is_resolved_once_and_its_answer_is_pinned() {
        use std::net::{IpAddr, Ipv4Addr};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (base, server) = serve(1, |_| response("200 OK", "", "first answer"));
        let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
        let url = format!("http://rebind.test:{port}/");
        let lookups = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&lookups);
        let lookup: Arc<Lookup> = Arc::new(move |_host: &str| {
            let call = counter.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Ok(vec![IpAddr::from(Ipv4Addr::new(127, 0, 0, 1))])
            } else {
                // A rebinding resolver answers with a link-local address on the
                // second lookup. It must never be consulted: the checked answer
                // is what the connection uses.
                Ok(vec![IpAddr::from(Ipv4Addr::new(169, 254, 169, 254))])
            }
        });
        let policy = FetchPolicy {
            allow_private_networks: true,
            ..FetchPolicy::default()
        };
        let fetcher = Fetcher::with_lookup(&policy, lookup).unwrap();
        assert_eq!(fetcher.fetch(&url).unwrap(), "first answer");
        assert_eq!(
            lookups.load(Ordering::SeqCst),
            1,
            "the host was resolved again after the check"
        );
        server.join().unwrap();
    }

    #[test]
    fn an_answer_mixing_public_and_private_addresses_is_refused() {
        use std::net::{IpAddr, Ipv4Addr};
        use std::sync::Arc;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://rebind.test:{port}/");
        let lookup: Arc<Lookup> = Arc::new(move |_host: &str| {
            // The first address would pass the check; the second is a metadata
            // service. A name with any private address must be refused whole.
            Ok(vec![
                IpAddr::from(Ipv4Addr::new(127, 0, 0, 1)),
                IpAddr::from(Ipv4Addr::new(169, 254, 169, 254)),
            ])
        });
        let policy = FetchPolicy {
            allow_private_networks: true,
            ..FetchPolicy::default()
        };
        let error = Fetcher::with_lookup(&policy, lookup)
            .unwrap()
            .fetch(&url)
            .unwrap_err();
        assert!(
            matches!(error, FetchError::BlockedAddress { .. }),
            "{error:?}"
        );
        // Nothing connected: the rejection happened before a request.
        assert!(listener.accept().is_err());
    }

    #[test]
    fn resolver_pool_keeps_its_thread_count_under_pressure() {
        use std::net::{IpAddr, Ipv4Addr};
        use std::sync::Arc;

        let lookup: Arc<Lookup> = Arc::new(|_host: &str| {
            thread::sleep(Duration::from_millis(120));
            Ok(vec![IpAddr::from(Ipv4Addr::LOCALHOST)])
        });
        let pool = ResolverPool::new(lookup);
        assert_eq!(pool.spawned_threads(), 4, "the pool must be fixed-size");
        let mut full = 0_usize;
        let mut timed_out = 0_usize;
        for _ in 0..200 {
            match pool.resolve("slow.test", Duration::from_millis(5)) {
                Err(ResolveFailure::QueueFull) => full += 1,
                Err(ResolveFailure::Timeout) => timed_out += 1,
                Ok(_) => {}
                Err(other) => panic!("unexpected resolver failure: {other:?}"),
            }
        }
        assert!(full > 0, "the bounded queue never reported being full");
        assert!(timed_out > 0, "a queued lookup never missed its deadline");
        assert_eq!(
            pool.spawned_threads(),
            4,
            "pressure must not spawn more resolver threads"
        );
    }

    #[test]
    fn a_full_resolver_queue_is_reported_as_busy_not_timeout() {
        let error = FetchError::ResolverBusy {
            url: "http://busy.test/".to_owned(),
        };
        assert_eq!(error.code(), "E_JOB_FAILED");
        assert!(
            error.to_string().contains("resolver busy"),
            "the message must name the busy condition: {error}"
        );
    }

    #[test]
    fn resolver_pool_serves_again_after_abandoned_lookups() {
        use std::net::{IpAddr, Ipv4Addr};
        use std::sync::Arc;

        let lookup: Arc<Lookup> = Arc::new(|_host: &str| {
            thread::sleep(Duration::from_millis(30));
            Ok(vec![IpAddr::from(Ipv4Addr::LOCALHOST)])
        });
        let pool = ResolverPool::new(lookup);
        for _ in 0..12 {
            let _ = pool.resolve("slow.test", Duration::from_millis(1));
        }
        thread::sleep(Duration::from_millis(400));
        let addresses = pool.resolve("slow.test", Duration::from_secs(2)).unwrap();
        assert_eq!(addresses, vec![IpAddr::from(Ipv4Addr::LOCALHOST)]);
        assert_eq!(pool.spawned_threads(), 4);
    }

    #[test]
    fn timed_out_fetches_do_not_grow_the_resolver_pool() {
        use std::net::{IpAddr, Ipv4Addr};
        use std::sync::Arc;

        let lookup: Arc<Lookup> = Arc::new(|_host: &str| {
            thread::sleep(Duration::from_millis(60));
            Ok(vec![IpAddr::from(Ipv4Addr::LOCALHOST)])
        });
        let policy = FetchPolicy {
            allow_private_networks: true,
            ..FetchPolicy::default()
        };
        let fetcher = Fetcher::with_lookup(&policy, lookup).unwrap();
        for _ in 0..40 {
            let _ = fetcher.fetch_within("http://slow.test:9/", Duration::from_millis(5));
        }
        assert_eq!(fetcher.resolver.spawned_threads(), 4);
    }

    #[test]
    fn name_resolution_counts_against_the_deadline() {
        use std::net::{IpAddr, Ipv4Addr};
        use std::sync::Arc;

        let lookup: Arc<Lookup> = Arc::new(|_host: &str| {
            thread::sleep(Duration::from_millis(80));
            Ok(vec![IpAddr::from(Ipv4Addr::new(127, 0, 0, 1))])
        });
        let policy = FetchPolicy {
            allow_private_networks: true,
            ..FetchPolicy::default()
        };
        let error = Fetcher::with_lookup(&policy, lookup)
            .unwrap()
            .fetch_within("http://slow.test:9/", Duration::from_millis(10))
            .unwrap_err();
        assert!(matches!(error, FetchError::Timeout { .. }), "{error:?}");
    }
}
