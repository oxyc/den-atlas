//! Small shared helpers — ports of `src/util.ts` (fnv1a, public_origin, plain json responses).

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;

/// FNV-1a 32-bit → 8-char hex, byte-identical to the TS `fnv1a` (which hashes `charCodeAt`, i.e. UTF-16
/// code units). Used for the small JSON responses' ETags; the big blobs use their real sha256.
pub fn fnv1a(input: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for unit in input.encode_utf16() {
        h ^= unit as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{:08x}", h)
}

/// A plain JSON response, explicitly uncacheable (used for /health, 404, 405, and the 503
/// dataset-unavailable body). `no-store` keeps a CDN from pinning a transient error/outage past its
/// recovery — the same reason the catalog error path shortens its TTL.
pub fn json_response(body: &'static str, status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*") // public data, no credentials
        .body(Body::from(body))
        .unwrap()
}

/// The public origin for descriptor blob URLs — `PUBLIC_BASE_URL` override, else `X-Forwarded-Proto` +
/// `X-Forwarded-Host`/`Host` (Caddy sets these), else `http`/`localhost`. Port of `publicOrigin`.
pub fn public_origin(headers: &HeaderMap, override_base: Option<&str>) -> String {
    if let Some(base) = override_base {
        return base.trim_end_matches('/').to_owned();
    }
    let first = |name: &str| -> Option<String> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    let proto = first("x-forwarded-proto").unwrap_or_else(|| "http".to_owned());
    // Only reflect a sane Host charset into the blob URLs we advertise (a spoofed Host would point
    // the app's fetch at an attacker origin; the checksum still gates content). PUBLIC_BASE_URL
    // short-circuits this in prod.
    let host = first("x-forwarded-host")
        .or_else(|| {
            headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_owned())
        })
        .filter(|h| is_sane_host(h))
        .unwrap_or_else(|| "localhost".to_owned());
    format!("{proto}://{host}")
}

/// A hostname/authority we're willing to reflect into a returned URL: alnum + host+port punctuation.
fn is_sane_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 255
        && h.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'_'))
}
