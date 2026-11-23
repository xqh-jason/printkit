//! Read-only browser view of files under `<data_dir>/exports`.
//!
//! `doc.exportPdf` returns a disk path; the browser cannot open `file://`
//! from an http(s) page, so the page opens this loopback HTTP route instead.
//! The listener already binds loopback. This handler additionally requires the
//! `Host` header to name the loopback endpoint (blocks DNS rebinding) and
//! confines reads to canonical paths under `exports/` (blocks traversal and
//! symlink escapes).

use std::path::{Component, Path, PathBuf};

use printkit_protocol::ErrorCode;
use salvo::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, HOST};
use salvo::http::{HeaderValue, Method, StatusCode};
use salvo::prelude::*;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::AppState;

/// Why an export file could not be served.
#[derive(Debug, PartialEq, Eq)]
pub enum ExportReadError {
    /// The request names a path outside `exports/`.
    Invalid,
    /// The path is acceptable but no regular file exists there.
    NotFound,
}

/// Decode a request path once, tolerating a literal `%` that is not an escape.
///
/// The HTTP stack may hand over the segment still encoded (`100%25.pdf`) or
/// already decoded (`100%.pdf`); accepting both without a second decode keeps
/// a percent in a filename readable through either spelling. Control bytes are
/// refused.
fn decode_export_path(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            let pair = bytes.get(index + 1..index + 3).and_then(|pair| {
                let text = std::str::from_utf8(pair).ok()?;
                u8::from_str_radix(text, 16).ok()
            });
            if let Some(value) = pair {
                decoded.push(value);
                index += 3;
                continue;
            }
        }
        decoded.push(byte);
        index += 1;
    }
    if decoded.iter().any(|byte| *byte < 0x20 || *byte == 0x7f) {
        return None;
    }
    String::from_utf8(decoded).ok()
}

