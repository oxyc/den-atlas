//! Small shared helpers — the JSON ETag hash, public_origin, plain json responses.

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;

/// Time since the FIRST CALL to this function, on the monotonic clock. Used for anything that has to
/// decide "recently?" — a wall clock steps under NTP and would answer wrongly in both directions.
///
/// First call, not process start: the `OnceLock` initialises lazily. That is fine for every use here
/// because they all compare two readings of it, and a shared base cancels — but a reader reasoning
/// about the very first recorded event needs to know the base is that event, not boot.
pub fn since_start() -> std::time::Duration {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed()
}

/// Whether it has been at least `every` since this slot last returned true; if so, claim it.
///
/// For messages on a request path. A per-request `eprintln!` is an unauthenticated amplifier: with a
/// blob unreadable, a ~30-byte request produced ~350 bytes of stderr, measured at 28k lines and
/// 4.8 MB per second on loopback — enough to fill a `json-file` log driver's disk, or to push
/// everything else out of journald's rate limiter. The condition these report is a state, not an
/// event, so one line a minute says the same thing.
pub fn log_due(slot: &std::sync::atomic::AtomicU64, every: std::time::Duration) -> bool {
    use std::sync::atomic::Ordering;
    let now = since_start().as_millis() as u64;
    let last = slot.load(Ordering::Relaxed);
    // `last == 0` is "never logged"; a real t=0 is indistinguishable and simply logs twice.
    if last != 0 && now.saturating_sub(last) < every.as_millis() as u64 {
        return false;
    }
    // A lost race just means two lines instead of one, which is not worth a lock.
    slot.store(now.max(1), Ordering::Relaxed);
    true
}

/// `eprintln!`, at most once a minute per call site, through `log_due`. For an upstream failure
/// reported on a request path: each use gets its own slot, so one noisy failure cannot hide another.
macro_rules! log_throttled {
    ($($arg:tt)*) => {{
        static SLOT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if $crate::util::log_due(&SLOT, std::time::Duration::from_secs(60)) {
            eprintln!($($arg)*);
        }
    }};
}
pub(crate) use log_throttled;

/// How long an upstream's `Retry-After` asks to be left alone: a number of seconds or an HTTP date. `None` when it
/// sends none that reads.
pub fn retry_after(headers: &HeaderMap) -> Option<std::time::Duration> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(std::time::Duration::from_secs(seconds));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(std::time::SystemTime::now()).unwrap_or_default())
}

/// How long until an upstream's allowance comes back, when its answer says none is left: the IETF draft
/// `RateLimit: "policy";r=0;t=30`, its older `RateLimit-Remaining` / `RateLimit-Reset`, or the common
/// `X-RateLimit-Remaining` / `X-RateLimit-Reset`. `None` while requests remain, or when it doesn't say. A reset
/// larger than a billion is read as a Unix time rather than seconds to wait.
pub fn exhausted_for(headers: &HeaderMap) -> Option<std::time::Duration> {
    let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
    let number = |value: &str| value.parse::<u64>().ok();
    let wait = |reset: u64| {
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        std::time::Duration::from_secs(if reset > 1_000_000_000 { reset.saturating_sub(now) } else { reset })
    };
    if let Some(field) = text("ratelimit") {
        let param = |key: &str| {
            field
                .split(';')
                .find_map(|part| part.trim().strip_prefix(key)?.strip_prefix('='))
                .and_then(number)
        };
        if param("r") == Some(0) {
            return Some(wait(param("t").unwrap_or(0)));
        }
    }
    for (remaining, reset) in
        [("ratelimit-remaining", "ratelimit-reset"), ("x-ratelimit-remaining", "x-ratelimit-reset")]
    {
        if text(remaining).and_then(number) == Some(0) {
            return Some(wait(text(reset).and_then(number).unwrap_or(0)));
        }
    }
    None
}

/// A pause on one upstream, shared by everything that asks it. A refusal (429, 5xx, no connection) starts it: for as
/// long as `Retry-After` says when the upstream sends one, else for a base that doubles with each refusal in a row.
/// While it runs every caller is turned away without asking, so a rate-limited upstream is waited out once instead
/// of being asked again by each request, row and loop tick. An answer ends it.
pub struct Backoff {
    /// When the pause ends, as `since_start` milliseconds; 0 when there is none.
    until: std::sync::atomic::AtomicU64,
    refusals: std::sync::atomic::AtomicU32,
    base: std::time::Duration,
    max: std::time::Duration,
}

impl Backoff {
    pub const fn new(base: std::time::Duration, max: std::time::Duration) -> Backoff {
        Backoff {
            until: std::sync::atomic::AtomicU64::new(0),
            refusals: std::sync::atomic::AtomicU32::new(0),
            base,
            max,
        }
    }

