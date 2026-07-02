//! Small shared helpers — ports of `src/util.ts` (fnv1a, public_origin, plain json/html responses).

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

/// A plain JSON response with no caching (matches the TS `json()` — used for /health, 404, 405).
pub fn json_response(body: &'static str, status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

/// A plain HTML response (the landing page).
pub fn html_response(body: String) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
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
    let host = first("x-forwarded-host")
        .or_else(|| {
            headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_owned())
        })
        .unwrap_or_else(|| "localhost".to_owned());
    format!("{proto}://{host}")
}

/// HTML-escape (landing page) — port of the TS `escapeHtml`.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// `57872` → `"57,872"` (matches `Number.toLocaleString("en-US")` for the landing page).
pub fn group_thousands(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}
