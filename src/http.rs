//! HTTP caching + conditional-request layer — the port of `src/http.ts`, but the body layer STREAMS from
//! disk (never loads a blob into RAM). One `serve` handles: strong ETag (+ distinct `-gzip` variant) with
//! `If-None-Match`; `Last-Modified` + `If-Modified-Since` → 304; `HEAD`; `Range` → 206/416; gzip negotiation
//! (`Vary` only when a gzip variant exists). Range is served on the identity representation only.

use axum::body::Body;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

/// A body source: an in-memory buffer (small JSON) or a file streamed from disk (the big blobs).
#[derive(Clone)]
pub enum Payload {
    Memory(Bytes),
    File(PathBuf),
}

pub struct Servable {
    /// Unquoted strong validator (sha256 for blobs, fnv for JSON).
    pub etag_base: String,
    pub content_type: String,
    pub cache_control: String,
    pub last_modified: Option<String>,
    /// Identity byte length.
    pub size: u64,
    pub identity: Payload,
    /// Precomputed gzip body + its size (only where compression pays — the labels JSON).
    pub gzip: Option<(Payload, u64)>,
}

pub async fn serve(method: &Method, headers: &HeaderMap, s: Servable) -> Response {
    let is_head = method == Method::HEAD;
    let range_header = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());
    // Range wins over gzip (byte offsets need the identity representation).
    let use_gzip = range_header.is_none() && s.gzip.is_some() && accepts_gzip(headers);
    // Distinct strong ETag per content-coding (RFC 9110 §8.8.3) — decided on the selected representation.
    let etag = if use_gzip {
        format!("\"{}-gzip\"", s.etag_base)
    } else {
        format!("\"{}\"", s.etag_base)
    };

    let mut base: Vec<(&'static str, String)> = vec![
        ("etag", etag.clone()),
        ("cache-control", s.cache_control.clone()),
        ("accept-ranges", "bytes".to_owned()),
    ];
    // `Vary` only when a gzip variant exists — else a CDN split-caches the identical identity blob per AE.
    if s.gzip.is_some() {
        base.push(("vary", "Accept-Encoding".to_owned()));
    }
    if let Some(lm) = &s.last_modified {
        base.push(("last-modified", lm.clone()));
    }

    if is_not_modified(headers, &etag, s.last_modified.as_deref()) {
        return build(StatusCode::NOT_MODIFIED, &base, Body::empty());
    }

    if let Some(rh) = &range_header {
        match parse_range(rh, s.size) {
            RangeResult::Unsatisfiable => {
                let mut h = base.clone();
                h.push(("content-range", format!("bytes */{}", s.size)));
                h.push(("content-type", s.content_type.clone()));
                return build(StatusCode::RANGE_NOT_SATISFIABLE, &h, Body::empty());
            }
            RangeResult::Range { start, end } => {
                let len = end - start + 1;
                let mut h = base.clone();
                h.push(("content-type", s.content_type.clone()));
                h.push(("content-range", format!("bytes {start}-{end}/{}", s.size)));
                h.push(("content-length", len.to_string()));
                let body = if is_head {
                    Body::empty()
                } else {
                    range_body(&s.identity, start, len).await
                };
                return build(StatusCode::PARTIAL_CONTENT, &h, body);
            }
            RangeResult::None => {} // malformed / multi-range → full 200
        }
    }

    let (payload, size, encoding) = if use_gzip {
        let (p, sz) = s.gzip.as_ref().unwrap();
        (p, *sz, Some("gzip"))
    } else {
        (&s.identity, s.size, None)
    };
    let mut h = base;
    h.push(("content-type", s.content_type.clone()));
    h.push(("content-length", size.to_string()));
    if let Some(enc) = encoding {
        h.push(("content-encoding", enc.to_owned()));
    }
    let body = if is_head {
        Body::empty()
    } else {
        full_body(payload).await
    };
    build(StatusCode::OK, &h, body)
}

