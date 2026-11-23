#![cfg(feature = "url-fetch")]
#![allow(clippy::panic, clippy::unwrap_used)]
#![allow(unsafe_code)] // SAFETY comments on the environment writes below.

//! A document URL must not travel through the user's HTTP(S) proxy.
//!
//! The client pins the DNS answers it checked. An IP proxy would connect to the
//! proxy instead of that pinned address, and a hostname proxy would resolve the
//! name itself. `.no_proxy()` on the builder closes both paths; this test is the
//! regression guard for that setting.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

use printkit_engine::{FetchPolicy, Fetcher};

const PROXY_VARS: [&str; 4] = ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"];
const BYPASS_VARS: [&str; 2] = ["NO_PROXY", "no_proxy"];

/// Sets the proxy variables for the test and restores them on drop.
struct ProxyEnv {
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl ProxyEnv {
    fn set(proxy: &str) -> Self {
        let saved = PROXY_VARS
            .iter()
            .chain(BYPASS_VARS.iter())
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        // SAFETY: this is the only test in this integration-test binary, so no
        // other thread reads the environment concurrently. `BYPASS_VARS` is
        // cleared so the proxy would apply if the client consulted it.
        unsafe {
            for key in PROXY_VARS {
                std::env::set_var(key, proxy);
            }
            for key in BYPASS_VARS {
                std::env::remove_var(key);
            }
        }
        Self { saved }
    }
}

impl Drop for ProxyEnv {
    fn drop(&mut self) {
        // SAFETY: same single-threaded reasoning as in `set`.
        unsafe {
            for (key, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// Serve one request on `listener`, or give up after two seconds.
fn serve_one(listener: TcpListener, body: &'static str) -> thread::JoinHandle<()> {
    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut request = [0_u8; 4096];
                    let _ = stream.read(&mut request);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        }
    })
}

#[test]
fn system_proxies_never_see_a_document_url() {
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let proxy_url = format!("http://{}/", proxy.local_addr().unwrap());

    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = target.local_addr().unwrap().port();
    let server = serve_one(target, "direct");
    let url = format!("http://127.0.0.1:{port}/");

    // An IP proxy and a hostname proxy both live behind these variables; the
    // client must ignore them.
    let _env = ProxyEnv::set(&proxy_url);

    let policy = FetchPolicy {
        timeout_ms: 300,
        allow_private_networks: true,
        ..FetchPolicy::default()
    };
    let fetcher = Fetcher::new(&policy).unwrap();
    assert_eq!(fetcher.fetch(&url).unwrap(), "direct");
    server.join().unwrap();

    // A poisoned client would have left at least one connection on the proxy.
    thread::sleep(Duration::from_millis(100));
    let mut proxy_connections = 0;
    while proxy.accept().is_ok() {
        proxy_connections += 1;
    }
    assert_eq!(
        proxy_connections, 0,
        "a document URL went through the system proxy"
    );
}
