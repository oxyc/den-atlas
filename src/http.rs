//! HTTP caching + conditional-request layer — the port of `src/http.ts`, but the body layer STREAMS from
//! disk (never loads a blob into RAM). One `serve` handles: strong ETag (+ distinct `-gzip` variant) with
//! `If-None-Match`; `Last-Modified` + `If-Modified-Since` → 304; `HEAD`; `Range` → 206/416; gzip negotiation
//! (`Vary` only when a gzip variant exists). Range is served on the identity representation only.

use crate::util::log_due;
use axum::body::Body;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
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
    /// The body embeds the request's forwarded host/scheme, so a shared cache must key on them.
    pub vary_on_origin: bool,
}

/// How often a standing condition on the request path may be reported.
const LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

pub async fn serve(method: &Method, headers: &HeaderMap, s: Servable) -> Response {
    let is_head = method == Method::HEAD;
    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).map(|s| s.to_owned());
    // Range wins over gzip (byte offsets need the identity representation).
    let wants_gzip = range_header.is_none() && s.gzip.is_some() && accepts_gzip(headers);
    // The gzip variant is an OPTIMISATION, which is what `resolve_blob` says at load time: an
    // unusable one drops the variant rather than taking the dataset down. Runtime has to agree, or a
    // release that stops publishing a `.gz` — the sync deletes the undeclared file while the process
    // is still serving the old meta — turns every `Accept-Encoding: gzip` request into a 503, which
    // is every URLSession client, for a blob sitting readable on disk right beside it.
    //
    // Opened HERE, before the ETag, because the ETag names the representation being served: falling
    // back after choosing `"<sha>-gzip"` would hand identity bytes to a cache under the gzip
    // validator (RFC 9110 §8.8.3). The cost is one open on a request that turns out to be a 304.
    let gz_open =
        if wants_gzip { open_payload(&s.gzip.as_ref().unwrap().0, "the gzip variant").await } else { None };
    let use_gzip = gz_open.is_some();
    // Distinct strong ETag per content-coding (RFC 9110 §8.8.3) — decided on the selected representation.
    let etag = if use_gzip { format!("\"{}-gzip\"", s.etag_base) } else { format!("\"{}\"", s.etag_base) };

    let mut base: Vec<(&'static str, String)> = vec![
        ("etag", etag.clone()),
        ("cache-control", s.cache_control.clone()),
        ("accept-ranges", "bytes".to_owned()),
    ];
    // `Vary` only when a gzip variant exists — else a CDN split-caches the identical identity blob per AE.
    // The forwarded host/scheme are added by the caller for bodies that EMBED them (the descriptor's
    // absolute blob URLs), because those are unkeyed request inputs a shared cache would otherwise
    // ignore, handing one requester's chosen origin to everyone under the plain URL.
    let mut vary: Vec<&str> = Vec::new();
    if s.gzip.is_some() {
        vary.push("Accept-Encoding");
    }
    if s.vary_on_origin {
        vary.extend(["X-Forwarded-Host", "X-Forwarded-Proto", "Host"]);
    }
    if !vary.is_empty() {
        base.push(("vary", vary.join(", ")));
    }
    if let Some(lm) = &s.last_modified {
        base.push(("last-modified", lm.clone()));
    }

    if is_not_modified(headers, &etag, s.last_modified.as_deref()) {
        return build(StatusCode::NOT_MODIFIED, &base, Body::empty());
    }

    if let Some(rh) = &range_header {
        // RFC 9110 §13.1.5: a Range with an If-Range that does not match the current representation
        // must be answered with the WHOLE thing, not the requested slice. Ignoring it meant a client
        // resuming a partial download across a dataset refresh spliced two datasets' bytes together
        // under the new ETag — and since Range wins over gzip, a client that had received the gzip
        // labels variant resumed with identity bytes spliced into a gzip stream. The sha256 check
        // catches it, so the cost is a wasted download rather than corruption; this makes the resume
        // work instead.
        // A non-matching If-Range falls through to the full 200 below.
        if if_range_matches(headers, &etag, s.last_modified.as_deref()) {
            match parse_range(rh, s.size) {
                RangeResult::Unsatisfiable => {
                    // NOT cacheable. This body depends entirely on the Range header, which is not in
                    // any shared cache's key, so it inherited the blob's `immutable, max-age=1y` and
                    // one client's bad Range could in principle pin a 416 on the blob for a year.
                    let mut h: Vec<(&'static str, String)> =
                        base.iter().filter(|(k, _)| *k != "cache-control").cloned().collect();
                    h.push(("cache-control", "no-store".to_owned()));
                    h.push(("content-range", format!("bytes */{}", s.size)));
                    h.push(("content-type", s.content_type.clone()));
                    return build(StatusCode::RANGE_NOT_SATISFIABLE, &h, Body::empty());
                }
                RangeResult::Range { start, end } => {
                    // Opened before the 206 is built, and the handle carries the body, so the file
                    // cannot vanish between the check and the read.
                    let Some(open) = open_payload(&s.identity, "the blob").await else {
                        return unavailable();
                    };
                    let len = end - start + 1;
                    let mut h = base.clone();
                    h.push(("content-type", s.content_type.clone()));
                    h.push(("content-range", format!("bytes {start}-{end}/{}", s.size)));
                    h.push(("content-length", len.to_string()));
                    let body = if is_head { Body::empty() } else { range_body(open, start, len).await };
                    return build(StatusCode::PARTIAL_CONTENT, &h, body);
                }
                RangeResult::None => {} // malformed / multi-range → full 200
            }
        }
    }

    // The gzip variant is an OPTIMISATION, which is what `resolve_blob` says at load time: an
    // unusable one drops the variant rather than taking the dataset down. Runtime has to agree, or a
    // release that stops publishing a `.gz` — the sync deletes the undeclared file while the process
    // still serves the old meta — turns every `Accept-Encoding: gzip` request into a 503, which is
    // every URLSession client, for a blob sitting readable on disk right next to it.
    let (open, size, encoding) = match gz_open {
        Some(o) => (o, s.gzip.as_ref().unwrap().1, Some("gzip")),
        None => {
            // Only a missing IDENTITY blob is unserveable.
            let Some(o) = open_payload(&s.identity, "the blob").await else {
                return unavailable();
            };
            (o, s.size, None)
        }
    };
    if wants_gzip && encoding.is_none() {
        // `resolve_blob` logs the same degradation at load time; this is its runtime twin, and
        // without it every client silently drops to the full uncompressed blob.
        //
        // Reported HERE rather than where the fallback is decided, because that runs before the
        // conditional check: a 304 serves nothing, and the line claimed it was serving identity.
        // Throttled, because it describes a STATE that lasts the whole release-without-a-gz window,
        // and one line per request is an amplifier.
        static GZ_FALLBACK: AtomicU64 = AtomicU64::new(0);
        if log_due(&GZ_FALLBACK, LOG_EVERY) {
            eprintln!("den-atlas: serving {} as identity — its gzip variant is unusable", s.etag_base);
        }
    }
    let mut h = base;
    // A FALLBACK is not a cacheable answer. The response is identity bytes under `Vary:
    // Accept-Encoding`, so a shared cache would store it against the gzip key — and with `?v=` that
    // is `immutable` for a year, long after the variant is back. Serving it is right; letting it
    // outlive the condition is the mistake `unavailable()` already avoids for the same reason.
    if wants_gzip && encoding.is_none() {
        h.retain(|(k, _)| *k != "cache-control");
        h.push(("cache-control", "no-store".to_owned()));
    }
    h.push(("content-type", s.content_type.clone()));
    h.push(("content-length", size.to_string()));
    if let Some(enc) = encoding {
        h.push(("content-encoding", enc.to_owned()));
    }
    let body = if is_head { Body::empty() } else { full_body(open) };
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

/// A payload with its file already OPEN, so the bytes a response promises are pinned before its
/// status and `content-length` are chosen.
///
/// Checking readability and then reopening for the body is not enough: the file can vanish between
/// the two opens, and the body helper's fallback then produces an empty body whose size hint hyper
/// uses to rewrite `content-length` to 0. The result is a self-consistent 200 with the blob's real
/// ETag and zero bytes — reproduced at 3 in 900 requests against an unlink-and-recreate loop —
/// which under `?v=<version>` is `immutable, max-age=1y`, so a CDN pins a zero-length file as the
/// blob's valid representation for a year. Holding the handle removes the window: an unlinked file
/// stays readable through an open descriptor.
enum Open {
    Memory(Bytes),
    File(tokio::fs::File),
}

async fn open_payload(p: &Payload, what: &str) -> Option<Open> {
    match p {
        Payload::Memory(b) => Some(Open::Memory(b.clone())),
        Payload::File(path) => match tokio::fs::File::open(path).await {
            Ok(f) => Some(Open::File(f)),
            Err(e) => {
                // The real errno, not just "gone". EACCES and EMFILE both land here and neither is
                // fixed by re-fetching the dataset, which is what the 503's detail text tells the
                // operator to do. Swallowing it left the whole runtime blob path silent.
                // Throttled: an unreadable blob is a standing condition, and a client can ask for
                // it in a loop — measured at 28k lines and 4.8 MB of stderr per second on loopback,
                // which fills a json-file log driver's disk and pushes everything else out of
                // journald's rate limiter. One line a minute reports the same fact.
                static OPEN_FAILED: AtomicU64 = AtomicU64::new(0);
                if log_due(&OPEN_FAILED, LOG_EVERY) {
                    eprintln!("den-atlas: cannot open {what} ({}): {e}", path.display());
                }
                None
            }
        },
    }
}

/// The blob is declared by the meta but could not be opened. `no-store`, because the whole problem
/// with the old behaviour was a broken answer being cached; this one must never outlive the
/// condition. `open_payload` has already logged the real errno — this text names the likeliest cause
/// rather than the only one.
fn unavailable() -> Response {
    build(
        StatusCode::SERVICE_UNAVAILABLE,
        &[("content-type", "application/json".to_owned()), ("cache-control", "no-store".to_owned())],
        Body::from(
            r#"{"error":"blob_unavailable","detail":"the dataset declares this blob but it could not be opened (missing, or unreadable); see the server log for the reason"}"#,
        ),
    )
}

fn full_body(p: Open) -> Body {
    match p {
        Open::Memory(b) => Body::from(b),
        Open::File(f) => Body::from_stream(ReaderStream::new(f)),
    }
}

async fn range_body(p: Open, start: u64, len: u64) -> Body {
    match p {
        Open::Memory(b) => {
            let s = start as usize;
            let e = (start + len) as usize;
            Body::from(b.slice(s..e.min(b.len())))
        }
        Open::File(mut f) => {
            let _ = f.seek(std::io::SeekFrom::Start(start)).await;
            Body::from_stream(ReaderStream::new(f.take(len)))
        }
    }
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

pub fn accepts_gzip(headers: &HeaderMap) -> bool {
    let ae = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).unwrap_or("");
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

    fn servable(gzip: bool) -> Servable {
        let raw = Bytes::from(vec![b'x'; 1000]);
        Servable {
            etag_base: SHA.to_owned(),
            content_type: "application/octet-stream".to_owned(),
            cache_control: "public, max-age=3600".to_owned(),
            last_modified: Some(LAST_MODIFIED.to_owned()),
            size: raw.len() as u64,
            identity: Payload::Memory(raw.clone()),
            gzip: if gzip { Some((Payload::Memory(Bytes::from(vec![b'z'; 40])), 40)) } else { None },
            vary_on_origin: false,
        }
    }

    async fn body_bytes(resp: Response) -> Vec<u8> {
        axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec()
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
            serve(&Method::GET, &hdrs(&[("if-none-match", "*")]), servable(false)).await.status(),
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
            serve(
                &Method::GET,
                &hdrs(&[("if-modified-since", "Tue, 30 Jun 2026 00:00:00 GMT")]),
                servable(false)
            )
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
        assert_eq!(
            serve(&Method::GET, &hdrs(&[]), servable(true)).await.headers().get("vary").unwrap(),
            "Accept-Encoding"
        );
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

    /// A body that embeds the request's own host/scheme must name them in Vary, or a shared cache
    /// serves one requester's chosen origin to everyone under the plain URL. The descriptor's blob
    /// URLs are built from those headers and it goes out `public, max-age=300`.
    /// A blob the meta declares but disk does not have used to go out as a 200 with the real ETag,
    /// the declared content-length and NO bytes. Under `?v=<version>` that carries
    /// `immutable, max-age=1y`, so a CDN pins a zero-length file as the valid representation for a
    /// year and every client behind it fails its sha256 check with no way to recover.
    #[tokio::test]
    async fn a_declared_but_missing_blob_is_unavailable_not_an_empty_200() {
        let missing = std::env::temp_dir().join(format!("den-atlas-gone-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&missing);
        let mut s = servable(false);
        s.identity = Payload::File(missing.clone());

        let resp = serve(&Method::GET, &HeaderMap::new(), s).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "a missing blob was served as success");
        let cc = resp.headers().get("cache-control").unwrap().to_str().unwrap().to_owned();
        assert!(cc.contains("no-store"), "a broken answer was made cacheable: {cc}");
        assert!(resp.headers().get("etag").is_none(), "the blob's ETag was attached to a failure");

        // The range path too — it built its own 206 with the same empty body.
        let mut s = servable(false);
        s.identity = Payload::File(missing);
        let r = serve(&Method::GET, &hdrs(&[("range", "bytes=0-99")]), s).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE, "a missing blob was served as a 206");
    }

    /// The same fixture, but FILE-backed — which is what production always is (`handler.rs` builds
    /// every blob from `Payload::File`). Every `serve` test used `Payload::Memory` for both
    /// representations, so the `Open::File` arms were exercised once and never for byte correctness:
    /// deleting the `seek` in `range_body`, taking the gzip content-length from the identity size,
    /// and dropping `range_header.is_none()` from the gzip decision all passed 104 tests.
    ///
    /// Identity is 1000 bytes of ascending values so an offset is visible in the bytes themselves;
    /// the "gzip" variant is 40 distinct bytes (not real gzip — nothing here decompresses).
    fn file_servable(dir: &std::path::Path, gzip: bool) -> Servable {
        std::fs::create_dir_all(dir).unwrap();
        let raw: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let id = dir.join("blob.bin");
        std::fs::write(&id, &raw).unwrap();
        let gz = dir.join("blob.bin.gz");
        std::fs::write(&gz, vec![b'z'; 40]).unwrap();
        Servable {
            etag_base: SHA.to_owned(),
            content_type: "application/octet-stream".to_owned(),
            cache_control: "public, max-age=31536000, immutable".to_owned(),
            last_modified: Some(LAST_MODIFIED.to_owned()),
            size: raw.len() as u64,
            identity: Payload::File(id),
            gzip: if gzip { Some((Payload::File(gz), 40)) } else { None },
            vary_on_origin: false,
        }
    }

    /// A Range forces the identity representation, so it must also carry the IDENTITY validator.
    /// Serving identity bytes under `"<sha>-gzip"` is the RFC 9110 §8.8.3 hazard the ordering above
    /// exists to prevent, and it is what a shared cache would then hand every later gzip request.
    #[tokio::test]
    async fn a_range_from_a_gzip_client_is_identity_bytes_under_the_identity_etag() {
        let dir = std::env::temp_dir().join(format!("den-atlas-rgz-{}", std::process::id()));
        let resp = serve(
            &Method::GET,
            &hdrs(&[("range", "bytes=100-109"), ("accept-encoding", "gzip")]),
            file_servable(&dir, true),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            resp.headers().get("etag").unwrap(),
            &format!("\"{SHA}\""),
            "a 206 carried the gzip validator"
        );
        assert!(resp.headers().get("content-encoding").is_none());
        // ...and the bytes come from the right OFFSET. Dropping the seek returns byte 0 onwards.
        let want: Vec<u8> = (100..110u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(body_bytes(resp).await, want, "the 206 served the wrong offset");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The gzip response must describe the GZIP file: its length, its bytes, its validator. Taking
    /// the length from the identity size makes a streamed body hang or truncate.
    #[tokio::test]
    async fn a_gzip_response_describes_the_gzip_file_not_the_identity_one() {
        let dir = std::env::temp_dir().join(format!("den-atlas-gzf-{}", std::process::id()));
        let resp =
            serve(&Method::GET, &hdrs(&[("accept-encoding", "gzip")]), file_servable(&dir, true)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("content-encoding").unwrap(), "gzip");
        assert_eq!(resp.headers().get("etag").unwrap(), &format!("\"{SHA}-gzip\""));
        assert_eq!(resp.headers().get("content-length").unwrap(), "40", "the identity length was declared");
        assert_eq!(body_bytes(resp).await, vec![b'z'; 40], "the identity bytes were served as gzip");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A 416 keeps every validator the 200 would have carried — a cache and a client both need them
    /// to revalidate afterwards. Only `cache-control` is replaced.
    #[tokio::test]
    async fn an_unsatisfiable_range_keeps_its_validators() {
        let dir = std::env::temp_dir().join(format!("den-atlas-416v-{}", std::process::id()));
        let resp = serve(&Method::GET, &hdrs(&[("range", "bytes=9999-")]), file_servable(&dir, true)).await;
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        let h = resp.headers();
        assert_eq!(h.get("etag").unwrap(), &format!("\"{SHA}\""));
        assert_eq!(h.get("last-modified").unwrap(), LAST_MODIFIED);
        assert_eq!(h.get("accept-ranges").unwrap(), "bytes");
        assert_eq!(h.get("vary").unwrap(), "Accept-Encoding");
        assert_eq!(h.get_all("cache-control").iter().count(), 1, "cache-control was duplicated");
        assert_eq!(h.get("cache-control").unwrap(), "no-store");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The gz→identity fallback must not be stored: it is identity bytes under `Vary:
    /// Accept-Encoding`, so a cache would serve them for the gzip key long after the variant is back.
    #[tokio::test]
    async fn the_gzip_fallback_is_not_cacheable() {
        let dir = std::env::temp_dir().join(format!("den-atlas-gzfb-{}", std::process::id()));
        let mut sv = file_servable(&dir, true);
        sv.gzip = Some((Payload::File(dir.join("not-there.gz")), 40));
        let resp = serve(&Method::GET, &hdrs(&[("accept-encoding", "gzip")]), sv).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("content-encoding").is_none());
        let cc = resp.headers().get("cache-control").unwrap().to_str().unwrap().to_owned();
        assert!(cc.contains("no-store"), "a fallback was made cacheable: {cc}");
        assert!(!cc.contains("immutable"), "{cc}");

        // ...while an ordinary identity request, which is not a fallback, keeps its long TTL.
        let plain = serve(&Method::GET, &HeaderMap::new(), file_servable(&dir, true)).await;
        let cc = plain.headers().get("cache-control").unwrap().to_str().unwrap().to_owned();
        assert!(cc.contains("immutable"), "a normal identity response lost its caching: {cc}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A gzip variant that has gone missing must fall back to the identity blob, not 503 a request
    /// the file on disk answers perfectly. The sync deletes a `.gz` the new meta no longer declares
    /// while the process still serves the old one, so this window is a normal part of a release.
    #[tokio::test]
    async fn a_missing_gzip_variant_falls_back_to_identity() {
        let gone = std::env::temp_dir().join(format!("den-atlas-nogz-{}.gz", std::process::id()));
        let _ = std::fs::remove_file(&gone);
        let mut s = servable(true);
        s.gzip = Some((Payload::File(gone), 40));

        let resp = serve(&Method::GET, &hdrs(&[("accept-encoding", "gzip")]), s).await;
        assert_eq!(resp.status(), StatusCode::OK, "a readable identity blob was refused");
        assert!(resp.headers().get("content-encoding").is_none(), "identity bytes were labelled gzip");
        // ...and under the IDENTITY validator. Serving identity under `"<sha>-gzip"` would hand a
        // shared cache the wrong bytes for every later gzip request.
        assert_eq!(resp.headers().get("etag").unwrap(), &format!("\"{SHA}\""));
        assert_eq!(resp.headers().get("content-length").unwrap(), "1000");
        assert_eq!(body_bytes(resp).await.len(), 1000);
    }

    /// ...but a missing IDENTITY blob is still unserveable.
    #[tokio::test]
    async fn a_missing_identity_blob_is_still_unavailable_even_with_a_gzip_variant() {
        let gone = std::env::temp_dir().join(format!("den-atlas-noid-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&gone);
        let mut s = servable(true);
        s.identity = Payload::File(gone);
        let resp = serve(&Method::GET, &HeaderMap::new(), s).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// The blob is opened ONCE and the handle carries the body. Checking readability and then
    /// reopening leaves a window in which the file vanishes: the body helper's empty fallback has a
    /// size hint of 0, so hyper rewrites content-length and the response goes out as a complete,
    /// self-consistent 200 with the blob's real ETag and no bytes — measured at 3 in 900 requests
    /// against an unlink-and-recreate loop. An open descriptor keeps reading an unlinked file, so
    /// holding it removes the window entirely.
    #[tokio::test]
    async fn a_blob_unlinked_after_the_response_starts_is_still_served_whole() {
        let path = std::env::temp_dir().join(format!("den-atlas-unlink-{}.bin", std::process::id()));
        std::fs::write(&path, vec![b'x'; 1000]).unwrap();
        let mut s = servable(false);
        s.identity = Payload::File(path.clone());

        let resp = serve(&Method::GET, &HeaderMap::new(), s).await;
        assert_eq!(resp.status(), StatusCode::OK);
        // Gone before a single byte of the body is read.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            body_bytes(resp).await.len(),
            1000,
            "the body came back short after the file was unlinked mid-response"
        );
    }

    /// A 416 must not be cacheable: it is determined by the Range header, which no shared cache keys
    /// on, and it inherited the blob's year-long `immutable`.
    #[tokio::test]
    async fn an_unsatisfiable_range_is_not_cacheable() {
        let resp = serve(&Method::GET, &hdrs(&[("range", "bytes=9999-")]), servable(false)).await;
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
        let resp = serve(&Method::GET, &hdrs(&[("range", "bytes=-0")]), servable(false)).await;
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    /// A zero-byte blob is reachable — `resolve_blob` takes the on-disk length, whatever the meta
    /// declares — and every range against it must be unsatisfiable rather than underflowing `size - 1`.
    #[tokio::test]
    async fn every_range_against_an_empty_representation_is_unsatisfiable() {
        for r in ["bytes=0-0", "bytes=0-", "bytes=-1", "bytes=-0", "bytes=5-9"] {
            assert!(
                matches!(parse_range(r, 0), RangeResult::Unsatisfiable),
                "{r} was satisfiable on an empty blob"
            );
        }
        let mut s = servable(false);
        s.identity = Payload::Memory(Bytes::new());
        s.size = 0;
        let resp = serve(&Method::GET, &hdrs(&[("range", "bytes=0-0")]), s).await;
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(resp.headers().get("content-range").unwrap(), "bytes */0");
    }

    /// A malformed or multi-range header degrades to the full 200 the RFC allows, rather than a 416
    /// — a client asking for something we cannot parse still gets the representation.
    #[tokio::test]
    async fn an_unparseable_range_serves_the_whole_representation() {
        for r in ["bytes=0-10,20-30", "items=0-10", "bytes=abc-def", "garbage"] {
            let resp = serve(&Method::GET, &hdrs(&[("range", r)]), servable(false)).await;
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
    /// Ignoring it spliced two datasets' bytes together when a client resumed across a refresh —
    /// and because Range wins over gzip, a client resuming a gzip download got identity bytes
    /// spliced into a gzip stream.
    #[tokio::test]
    async fn a_stale_if_range_gets_the_whole_thing_not_a_slice() {
        let etag = format!("\"{SHA}\"");
        let range = ("range", "bytes=0-9");

        let stale =
            serve(&Method::GET, &hdrs(&[range, ("if-range", "\"an-older-dataset\"")]), servable(false)).await;
        assert_eq!(stale.status(), StatusCode::OK, "a stale If-Range still got a partial response");
        assert_eq!(body_bytes(stale).await.len(), 1000);

        let current = serve(&Method::GET, &hdrs(&[range, ("if-range", &etag)]), servable(false)).await;
        assert_eq!(current.status(), StatusCode::PARTIAL_CONTENT, "a matching If-Range was ignored");
        assert_eq!(body_bytes(current).await.len(), 10);

        // The date form, against Last-Modified.
        let by_date =
            serve(&Method::GET, &hdrs(&[range, ("if-range", LAST_MODIFIED)]), servable(false)).await;
        assert_eq!(by_date.status(), StatusCode::PARTIAL_CONTENT);
        let wrong_date = serve(
            &Method::GET,
            &hdrs(&[range, ("if-range", "Tue, 01 Jul 2025 00:00:00 GMT")]),
            servable(false),
        )
        .await;
        assert_eq!(wrong_date.status(), StatusCode::OK);

        // A weak tag never satisfies If-Range — strong comparison only.
        let weak =
            serve(&Method::GET, &hdrs(&[range, ("if-range", &format!("W/{etag}"))]), servable(false)).await;
        assert_eq!(weak.status(), StatusCode::OK, "a weak validator satisfied If-Range");

        // No If-Range at all is still an ordinary range request.
        let plain = serve(&Method::GET, &hdrs(&[range]), servable(false)).await;
        assert_eq!(plain.status(), StatusCode::PARTIAL_CONTENT);
    }

    #[tokio::test]
    async fn a_body_built_from_the_request_origin_varies_on_it() {
        let mut s = servable(false);
        s.vary_on_origin = true;
        let r = serve(&Method::GET, &hdrs(&[]), s).await;
        let vary = r.headers().get("vary").expect("no Vary at all").to_str().unwrap().to_ascii_lowercase();
        assert!(vary.contains("x-forwarded-host"), "{vary}");
        assert!(vary.contains("x-forwarded-proto"), "{vary}");
        assert!(vary.contains("host"), "{vary}");

        // ...and it still composes with the gzip variant rather than replacing it.
        let mut s = servable(true);
        s.vary_on_origin = true;
        let r = serve(&Method::GET, &hdrs(&[]), s).await;
        let vary = r.headers().get("vary").unwrap().to_str().unwrap().to_ascii_lowercase();
        assert!(vary.contains("accept-encoding") && vary.contains("x-forwarded-host"), "{vary}");
    }

    /// The 304 must carry the same Vary as the 200 it stands in for. Assembling it after the
    /// conditional early-return silently drops it, which is the shape that bit the sibling service:
    /// a revalidating cache then keeps a body built for another origin.
    #[tokio::test]
    async fn a_304_carries_the_origin_vary_too() {
        let mut s = servable(false);
        s.vary_on_origin = true;
        let etag = format!("\"{}\"", s.etag_base);
        let r = serve(&Method::GET, &hdrs(&[("if-none-match", &etag)]), s).await;
        assert_eq!(r.status(), StatusCode::NOT_MODIFIED, "the probe did not take the 304 path");
        let vary = r.headers().get("vary").expect("the 304 dropped Vary entirely");
        let vary = vary.to_str().unwrap().to_ascii_lowercase();
        assert!(vary.contains("x-forwarded-host"), "{vary}");
    }
}
