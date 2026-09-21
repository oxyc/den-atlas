//! HTTP caching + conditional-request layer — the port of `src/http.ts`. One `serve` handles: strong ETag
//! with `If-None-Match`; `Last-Modified` + `If-Modified-Since` → 304; `HEAD`; `Range` → 206/416.
//!
//! Every body is now an in-memory one. The file-streaming path, its gzip variants and their `Vary` existed
//! for the dataset blobs (`labels-*.json`, `vectors-*.bin`, the metadata sidecar, the premise pair,
//! `facets.bin`), and those are no longer served (#113) — atlas answers questions about the store instead
//! of handing out copies of it.

use axum::body::Body;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;

/// The file generation described by the startup metadata, so a `Dataset::load` that overlaps a refresh
/// cannot bind the new files to a descriptor read before it. Writers stage on the same filesystem and
/// rename, so the two stats straddling the load see different inodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    len: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl FileIdentity {
    pub fn from_metadata(meta: &std::fs::Metadata) -> std::io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            len: meta.len(),
            modified: meta.modified()?,
            #[cfg(unix)]
            dev: meta.dev(),
            #[cfg(unix)]
            ino: meta.ino(),
        })
    }
}

pub struct Servable {
    /// Unquoted strong validator (the fnv of the body).
    pub etag_base: String,
    pub content_type: String,
    pub cache_control: String,
    pub last_modified: Option<String>,
    pub body: Bytes,
}

pub async fn serve(method: &Method, headers: &HeaderMap, s: Servable) -> Response {
    let is_head = method == Method::HEAD;
    let size = s.body.len() as u64;
    let etag = format!("\"{}\"", s.etag_base);

    let mut base: Vec<(&'static str, String)> = vec![
        ("etag", etag.clone()),
        ("cache-control", s.cache_control.clone()),
        ("accept-ranges", "bytes".to_owned()),
    ];
    if let Some(lm) = &s.last_modified {
        base.push(("last-modified", lm.clone()));
    }

    if is_not_modified(headers, &etag, s.last_modified.as_deref()) {
        return build(StatusCode::NOT_MODIFIED, &base, Body::empty());
    }

    if let Some(rh) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        // RFC 9110 §13.1.5: a Range with an If-Range that does not match the current representation
        // must be answered with the WHOLE thing, not the requested slice.
        // A non-matching If-Range falls through to the full 200 below.
        if if_range_matches(headers, &etag, s.last_modified.as_deref()) {
            match parse_range(rh, size) {
                RangeResult::Unsatisfiable => {
                    // NOT cacheable. This body depends entirely on the Range header, which is not in
                    // any shared cache's key, so it inherited the response's own `max-age` and one
                    // client's bad Range could in principle pin a 416 on the URL for its whole TTL.
                    let mut h: Vec<(&'static str, String)> =
                        base.iter().filter(|(k, _)| *k != "cache-control").cloned().collect();
                    h.push(("cache-control", "no-store".to_owned()));
                    h.push(("content-range", format!("bytes */{size}")));
                    h.push(("content-type", s.content_type.clone()));
                    return build(StatusCode::RANGE_NOT_SATISFIABLE, &h, Body::empty());
                }
                RangeResult::Range { start, end } => {
                    let len = end - start + 1;
                    let mut h = base.clone();
                    h.push(("content-type", s.content_type.clone()));
                    h.push(("content-range", format!("bytes {start}-{end}/{size}")));
                    h.push(("content-length", len.to_string()));
                    let body = if is_head {
                        Body::empty()
                    } else {
                        Body::from(s.body.slice(start as usize..(start + len) as usize))
                    };
                    return build(StatusCode::PARTIAL_CONTENT, &h, body);
                }
                RangeResult::None => {} // malformed / multi-range → full 200
            }
        }
    }

    let mut h = base;
    h.push(("content-type", s.content_type.clone()));
    h.push(("content-length", size.to_string()));
    let body = if is_head { Body::empty() } else { Body::from(s.body) };
    build(StatusCode::OK, &h, body)
}