    /// How much of the pause is left, if one is running.
    pub fn paused(&self) -> Option<std::time::Duration> {
        let until = self.until.load(std::sync::atomic::Ordering::Relaxed);
        let now = since_start().as_millis() as u64;
        (until > now).then(|| std::time::Duration::from_millis(until - now))
    }

    /// Start a pause after a refusal, and say how long it is. `Retry-After` is honoured as sent, up to a day; without
    /// one the pause doubles per refusal in a row up to `max`, with a quarter of jitter so callers don't return as one.
    pub fn refused(&self, retry_after: Option<std::time::Duration>) -> std::time::Duration {
        use std::sync::atomic::Ordering;
        let refusals = self.refusals.fetch_add(1, Ordering::Relaxed).min(16);
        let pause = match retry_after {
            Some(asked) => asked.min(std::time::Duration::from_secs(86_400)),
            None => {
                let doubled = self.base.saturating_mul(1 << refusals).min(self.max);
                let spread = doubled.as_millis() as u64 / 4;
                let jitter = if spread == 0 { 0 } else { since_start().subsec_nanos() as u64 % spread };
                doubled + std::time::Duration::from_millis(jitter)
            }
        };
        let until = since_start().as_millis() as u64 + pause.as_millis() as u64;
        self.until.fetch_max(until.max(1), Ordering::Relaxed);
        pause
    }