/// Resolve one URL path to a canonical file under `<data_dir>/exports`.
///
/// Directory components must be plain names: `.`, `..`, roots and prefixes are
/// rejected after decoding, before touching the filesystem. The canonical
/// target must still start with the canonical exports prefix, so a pre-existing
/// symlink cannot escape it.
pub fn resolve_export_file(data_dir: &Path, rest: &str) -> Result<PathBuf, ExportReadError> {
    if rest.is_empty() {
        return Err(ExportReadError::Invalid);
    }
    let rest = decode_export_path(rest).ok_or(ExportReadError::Invalid)?;
    let request = Path::new(&rest);
    if request.is_absolute() {
        return Err(ExportReadError::Invalid);
    }
    let mut relative = PathBuf::new();
    for component in request.components() {
        match component {
            Component::Normal(name) => {
                // Windows strips trailing dots, so `name.` can alias `name`;
                // refuse the variant instead of resolving a different file.
                if name.to_string_lossy().ends_with('.') {
                    return Err(ExportReadError::Invalid);
                }
                relative.push(name);
            }
            Component::CurDir | Component::ParentDir => return Err(ExportReadError::Invalid),
            Component::RootDir | Component::Prefix(_) => return Err(ExportReadError::Invalid),
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(ExportReadError::Invalid);
    }

    let exports = data_dir.join("exports");
    let canonical_exports =
        std::fs::canonicalize(&exports).map_err(|_| ExportReadError::NotFound)?;
    let target = canonical_exports.join(relative);
    let canonical_target = std::fs::canonicalize(&target).map_err(|_| ExportReadError::NotFound)?;
    if !canonical_target.starts_with(&canonical_exports) {
        return Err(ExportReadError::Invalid);
    }
    if !canonical_target.is_file() {
        return Err(ExportReadError::NotFound);
    }
    Ok(canonical_target)
}

/// Browser-readable exports are capability URLs: the link carries the exact
/// canonical file path and a short expiry, signed with a per-run secret. This
/// keeps `<a href>` working without a cookie while stopping another local user
/// from reading any file under `exports/` just because it exists.
pub fn sign_export(secret: &[u8; 32], canonical_path: &str, expires_at: u64) -> String {
    let mut mac = hmac_sha256(secret, format!("{canonical_path}\n{expires_at}").as_bytes());
    let mut out = String::with_capacity(mac.len() * 2);
    for byte in &mut mac {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Verify an export signature. Expired and wrong-length signatures fail
/// without leaking which part was wrong; the compare is constant time.
pub fn verify_export(
    secret: &[u8; 32],
    canonical_path: &str,
    expires_at: &str,
    signature: &str,
    now: u64,
) -> bool {
    let Ok(expires_at) = expires_at.trim().parse::<u64>() else {
        return false;
    };
    if expires_at < now {
        return false;
    }
    if signature.len() != 64 {
        return false;
    }
    let expected = sign_export(secret, canonical_path, expires_at);
    constant_time_eq(expected.as_bytes(), signature.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    let mut outer = Sha256::new();
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for index in 0..64 {
        ipad[index] ^= block[index];
        opad[index] ^= block[index];
    }
    inner.update(ipad);
    inner.update(message);
    let inner = inner.finalize();
    outer.update(opad);
    outer.update(inner);
    outer.finalize().into()
}

/// The `Host` header must be one of the service's own loopback origins.
fn host_matches(origins: &[String], host: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    origins.iter().any(|origin| {
        origin
            .strip_prefix("http://")
            .is_some_and(|allowed| allowed.eq_ignore_ascii_case(host))
    })
}

fn host_is_local(state: &AppState, req: &Request) -> bool {
    host_matches(
        &state.admin_origins,
        req.headers()
            .get(HOST)
            .and_then(|value| value.to_str().ok()),
    )
}
/// Error bodies on this route are JSON too, so a browser integration can
/// branch on `code` instead of string-matching `text/plain`.
fn fail(res: &mut Response, status: StatusCode, code: ErrorCode, message: &str) {
    res.status_code(status);
    res.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    res.render(Text::Json(
        json!({ "ok": false, "error": { "code": code.as_str(), "message": message } }).to_string(),
    ));
}

#[handler]
pub async fn serve(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    let Some(state) = depot.obtain::<std::sync::Arc<AppState>>().ok().cloned() else {
        return fail(
            res,
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::JobFailed,
            "service state is missing",
        );
    };
    if !host_is_local(&state, req) {
        return fail(
            res,
            StatusCode::FORBIDDEN,
            ErrorCode::Origin,
            "the export view is loopback-only",
        );
    }
    // Read the raw URI path instead of a decoded route param, so a literal `%`
    // in a filename is decoded exactly once by `resolve_export_file`.
    let rest = req
        .uri()
        .path()
        .strip_prefix("/exports/")
        .unwrap_or_default()
        .to_owned();
    let expires_at = req.query::<String>("exp").unwrap_or_default();
    let signature = req.query::<String>("sig").unwrap_or_default();
    let target = match resolve_export_file(&state.data_dir, &rest) {
        Ok(target) => target,
        Err(ExportReadError::Invalid) => {
            return fail(
                res,
                StatusCode::BAD_REQUEST,
                ErrorCode::Schema,
                "invalid export path",
            );
        }
        Err(ExportReadError::NotFound) => {
            return fail(
                res,
                StatusCode::NOT_FOUND,
                ErrorCode::NotFound,
                "export not found",
            );
        }
    };
    let canonical = target.to_string_lossy().to_string();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default();
    let Some(export_secret) = state.export_secret.as_ref() else {
        return fail(
            res,
            StatusCode::FORBIDDEN,
            ErrorCode::Auth,
            "export link signing is unavailable",
        );
    };
    if !verify_export(export_secret, &canonical, &expires_at, &signature, now) {
        return fail(
            res,
            StatusCode::FORBIDDEN,
            ErrorCode::Auth,
            "this export link is missing, expired or not valid for the file",
        );
    }
    let metadata = match tokio::fs::metadata(&target).await {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => {
            return fail(
                res,
                StatusCode::NOT_FOUND,
                ErrorCode::NotFound,
                "export not found",
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return fail(
                res,
                StatusCode::NOT_FOUND,
                ErrorCode::NotFound,
                "export not found",
            );
        }
        Err(_) => {
            return fail(
                res,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::JobFailed,
                "cannot read the export",
            );
        }
    };

    res.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/pdf"));
    res.headers_mut()
        .insert(CONTENT_DISPOSITION, HeaderValue::from_static("inline"));
    res.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res.headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from(metadata.len()));
    // HEAD answers from metadata only; the body is never read or allocated.
    if req.method() == Method::HEAD {
        return;
    }
    match tokio::fs::read(&target).await {
        Ok(bytes) => {
            let _ = res.write_body(bytes);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return fail(
                res,
                StatusCode::NOT_FOUND,
                ErrorCode::NotFound,
                "export not found",
            );
        }
        Err(_) => {
            return fail(
                res,
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::JobFailed,
                "cannot read the export",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn rejects_encoded_traversal_and_trailing_dots() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("exports")).unwrap();
        fs::write(dir.path().join("exports/name."), b"%PDF-1.4").unwrap();
        assert_eq!(
            resolve_export_file(dir.path(), "name."),
            Err(ExportReadError::Invalid)
        );
        assert_eq!(
            resolve_export_file(dir.path(), "%2e%2e/secret.pdf"),
            Err(ExportReadError::Invalid)
        );
        assert_eq!(
            resolve_export_file(dir.path(), "..%2fsecret.pdf"),
            Err(ExportReadError::Invalid)
        );
    }

    #[test]
    fn a_percent_in_a_filename_is_decoded_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("exports")).unwrap();
        let file = dir.path().join("exports/100%.pdf");
        fs::write(&file, b"%PDF-1.4").unwrap();
        let resolved = resolve_export_file(dir.path(), "100%25.pdf").unwrap();
        assert_eq!(resolved, fs::canonicalize(&file).unwrap());
        // The router may hand over the path already decoded; a literal percent
        // still names the same file and must not be rejected or decoded twice.
        assert_eq!(
            resolve_export_file(dir.path(), "100%.pdf").unwrap(),
            fs::canonicalize(&file).unwrap()
        );
        assert_eq!(
            resolve_export_file(dir.path(), "100%2525.pdf"),
            Err(ExportReadError::NotFound),
            "one decode only: %2525 names %25, not %"
        );
    }

    #[test]
    fn serves_a_file_under_exports() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("exports/labels")).unwrap();
        let file = dir.path().join("exports/labels/a.pdf");
        fs::write(&file, b"%PDF-1.4").unwrap();
        let resolved = resolve_export_file(dir.path(), "labels/a.pdf").unwrap();
        assert_eq!(resolved, fs::canonicalize(&file).unwrap());
    }

    #[test]
    fn rejects_traversal_and_rooted_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("exports")).unwrap();
        assert_eq!(
            resolve_export_file(dir.path(), "../secret.pdf"),
            Err(ExportReadError::Invalid)
        );
        assert_eq!(
            resolve_export_file(dir.path(), "/etc/passwd"),
            Err(ExportReadError::Invalid)
        );
        assert_eq!(
            resolve_export_file(dir.path(), "./a.pdf"),
            Err(ExportReadError::Invalid)
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlink_that_leaves_exports() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.pdf"), b"secret").unwrap();
        fs::create_dir_all(dir.path().join("exports")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.pdf"),
            dir.path().join("exports/link.pdf"),
        )
        .unwrap();
        assert_eq!(
            resolve_export_file(dir.path(), "link.pdf"),
            Err(ExportReadError::Invalid)
        );
    }

    #[test]
    fn reports_a_missing_file_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("exports")).unwrap();
        assert_eq!(
            resolve_export_file(dir.path(), "missing.pdf"),
            Err(ExportReadError::NotFound)
        );
    }

    #[test]
    fn hmac_sha256_matches_the_rfc4231_vector() {
        // RFC 4231 test case 2.
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex = mac
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn export_signatures_bind_path_and_expiry() {
        let secret = [7u8; 32];
        let path = "/data/exports/a.pdf";
        let now = 1_700_000_000;
        let expires = now + 600;
        let signature = sign_export(&secret, path, expires);
        assert!(verify_export(
            &secret,
            path,
            &expires.to_string(),
            &signature,
            now
        ));
        assert!(!verify_export(
            &secret,
            "/data/exports/b.pdf",
            &expires.to_string(),
            &signature,
            now
        ));
        assert!(!verify_export(
            &secret,
            path,
            &(expires + 1).to_string(),
            &signature,
            now
        ));
        assert!(!verify_export(
            &secret,
            path,
            &expires.to_string(),
            &signature[..62],
            now
        ));
        assert!(!verify_export(
            &secret,
            path,
            &expires.to_string(),
            &signature,
            expires + 1
        ));
    }

    #[test]
    fn host_allowlist_accepts_loopback_and_rejects_other_hosts() {
        let origins = vec![
            "http://127.0.0.1:19080".to_owned(),
            "http://localhost:19080".to_owned(),
        ];
        assert!(host_matches(&origins, Some("127.0.0.1:19080")));
        assert!(host_matches(&origins, Some("LOCALHOST:19080")));
        assert!(!host_matches(&origins, Some("evil.example:19080")));
        assert!(!host_matches(&origins, Some("127.0.0.1:19081")));
        assert!(!host_matches(&origins, None));
    }
}