fn build(status: StatusCode, headers: &[(&'static str, String)], body: Body) -> Response {
    // `Access-Control-Allow-Origin` is added once, for every response, by `handler::handle`.
    let mut b = Response::builder().status(status);
    for (k, v) in headers {
        // Skip a header whose value isn't a valid HTTP field value (e.g. a junk sha256/date from a bad
        // meta with a newline/control byte) rather than letting `body().unwrap()` panic the task.
        if let Ok(val) = header::HeaderValue::from_str(v) {
            b = b.header(*k, val);
        }
    }
    b.body(body).unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Whether an `If-Range` precondition allows the range to be served.
///
/// Absent ⇒ yes (an unconditional Range). Present ⇒ it is either the entity-tag or the
/// last-modified date the client already holds, and only an exact match permits the partial
/// response. RFC 9110 requires a strong comparison here, so a `W/` weak tag never matches — unlike
/// `If-None-Match`, where weak comparison is correct.
fn if_range_matches(headers: &HeaderMap, etag_quoted: &str, last_modified: Option<&str>) -> bool {
    let Some(ir) = headers.get("if-range").and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let ir = ir.trim();
    if ir.starts_with('"') {
        return ir == etag_quoted;
    }
    // Not an entity-tag ⇒ an HTTP-date, compared against Last-Modified. No date to compare against
    // means the client cannot have a valid one either.
    last_modified.is_some_and(|lm| lm == ir)
}

/// `If-None-Match` (precedence, RFC 9110 §13.1.3), else `If-Modified-Since`.
pub fn is_not_modified(headers: &HeaderMap, etag_quoted: &str, last_modified: Option<&str>) -> bool {
    if let Some(inm) = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) {
        return inm
            .split(',')
            .map(|t| t.trim().trim_start_matches("W/"))
            .any(|t| t == "*" || t == etag_quoted);
    }
    if let (Some(ims), Some(lm)) =
        (headers.get(header::IF_MODIFIED_SINCE).and_then(|v| v.to_str().ok()), last_modified)
    {
        if let (Ok(since), Ok(modified)) = (httpdate::parse_http_date(ims), httpdate::parse_http_date(lm)) {
            return modified <= since;
        }
    }
    false
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
    // Both halves are already known to be all-digits, so a parse failure means one thing: the number
    // does not fit in u64. `unwrap_or(0)` treated that as zero, which is the opposite of what the
    // client asked for — `bytes=99999999999999999999999-` became "from byte 0", so a request for a
    // range past the end of the file was answered with the WHOLE file under a 206.
    let (start, end);
    if a.is_empty() {
        // A suffix larger than u64 is larger than the file, so it selects all of it.
        let suffix: u64 = b.parse().unwrap_or(u64::MAX);
        if suffix == 0 {
            return RangeResult::Unsatisfiable;
        }
        start = size.saturating_sub(suffix);
        end = size - 1;
    } else {
        // A start beyond u64 is beyond the file.
        let Ok(s) = a.parse::<u64>() else {
            return RangeResult::Unsatisfiable;
        };
        start = s;
        if start >= size {
            return RangeResult::Unsatisfiable;
        }
        // An end beyond u64 is beyond the file, which RFC 9110 says to clamp, not reject.
        end = if b.is_empty() { size - 1 } else { b.parse::<u64>().unwrap_or(u64::MAX).min(size - 1) };
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

    fn servable() -> Servable {
        Servable {
            etag_base: SHA.to_owned(),
            content_type: "application/octet-stream".to_owned(),
            cache_control: "public, max-age=3600".to_owned(),
            last_modified: Some(LAST_MODIFIED.to_owned()),
            body: Bytes::from(vec![b'x'; 1000]),
        }
    }

    async fn body_bytes(resp: Response) -> Vec<u8> {
        axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec()
    }

    /// A replacement is a different generation even when it lands with the same length and mtime,
    /// because it is a different inode. `Dataset::load` straddles its own read of dataset.meta.json
    /// with this, so a load that overlaps a refresh refuses rather than binding new files to an old
    /// descriptor.
    #[test]
    fn a_replaced_file_is_not_the_generation_that_was_loaded() {
        let dir = std::env::temp_dir().join(format!("atlas-generation-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blob");
        std::fs::write(&path, b"old-bytes!").unwrap();
        let stat = std::fs::metadata(&path).unwrap();
        let loaded = FileIdentity::from_metadata(&stat).unwrap();
        assert_eq!(FileIdentity::from_metadata(&std::fs::metadata(&path).unwrap()).unwrap(), loaded);

        let next = dir.join("next");
        std::fs::write(&next, b"new-bytes!").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&next)
            .unwrap()
            .set_modified(stat.modified().unwrap())
            .unwrap();
        std::fs::rename(&next, &path).unwrap();
        let replaced = FileIdentity::from_metadata(&std::fs::metadata(&path).unwrap()).unwrap();
        assert_ne!(replaced, loaded, "a same-length, same-mtime replacement passed as the loaded file");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn t304_on_if_none_match() {
        let h = hdrs(&[("if-none-match", &format!("\"{SHA}\""))]);
        let r = serve(&Method::GET, &h, servable()).await;
        assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(r.headers().get("etag").unwrap(), &format!("\"{SHA}\""));
        assert!(body_bytes(r).await.is_empty());
    }

    #[tokio::test]
    async fn t304_on_star_and_ims() {
        assert_eq!(
            serve(&Method::GET, &hdrs(&[("if-none-match", "*")]), servable()).await.status(),
            StatusCode::NOT_MODIFIED
        );
        assert_eq!(
            serve(&Method::GET, &hdrs(&[("if-modified-since", LAST_MODIFIED)]), servable()).await.status(),
            StatusCode::NOT_MODIFIED
        );
        // Before the build time → 200.
        assert_eq!(
            serve(&Method::GET, &hdrs(&[("if-modified-since", "Tue, 30 Jun 2026 00:00:00 GMT")]), servable())
                .await
                .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn t_inm_precedence_over_ims() {
        let h = hdrs(&[("if-none-match", "\"nope\""), ("if-modified-since", LAST_MODIFIED)]);
        assert_eq!(serve(&Method::GET, &h, servable()).await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn t_range_206_416() {
        let r = serve(&Method::GET, &hdrs(&[("range", "bytes=0-9")]), servable()).await;
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(r.headers().get("content-range").unwrap(), "bytes 0-9/1000");
        assert_eq!(r.headers().get("content-length").unwrap(), "10");
        assert_eq!(body_bytes(r).await.len(), 10);

        let sfx = serve(&Method::GET, &hdrs(&[("range", "bytes=-7")]), servable()).await;
        assert_eq!(sfx.headers().get("content-range").unwrap(), "bytes 993-999/1000");

        let un = serve(&Method::GET, &hdrs(&[("range", "bytes=2000-")]), servable()).await;
        assert_eq!(un.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(un.headers().get("content-range").unwrap(), "bytes */1000");
    }

    #[tokio::test]
    async fn t_head_no_body() {
        let r = serve(&Method::HEAD, &hdrs(&[]), servable()).await;
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

    /// A 416 keeps every validator the 200 would have carried — a cache and a client both need them
    /// to revalidate afterwards. Only `cache-control` is replaced.
    #[tokio::test]
    async fn an_unsatisfiable_range_keeps_its_validators() {
        let resp = serve(&Method::GET, &hdrs(&[("range", "bytes=9999-")]), servable()).await;
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        let h = resp.headers();
        assert_eq!(h.get("etag").unwrap(), &format!("\"{SHA}\""));
        assert_eq!(h.get("last-modified").unwrap(), LAST_MODIFIED);
        assert_eq!(h.get("accept-ranges").unwrap(), "bytes");
        assert_eq!(h.get_all("cache-control").iter().count(), 1, "cache-control was duplicated");
        assert_eq!(h.get("cache-control").unwrap(), "no-store");
    }

    /// A 416 must not be cacheable: it is determined by the Range header, which no shared cache keys
    /// on, and it inherited the blob's year-long `immutable`.
    #[tokio::test]
    async fn an_unsatisfiable_range_is_not_cacheable() {
        let resp = serve(&Method::GET, &hdrs(&[("range", "bytes=9999-")]), servable()).await;
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        let cc = resp.headers().get("cache-control").unwrap().to_str().unwrap().to_owned();
        assert!(cc.contains("no-store"), "a 416 was made cacheable: {cc}");
        assert!(!cc.contains("immutable"), "a 416 inherited the blob's immutable: {cc}");
        assert_eq!(resp.headers().get("content-range").unwrap(), "bytes */1000");
    }

    /// `bytes=-0` asks for the last zero bytes. Without the guard the suffix branch produces
    /// `start = size, end = size - 1`, and `serve`'s `end - start + 1` then underflows — a panic in
    /// debug, a wrapped length in release. Nothing else in that branch checks `start > end`.
    #[tokio::test]
    async fn a_zero_length_suffix_range_is_unsatisfiable_not_an_underflow() {
        assert!(matches!(parse_range("bytes=-0", 1000), RangeResult::Unsatisfiable));
        let resp = serve(&Method::GET, &hdrs(&[("range", "bytes=-0")]), servable()).await;
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    /// An empty body is reachable (an empty catalog page is a real answer), and every range against
    /// it must be unsatisfiable rather than underflowing `size - 1`.
    #[tokio::test]
    async fn every_range_against_an_empty_representation_is_unsatisfiable() {
        for r in ["bytes=0-0", "bytes=0-", "bytes=-1", "bytes=-0", "bytes=5-9"] {
            assert!(
                matches!(parse_range(r, 0), RangeResult::Unsatisfiable),
                "{r} was satisfiable on an empty body"
            );
        }
        let mut s = servable();
        s.body = Bytes::new();
        let resp = serve(&Method::GET, &hdrs(&[("range", "bytes=0-0")]), s).await;
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(resp.headers().get("content-range").unwrap(), "bytes */0");
    }

    /// A malformed or multi-range header degrades to the full 200 the RFC allows, rather than a 416
    /// — a client asking for something we cannot parse still gets the representation.
    #[tokio::test]
    async fn an_unparseable_range_serves_the_whole_representation() {
        for r in ["bytes=0-10,20-30", "items=0-10", "bytes=abc-def", "garbage"] {
            let resp = serve(&Method::GET, &hdrs(&[("range", r)]), servable()).await;
            assert_eq!(resp.status(), StatusCode::OK, "{r} was answered with {}", resp.status());
            assert_eq!(body_bytes(resp).await.len(), 1000, "{r}");
        }
    }

    /// Both halves of a range are already known to be all-digits, so a parse failure means the number
    /// does not fit in u64. `unwrap_or(0)` read that as zero — the opposite of what was asked — so a
    /// range starting past the end of the file was answered with the WHOLE file under a 206.
    #[test]
    fn a_range_too_large_for_u64_is_unsatisfiable_not_the_whole_file() {
        let huge = "99999999999999999999999";
        assert!(matches!(parse_range(&format!("bytes={huge}-"), 1000), RangeResult::Unsatisfiable));
        assert!(matches!(parse_range(&format!("bytes={huge}-{huge}"), 1000), RangeResult::Unsatisfiable));
        // An END past u64 is clamped rather than rejected, per RFC 9110.
        assert!(matches!(
            parse_range(&format!("bytes=10-{huge}"), 1000),
            RangeResult::Range { start: 10, end: 999 }
        ));
        // A SUFFIX past u64 is longer than the file, so it selects all of it.
        assert!(matches!(
            parse_range(&format!("bytes=-{huge}"), 1000),
            RangeResult::Range { start: 0, end: 999 }
        ));
    }

    /// RFC 9110 §13.1.5: a Range whose If-Range does not match must get the whole representation.
    /// Ignoring it spliced two generations' bytes together when a client resumed across a refresh.
    #[tokio::test]
    async fn a_stale_if_range_gets_the_whole_thing_not_a_slice() {
        let etag = format!("\"{SHA}\"");
        let range = ("range", "bytes=0-9");

        let stale =
            serve(&Method::GET, &hdrs(&[range, ("if-range", "\"an-older-dataset\"")]), servable()).await;
        assert_eq!(stale.status(), StatusCode::OK, "a stale If-Range still got a partial response");
        assert_eq!(body_bytes(stale).await.len(), 1000);

        let current = serve(&Method::GET, &hdrs(&[range, ("if-range", &etag)]), servable()).await;
        assert_eq!(current.status(), StatusCode::PARTIAL_CONTENT, "a matching If-Range was ignored");
        assert_eq!(body_bytes(current).await.len(), 10);

        // The date form, against Last-Modified.
        let by_date = serve(&Method::GET, &hdrs(&[range, ("if-range", LAST_MODIFIED)]), servable()).await;
        assert_eq!(by_date.status(), StatusCode::PARTIAL_CONTENT);
        let wrong_date =
            serve(&Method::GET, &hdrs(&[range, ("if-range", "Tue, 01 Jul 2025 00:00:00 GMT")]), servable())
                .await;
        assert_eq!(wrong_date.status(), StatusCode::OK);

        // A weak tag never satisfies If-Range — strong comparison only.
        let weak = serve(&Method::GET, &hdrs(&[range, ("if-range", &format!("W/{etag}"))]), servable()).await;
        assert_eq!(weak.status(), StatusCode::OK, "a weak validator satisfied If-Range");

        // No If-Range at all is still an ordinary range request.
        let plain = serve(&Method::GET, &hdrs(&[range]), servable()).await;
        assert_eq!(plain.status(), StatusCode::PARTIAL_CONTENT);
    }

    /// Nothing here varies on a request header any more: no gzip variant to negotiate, and no body
    /// built from the request's own origin. A `Vary` naming a header the body does not depend on
    /// splits a shared cache for nothing.
    #[tokio::test]
    async fn no_response_varies_on_a_request_header() {
        for headers in [hdrs(&[]), hdrs(&[("accept-encoding", "gzip")]), hdrs(&[("host", "atlas.test")])] {
            let r = serve(&Method::GET, &headers, servable()).await;
            assert!(r.headers().get("vary").is_none(), "{:?}", r.headers());
            assert!(r.headers().get("content-encoding").is_none(), "{:?}", r.headers());
        }
    }
}