fn build(status: StatusCode, headers: &[(&'static str, String)], body: Body) -> Response {
    // Public, credential-free data — allow cross-origin reads (e.g. a browser-based Stremio client).
    let mut b = Response::builder().status(status).header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
    for (k, v) in headers {
        // Skip a header whose value isn't a valid HTTP field value (e.g. a junk sha256/date from a bad
        // meta with a newline/control byte) rather than letting `body().unwrap()` panic the task.
        if let Ok(val) = header::HeaderValue::from_str(v) {
            b = b.header(*k, val);
        }
    }
    b.body(body).unwrap_or_else(|_| Response::new(Body::empty()))
}

async fn full_body(p: &Payload) -> Body {
    match p {
        Payload::Memory(b) => Body::from(b.clone()),
        Payload::File(path) => match tokio::fs::File::open(path).await {
            Ok(f) => Body::from_stream(ReaderStream::new(f)),
            Err(_) => Body::empty(),
        },
    }
}

async fn range_body(p: &Payload, start: u64, len: u64) -> Body {
    match p {
        Payload::Memory(b) => {
            let s = start as usize;
            let e = (start + len) as usize;
            Body::from(b.slice(s..e.min(b.len())))
        }
        Payload::File(path) => match tokio::fs::File::open(path).await {
            Ok(mut f) => {
                let _ = f.seek(std::io::SeekFrom::Start(start)).await;
                Body::from_stream(ReaderStream::new(f.take(len)))
            }
            Err(_) => Body::empty(),
        },
    }
}

/// `If-None-Match` (precedence, RFC 9110 §13.1.3), else `If-Modified-Since`.
pub fn is_not_modified(headers: &HeaderMap, etag_quoted: &str, last_modified: Option<&str>) -> bool {
    if let Some(inm) = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) {
        return inm
            .split(',')
            .map(|t| t.trim().trim_start_matches("W/"))
            .any(|t| t == "*" || t == etag_quoted);
    }
    if let (Some(ims), Some(lm)) = (
        headers
            .get(header::IF_MODIFIED_SINCE)
            .and_then(|v| v.to_str().ok()),
        last_modified,
    ) {
        if let (Ok(since), Ok(modified)) =
            (httpdate::parse_http_date(ims), httpdate::parse_http_date(lm))
        {
            return modified <= since;
        }
    }
    false
}

pub fn accepts_gzip(headers: &HeaderMap) -> bool {
    let ae = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    ae.split(',').any(|part| {
        let mut it = part.trim().split(';');
        let enc = it.next().unwrap_or("").trim();
        if enc != "gzip" && enc != "*" {
            return false; // `*` = any encoding (RFC 9110 §12.5.3)
        }
        match it.map(|p| p.trim()).find(|p| p.starts_with("q=")) {
            None => true,
            Some(qs) => qs[2..].parse::<f64>().map(|n| n > 0.0).unwrap_or(false), // honor q=0 / garbage
        }
    })
}

#[derive(Debug, PartialEq)]
pub enum RangeResult {
    Range { start: u64, end: u64 },
    Unsatisfiable,
    None,
}

/// Single-range `bytes=a-b` only. Multi-range / garbage → `None` (serve full 200). Port of `parseRange`.
pub fn parse_range(header: &str, size: u64) -> RangeResult {
    let rest = match header.trim().strip_prefix("bytes=") {
        Some(r) => r,
        None => return RangeResult::None,
    };
    if size == 0 {
        return RangeResult::Unsatisfiable; // avoid `size - 1` underflow on an empty representation
    }
    let parts: Vec<&str> = rest.split('-').collect();
    if parts.len() != 2 {
        return RangeResult::None; // multi-range or garbage
    }
    let (a, b) = (parts[0], parts[1]);
    if !a.bytes().all(|c| c.is_ascii_digit()) || !b.bytes().all(|c| c.is_ascii_digit()) {
        return RangeResult::None;
    }
    if a.is_empty() && b.is_empty() {
        return RangeResult::None;
    }
    let (start, end);
    if a.is_empty() {
        let suffix: u64 = b.parse().unwrap_or(0);
        if suffix == 0 {
            return RangeResult::Unsatisfiable;
        }
        start = size.saturating_sub(suffix);
        end = size - 1;
    } else {
        start = a.parse().unwrap_or(0);
        if start >= size {
            return RangeResult::Unsatisfiable;
        }
        end = if b.is_empty() {
            size - 1
        } else {
            b.parse::<u64>().unwrap_or(0).min(size - 1)
        };
        if start > end {
            return RangeResult::Unsatisfiable;
        }
    }
    RangeResult::Range { start, end }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    const SHA: &str = "aaaaaaaaaaaaaaaa"; // 16 hex
    const LAST_MODIFIED: &str = "Wed, 01 Jul 2026 00:00:00 GMT";

    fn hdrs(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            let name = axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap();
            h.insert(name, v.parse().unwrap());
        }
        h
    }

    fn servable(gzip: bool) -> Servable {
        let raw = Bytes::from(vec![b'x'; 1000]);
        Servable {
            etag_base: SHA.to_owned(),
            content_type: "application/octet-stream".to_owned(),
            cache_control: "public, max-age=3600".to_owned(),
            last_modified: Some(LAST_MODIFIED.to_owned()),
            size: raw.len() as u64,
            identity: Payload::Memory(raw.clone()),
            gzip: if gzip {
                Some((Payload::Memory(Bytes::from(vec![b'z'; 40])), 40))
            } else {
                None
            },
        }
    }

    async fn body_bytes(resp: Response) -> Vec<u8> {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    #[tokio::test]
    async fn t304_on_if_none_match() {
        let h = hdrs(&[("if-none-match", &format!("\"{SHA}\""))]);
        let r = serve(&Method::GET, &h, servable(false)).await;
        assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(r.headers().get("etag").unwrap(), &format!("\"{SHA}\""));
        assert!(body_bytes(r).await.is_empty());
    }

    #[tokio::test]
    async fn t304_on_star_and_ims() {
        assert_eq!(
            serve(&Method::GET, &hdrs(&[("if-none-match", "*")]), servable(false))
                .await
                .status(),
            StatusCode::NOT_MODIFIED
        );
        assert_eq!(
            serve(&Method::GET, &hdrs(&[("if-modified-since", LAST_MODIFIED)]), servable(false))
                .await
                .status(),
            StatusCode::NOT_MODIFIED
        );
        // Before the build time → 200.
        assert_eq!(
            serve(&Method::GET, &hdrs(&[("if-modified-since", "Tue, 30 Jun 2026 00:00:00 GMT")]), servable(false))
                .await
                .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn t_inm_precedence_over_ims() {
        let h = hdrs(&[("if-none-match", "\"nope\""), ("if-modified-since", LAST_MODIFIED)]);
        assert_eq!(serve(&Method::GET, &h, servable(false)).await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn t_gzip_distinct_etag_and_vary() {
        let h = hdrs(&[("accept-encoding", "gzip, deflate")]);
        let r = serve(&Method::GET, &h, servable(true)).await;
        assert_eq!(r.headers().get("content-encoding").unwrap(), "gzip");
        assert_eq!(r.headers().get("vary").unwrap(), "Accept-Encoding");
        assert_eq!(r.headers().get("etag").unwrap(), &format!("\"{SHA}-gzip\""));
        assert_eq!(body_bytes(r).await.len(), 40);
    }

    #[tokio::test]
    async fn t_gzip_star_and_q0_and_absent() {
        assert_eq!(
            serve(&Method::GET, &hdrs(&[("accept-encoding", "*")]), servable(true))
                .await
                .headers()
                .get("content-encoding")
                .map(|v| v.to_str().unwrap().to_owned()),
            Some("gzip".to_owned())
        );
        assert!(serve(&Method::GET, &hdrs(&[("accept-encoding", "gzip;q=0")]), servable(true))
            .await
            .headers()
            .get("content-encoding")
            .is_none());
        // Identity servable → no Vary.
        assert!(serve(&Method::GET, &hdrs(&[]), servable(false)).await.headers().get("vary").is_none());
        // Gzip servable → Vary present.
        assert_eq!(serve(&Method::GET, &hdrs(&[]), servable(true)).await.headers().get("vary").unwrap(), "Accept-Encoding");
    }

    #[tokio::test]
    async fn t_range_206_416() {
        let r = serve(&Method::GET, &hdrs(&[("range", "bytes=0-9")]), servable(false)).await;
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(r.headers().get("content-range").unwrap(), "bytes 0-9/1000");
        assert_eq!(r.headers().get("content-length").unwrap(), "10");
        assert_eq!(body_bytes(r).await.len(), 10);

        let sfx = serve(&Method::GET, &hdrs(&[("range", "bytes=-7")]), servable(false)).await;
        assert_eq!(sfx.headers().get("content-range").unwrap(), "bytes 993-999/1000");

        let un = serve(&Method::GET, &hdrs(&[("range", "bytes=2000-")]), servable(false)).await;
        assert_eq!(un.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(un.headers().get("content-range").unwrap(), "bytes */1000");
    }

    #[tokio::test]
    async fn t_range_wins_over_gzip() {
        let h = hdrs(&[("range", "bytes=0-9"), ("accept-encoding", "gzip")]);
        let r = serve(&Method::GET, &h, servable(true)).await;
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert!(r.headers().get("content-encoding").is_none());
    }

    #[tokio::test]
    async fn t_head_no_body() {
        let r = serve(&Method::HEAD, &hdrs(&[]), servable(false)).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers().get("content-length").unwrap(), "1000");
        assert!(body_bytes(r).await.is_empty());
    }

    #[test]
    fn t_parse_range() {
        assert_eq!(parse_range("bytes=0-9", 100), RangeResult::Range { start: 0, end: 9 });
        assert_eq!(parse_range("bytes=90-", 100), RangeResult::Range { start: 90, end: 99 });
        assert_eq!(parse_range("bytes=-10", 100), RangeResult::Range { start: 90, end: 99 });
        assert_eq!(parse_range("bytes=50-9999", 100), RangeResult::Range { start: 50, end: 99 });
        assert_eq!(parse_range("bytes=200-", 100), RangeResult::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-9,20-29", 100), RangeResult::None);
        assert_eq!(parse_range("nonsense", 100), RangeResult::None);
    }
}