    /// The upstream answered: no pause, and the next refusal starts from the base again.
    pub fn answered(&self) {
        self.refusals.store(0, std::sync::atomic::Ordering::Relaxed);
        self.until.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Whether a status is the upstream asking to be left alone for a while, rather than refusing this one request.
pub fn backs_off(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Lock a mutex, poisoned or not.
///
/// Used wherever the critical section is a short, non-unwinding map or counter update, so a poisoned
/// lock still guards usable data. Uniformly, because the alternative failed in both directions:
/// `unwrap()` inside a `Drop` that runs during unwind double-panics into an abort, while hardening
/// only that one turns "abort and restart clean" into "every request panics forever".
pub fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// FNV-1a 64-bit over the UTF-8 bytes → `<16 hex>-<byte length in hex>`, the ETag den-reel gives its JSON
/// too. Used for the small JSON and HTML responses' ETags; the big blobs use their real sha256. 32 bits made a
/// collision between two bodies of one route plausible over a long enough life — a 304 for a changed body —
/// and folding in the length costs nothing. A fixed hash, not std's `DefaultHasher`, so a validator survives
/// a restart and a toolchain upgrade.
pub fn fnv1a(input: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in input.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}-{:x}", input.len())
}

/// A plain JSON response, explicitly uncacheable (used for /health, 404, 405, and the 503
/// dataset-unavailable body). `no-store` keeps a CDN from pinning a transient error/outage past its
/// recovery — the same reason the catalog error path shortens its TTL.
pub fn json_response(body: impl Into<Body>, status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(body.into())
        .unwrap()
}

/// How long a client is asked to wait out a condition that clears on its own: an index or dataset loading, den-embed
/// waking its model, a blob mid-replacement.
pub const RELOAD_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// A 503 JSON response carrying `Retry-After`, so a client waits as long as the condition lasts instead of polling
/// into it. Short waits only: the TV app closes a whole host for as long as a Retry-After says.
pub fn unavailable_response(body: impl Into<Body>, wait: std::time::Duration) -> Response {
    let mut resp = json_response(body, StatusCode::SERVICE_UNAVAILABLE);
    let secs = wait.as_secs().max(1);
    resp.headers_mut().insert(header::RETRY_AFTER, header::HeaderValue::from(secs));
    resp
}

/// The public origin for descriptor blob URLs — `PUBLIC_BASE_URL` override, else `X-Forwarded-Proto` +
/// `X-Forwarded-Host`/`Host` (a reverse proxy sets these; on the LAN it is the request's own `Host`), else
/// `http`/`localhost`. Port of `publicOrigin`.
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
    // Coerced to exactly one of two values. It was reflected verbatim, so a header of
    // `https://evil.example/pwn?x=` was spliced straight into the advertised blob URLs — the Host
    // beside it is filtered for precisely this reason and the scheme was not.
    let proto = match first("x-forwarded-proto").as_deref() {
        Some(p) if p.eq_ignore_ascii_case("https") => "https",
        _ => "http",
    };
    // Only reflect a sane Host charset into the blob URLs we advertise (a spoofed Host would point
    // the app's fetch at an attacker origin; the checksum still gates content). PUBLIC_BASE_URL
    // short-circuits this in prod. Note the charset filter does not make a host TRUSTED — an
    // attacker-chosen name passes it — so the response must also name these headers in Vary, or a
    // shared cache hands one requester's origin to everyone.
    let host = first("x-forwarded-host")
        .or_else(|| headers.get(header::HOST).and_then(|v| v.to_str().ok()).map(|s| s.to_owned()))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The forwarded scheme was spliced verbatim into the descriptor's absolute blob URLs, so a
    /// header of `https://evil.example/pwn?x=` pointed the app's fetch at an attacker origin. The
    /// Host beside it is charset-filtered for exactly this reason; the scheme had nothing.
    #[test]
    fn a_forwarded_scheme_is_http_or_https_and_nothing_else() {
        let origin = |proto: &str| {
            let mut h = HeaderMap::new();
            h.insert("x-forwarded-proto", proto.parse().unwrap());
            h.insert(header::HOST, "atlas.local".parse().unwrap());
            public_origin(&h, None)
        };
        assert_eq!(origin("https"), "https://atlas.local");
        assert_eq!(origin("HTTPS"), "https://atlas.local", "a title-cased header downgraded to plaintext");
        assert_eq!(origin("http"), "http://atlas.local");
        for hostile in ["https://evil.example/pwn?x=", "javascript:", "://", "https evil"] {
            assert_eq!(origin(hostile), "http://atlas.local", "{hostile:?} reached the advertised blob URLs");
        }
    }

    #[test]
    fn a_json_etag_is_64_bit_fnv_and_the_length() {
        // The FNV-1a 64 reference values.
        assert_eq!(fnv1a(""), "cbf29ce484222325-0");
        assert_eq!(fnv1a("a"), "af63dc4c8601ec8c-1");
        let tag = fnv1a(r#"{"metas":[]}"#);
        let (hash, len) = tag.split_once('-').unwrap();
        assert_eq!(hash.len(), 16, "{tag}");
        assert_eq!(len, "c", "{tag}");
        assert_ne!(fnv1a(r#"{"ids":[1]}"#), fnv1a(r#"{"ids":[2]}"#));
    }

    #[test]
    fn retry_after_reads_seconds_or_a_date_and_nothing_else() {
        use std::time::{Duration, SystemTime};
        let with = |value: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::RETRY_AFTER, value.parse().unwrap());
            retry_after(&h)
        };
        assert_eq!(with("120"), Some(Duration::from_secs(120)));
        let secs =
            with(&httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(3600))).unwrap().as_secs();
        assert!((3590..=3600).contains(&secs), "an HTTP date an hour away read as {secs}s");
        let past = httpdate::fmt_http_date(SystemTime::now() - Duration::from_secs(60));
        assert_eq!(with(&past), Some(Duration::ZERO), "a date gone by is no wait, not an error");
        assert_eq!(with("soon"), None);
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn an_allowance_that_says_none_is_left_names_its_reset() {
        use std::time::{Duration, SystemTime, UNIX_EPOCH};
        let with = |pairs: &[(&'static str, String)]| {
            let mut h = HeaderMap::new();
            for (name, value) in pairs {
                h.insert(*name, value.parse().unwrap());
            }
            exhausted_for(&h)
        };
        assert_eq!(with(&[("ratelimit", r#""daily";r=0;t=90"#.into())]), Some(Duration::from_secs(90)));
        assert_eq!(with(&[("ratelimit", r#""daily";r=3;t=90"#.into())]), None, "requests remain");
        assert_eq!(
            with(&[("x-ratelimit-remaining", "0".into()), ("x-ratelimit-reset", "45".into())]),
            Some(Duration::from_secs(45))
        );
        let in_a_minute = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 60;
        let secs = with(&[("ratelimit-remaining", "0".into()), ("ratelimit-reset", in_a_minute.to_string())])
            .unwrap()
            .as_secs();
        assert!((58..=60).contains(&secs), "a Unix-time reset read as {secs}s");
        assert_eq!(with(&[("x-ratelimit-remaining", "12".into())]), None);
        assert_eq!(with(&[]), None);
    }

    #[test]
    fn a_backoff_waits_what_it_is_told_else_doubles_and_an_answer_ends_it() {
        use std::time::Duration;
        let backoff = Backoff::new(Duration::from_secs(10), Duration::from_secs(60));
        assert!(backoff.paused().is_none());
        let told = Duration::from_secs(300);
        assert_eq!(backoff.refused(Some(told)), told, "Retry-After is honoured as sent, even past the max");
        assert!(backoff.paused().is_some_and(|left| left > Duration::from_secs(290)));
        backoff.answered();
        assert!(backoff.paused().is_none(), "an answer ends the pause");
        let first = backoff.refused(None);
        let second = backoff.refused(None);
        assert!(first >= Duration::from_secs(10) && first < Duration::from_millis(12_500), "{first:?}");
        assert!(second >= Duration::from_secs(20) && second < Duration::from_secs(25), "{second:?}");
        for _ in 0..10 {
            backoff.refused(None);
        }
        assert!(
            backoff.refused(None) < Duration::from_secs(75),
            "doubling stops at the max, plus its jitter"
        );
    }
}
