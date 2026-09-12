//! Routing — the port of `handleAtlas`. A single fallback handler matches on the path (exact, like the TS),
//! so unknown paths 404 and non-GET/HEAD 405.

use crate::config::Config;
use crate::dataset::{Blob, Dataset};
use crate::descriptor::build_descriptor;
use crate::http::{serve, Payload, Servable};
use crate::manifest::manifest_json;
use crate::titles;
use crate::util::{fnv1a, json_response, public_origin};
use crate::AppState;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use std::sync::Arc;
use std::time::Instant;

/// The /configure page, embedded so the binary is self-contained. Region + provider choice is plaintext
/// (no secrets to seal); the page's JS builds the `<region>_<codes>` install URL client-side.
const CONFIGURE_PAGE: &str = include_str!("configure.html");

/// Says why an answer is not the normal one, using /health's reason slugs; absent on a normal answer.
/// The Den app reads it the same way from every addon, so a stale row can be flagged on the row itself
/// instead of the app having to poll /health and guess which rows that affects.
const DEGRADED: &str = "x-den-degraded";

/// A duration in Server-Timing's unit: milliseconds, to a tenth.
fn ms(d: std::time::Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1000.0)
}

/// Attach `Server-Timing` — how long each phase of this answer took, readable in browser devtools
/// and by the app. Phase names and durations only; nothing about what was asked.
fn with_timing(mut resp: Response, timing: &str) -> Response {
    if let Ok(v) = header::HeaderValue::from_str(timing) {
        resp.headers_mut().insert("server-timing", v);
    }
    resp
}

/// Every response leaves through here, so every one carries `Access-Control-Allow-Origin: *` — the
/// 404, a refused /metrics, a 304 and an /embed error included. Setting it per helper left out
/// whatever was built outside those helpers (the /metrics body was), and a browser reports a missing
/// header as a CORS failure rather than the status the server actually sent. The data is public and
/// credential-free, so a wildcard origin gives nothing away.
///
/// It is also where the opt-in request log is written. What the line needs is captured before routing
/// consumes the request, and only when logging is on — off, the whole cost is one bool check.
pub async fn handle(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let log = state.log_requests.then(|| {
        (std::time::Instant::now(), req.method().clone(), loggable_path(req.uri()), request_id(req.headers()))
    });
    let mut resp = route(State(state), req).await;
    resp.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, header::HeaderValue::from_static("*"));
    // The debug headers readable too: a cross-origin fetch sees only the CORS-safelisted headers unless
    // Expose-Headers names more, and Resource Timing hides Server-Timing without Timing-Allow-Origin.
    resp.headers_mut().insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        header::HeaderValue::from_static("Server-Timing, X-Den-Degraded"),
    );
    resp.headers_mut().insert("timing-allow-origin", header::HeaderValue::from_static("*"));
    if let Some((started, method, path, rid)) = log {
        eprintln!(
            "{}",
            request_line(
                &method,
                &path,
                resp.status().as_u16(),
                started.elapsed().as_millis(),
                rid.as_deref()
            )
        );
    }
    resp
}

/// `<METHOD> <path> <status> <ms>ms`, plus ` rid=<id>` when the caller sent an `X-Request-Id`.
fn request_line(method: &Method, path: &str, status: u16, ms: u128, rid: Option<&str>) -> String {
    match rid {
        Some(rid) => format!("{method} {path} {status} {ms}ms rid={rid}"),
        None => format!("{method} {path} {status} {ms}ms"),
    }
}

/// The caller's `X-Request-Id`, as a log line may carry it. The Den app sends one per addon request and
/// logs the same id, so a line on each side of a failure can be matched exactly instead of by timestamp.
/// It comes off the network, so only `[A-Za-z0-9_-]` survives and at most 32 of those: nothing in it can
/// break the line apart or forge a second one. Nothing left means no id.
fn request_id(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers.get("x-request-id")?.to_str().ok()?;
    let id: String =
        raw.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(32).collect();
    (!id.is_empty()).then_some(id)
}

/// The request path as the log may show it. A leading per-install config segment becomes `<config>`
/// — it is a user's region and service choice, which a log has no need to keep — and the query string
/// is never read, so nothing in it can reach the log. Redacted by the same `Config::parse` the router
/// uses, so exactly the segments treated as a config are the ones hidden. A search catalog's query is
/// what someone typed, so it becomes `<query>`.
fn loggable_path(uri: &axum::http::Uri) -> String {
    let path = uri.path();
    let trimmed = path.trim_start_matches('/');
    let (first, rest) = trimmed.split_once('/').map_or((trimmed, None), |(f, r)| (f, Some(r)));
    let shown = if Config::parse(first).is_none() {
        path.to_owned()
    } else {
        match rest {
            Some(r) => format!("/<config>/{r}"),
            None => "/<config>".to_owned(),
        }
    };
    redact_search(shown)
}

/// Replace the value of a `search=` path extra with `<query>`; the value ends at `&`, `/` or `.json`.
fn redact_search(path: String) -> String {
    let Some(start) = path.find("search=").map(|i| i + "search=".len()) else { return path };
    let tail = &path[start..];
    let len =
        tail.find(['&', '/']).unwrap_or_else(|| tail.strip_suffix(".json").map_or(tail.len(), str::len));
    format!("{}<query>{}", &path[..start], &tail[len..])
}

// There is deliberately NO server-side request deadline, and this is the third and last thing tried
// here. A 20s one shipped and was measured strictly worse than nothing at the load it was added for.
//
// The catalog path takes two permits in sequence (the country's package list, then the chart), and
// `Semaphore` is FIFO-fair, so every request's SECOND acquire queues behind every other request's
// first. Completions therefore cluster at the very end of the drain rather than spreading across
// it, and truncating that queue does not shed a proportional slice — it discards nearly all of the
// work. Measured at 600 concurrent cold keys: with the deadline, 0 of 600 rows served and 0 keys
// cached, having spent ~579 upstream calls (65 aborted mid-flight); without it, 600 of 600 served
// and cached, in exactly the minimum 1200 calls, p50 34s. Nothing caching also meant the next wave
// was equally cold, so the failure was self-sustaining rather than a spike.
//
// What a deadline actually protects against was already handled: hyper drops the handler future
// when the client disconnects, which releases the permit and the single-flight gate. So a client
// that has given up already stops holding a slot; the queue's length costs latency, which the
// client bounds itself, not resources. Slow-and-correct beats fast-and-empty here — and a 5xx would
// also bypass the serve-stale path that `handle_catalog` promises never returns one.
async fn route(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let method = req.method().clone();
    // CORS preflight for browser-based Stremio clients (public, credential-free data).
    if method == Method::OPTIONS {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, HEAD, POST, OPTIONS")
            .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "*")
            // Let browsers cache the preflight for a day so they stop re-preflighting every request.
            .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
            .body(Body::empty())
            .unwrap();
    }

    let path = req.uri().path().to_owned();
    // A leading `<region>_<codes>` path segment is a per-install config (Stremio config-URL pattern);
    // strip it and route on the remainder. Absent/garbled → the operator-default config. The dataset,
    // health, and blob routes are config-independent — the config only shapes manifest + catalog.
    let (config, route) = split_config(&path);
    let route = route.as_str();

    // Search query-embed proxy: POST /embed forwards the query to den-embed (the single quantizer authority)
    // and returns its int8 vector. Handled before the GET/HEAD guard because it's the one write route.
    if method == Method::POST {
        return if route == "/embed" {
            handle_embed(&state, req).await
        } else if let Some(rest) = route.strip_prefix("/index/") {
            handle_index_post(&state, rest, req).await
        } else {
            json_response(r#"{"error":"method_not_allowed"}"#, StatusCode::METHOD_NOT_ALLOWED)
        };
    }
    if method != Method::GET && method != Method::HEAD {
        return json_response(r#"{"error":"method_not_allowed"}"#, StatusCode::METHOD_NOT_ALLOWED);
    }
    let headers = req.headers().clone();
    let query = req.uri().query().unwrap_or("").to_owned();
    let origin = public_origin(&headers, state.public_base.as_deref());
    let ds = state.dataset.as_ref();

    if route == "/metrics" {
        if !crate::metrics::authorized(&headers, state.metrics_token.as_deref()) {
            return json_response(r#"{"error":"not_found"}"#, StatusCode::NOT_FOUND);
        }
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::from(crate::metrics::render(&state)))
            .unwrap();
    }
    if route == "/" || route == "/configure" || route == "/configure/" {
        return serve_html(&method, &headers, CONFIGURE_PAGE).await;
    }
    if route == "/health" {
        note_health(&state);
        // Standard Den addon health shape (ADDON-02): 200 for liveness, but report `degraded` so the
        // app's Plugins screen (and any monitor) can see a problem.
        return json_response(
            health_body(ds.is_some(), state.catalog.fresh(), state.catalog.schema_suspect()),
            StatusCode::OK,
        );
    }
    if route == "/manifest.json" {
        return serve_json(
            &method,
            &headers,
            manifest_json(&config, state.titles.is_some()),
            "public, max-age=3600, stale-while-revalidate=600",
            None,
            false,
        )
        .await;
    }
    if route == "/dataset.json" {
        return match ds {
            Some(ds) => {
                // The descriptor embeds absolute blob URLs built from the request's own
                // host/scheme, so those headers are part of what the body says.
                serve_json(
                    &method,
                    &headers,
                    build_descriptor(&origin, ds, state.embed.is_some(), state.index.is_some()),
                    "public, max-age=300",
                    ds.last_modified.clone(),
                    true,
                )
                .await
            }
            None => json_response(
                r#"{"error":"dataset_unavailable","detail":"the dataset failed to load (missing/old dataset.meta.json); refresh it with scripts/fetch-dataset.sh"}"#,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        };
    }
    // Blob routes exist only when the dataset loaded (their names come from the meta).
    if let Some(ds) = ds {
        if route == format!("/{}", ds.labels.name) {
            return serve_blob(&method, &headers, &query, ds, &ds.labels).await;
        }
        if route == format!("/{}", ds.vectors.name) {
            return serve_blob(&method, &headers, &query, ds, &ds.vectors).await;
        }
        if let Some(md) = &ds.metadata {
            if route == format!("/{}", md.name) {
                return serve_blob(&method, &headers, &query, ds, md).await;
            }
        }
        // DT-H premise index blobs.
        if let Some(pl) = &ds.premise_labels {
            if route == format!("/{}", pl.name) {
                return serve_blob(&method, &headers, &query, ds, pl).await;
            }
        }
        if let Some(pv) = &ds.premise_vectors {
            if route == format!("/{}", pv.name) {
                return serve_blob(&method, &headers, &query, ds, pv).await;
            }
        }
        // DT-I facet blob.
        if let Some(f) = &ds.facets {
            if route == format!("/{}", f.name) {
                return serve_blob(&method, &headers, &query, ds, f).await;
            }
        }
    }
    if let Some(rest) = route.strip_prefix("/catalog/") {
        return handle_catalog(&method, &headers, rest, &config, &state).await;
    }
    if let Some(rest) = route.strip_prefix("/index/") {
        return handle_index(&method, &headers, rest, &query, &state).await;
    }
    json_response(r#"{"error":"not_found"}"#, StatusCode::NOT_FOUND)
}

/// `POST /embed` — the search query-embed proxy. Forwards the JSON body (`{"text":"…"}`) to den-embed and
/// returns its response verbatim (`{"vector":int8[dims],"dims":Int,"model":String}`). den-atlas never runs
/// the model, so a query embeds through the SAME bge-m3 + int8 quantizer as the corpus (the alignment rule),
/// and den-embed stays internal. Absent `EMBED_URL` ⇒ 503 (dataset serving is unaffected).
async fn handle_embed(state: &Arc<AppState>, req: Request) -> Response {
    let started = Instant::now();
    let Some(proxy) = state.embed.as_ref() else {
        return json_response(
            r#"{"error":"embed_unavailable","detail":"search embeds are not configured (EMBED_URL unset)"}"#,
            StatusCode::SERVICE_UNAVAILABLE,
        );
    };
    // Passed on to den-embed, so its log line for this query carries the same id as the app's and ours.
    let rid = request_id(req.headers());
    // A search query is short — cap the body so this can't relay large payloads to the internal service.
    let body = match axum::body::to_bytes(req.into_body(), 64 * 1024).await {
        Ok(b) => b,
        Err(_) => return json_response(r#"{"error":"bad_request"}"#, StatusCode::BAD_REQUEST),
    };
    // Bounded, with a deadline — an unauthenticated public POST must not be able to open as many
    // concurrent model invocations as a client cares to make, and a queue without a deadline is just
    // a slower way of failing.
    let Ok(Ok(_permit)) =
        tokio::time::timeout(crate::EMBED_WAIT, proxy.inflight.clone().acquire_owned()).await
    else {
        return json_response(
            r#"{"error":"embed_busy","detail":"too many concurrent embeds; retry shortly"}"#,
            StatusCode::SERVICE_UNAVAILABLE,
        );
    };
    let upstream = Instant::now();
    let mut upstream_req =
        proxy.client.post(format!("{}/embed", proxy.base)).header(header::CONTENT_TYPE, "application/json");
    if let Some(rid) = &rid {
        upstream_req = upstream_req.header("x-request-id", rid.as_str());
    }
    let resp = match upstream_req.body(body).send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let bytes = resp.bytes().await.unwrap_or_default();
            Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                // A POST proxy of per-query vectors — never let a heuristic/intermediary cache these.
                .header(header::CACHE_CONTROL, "no-store")
                .body(Body::from(bytes))
                .unwrap()
        }
        Err(e) => {
            crate::util::log_throttled!("embed upstream error: {e}");
            json_response(r#"{"error":"embed_upstream_failed"}"#, StatusCode::BAD_GATEWAY)
        }
    };
    with_timing(resp, &format!("embed;dur={}, total;dur={}", ms(upstream.elapsed()), ms(started.elapsed())))
}

/// What `/health` reports: `None` when healthy, else the reason slug and a one-sentence detail.
/// Dataset-unavailable outranks a stale catalog (no dataset is the more severe condition): no dataset ⇒
/// `dataset_unavailable`; else a failed last JustWatch refresh ⇒ `stale_catalog`. One function feeds
/// both the body and the state-change log line, so the two cannot disagree.
pub(crate) fn health_state(
    dataset_loaded: bool,
    catalog_fresh: bool,
    schema_suspect: bool,
) -> Option<(&'static str, &'static str)> {
    if !dataset_loaded {
        Some(("dataset_unavailable", "dataset failed to load; refresh with scripts/fetch-dataset.sh"))
    } else if !catalog_fresh {
        Some(("stale_catalog", "last JustWatch refresh failed; serving stale catalog"))
    } else if schema_suspect {
        // Ranks below the two above: those mean rows are missing, this means rows are SHORT. It has
        // to be here at all because a partial break is otherwise invisible — a chart that comes back
        // with a fifth of its titles is a successful refresh by every other measure, caches as
        // complete for the full TTL, and serves with an hour of max-age.
        Some((
            "catalog_schema_suspect",
            "a JustWatch chart returned far fewer usable titles than it carried; rows may be short",
        ))
    } else {
        None
    }
}

/// The `/health` JSON body (ADDON-02). Always paired with 200 + `no-store` — liveness never fails; the
/// body carries the real state. Pure, so the decision is unit-testable without an HTTP round-trip.
fn health_body(dataset_loaded: bool, catalog_fresh: bool, schema_suspect: bool) -> String {
    match health_state(dataset_loaded, catalog_fresh, schema_suspect) {
        None => r#"{"status":"ok"}"#.to_owned(),
        Some((reason, detail)) => {
            format!(r#"{{"status":"degraded","reason":"{reason}","detail":"{detail}"}}"#)
        }
    }
}

/// Log `/health`'s answer when it changes, and only then: one line when the addon goes degraded (with
/// the reason), one when it recovers. The state is what an operator needs; a line per request would
/// repeat it thousands of times. Checked where the answer can move — after a catalog refresh — and
/// where it is read, so a schema-break window that has quietly expired is reported the next time
/// anyone looks, with no timer running.
fn note_health(state: &AppState) {
    let now = health_state(state.dataset.is_some(), state.catalog.fresh(), state.catalog.schema_suspect());
    let slug = now.map_or("ok", |(reason, _)| reason);
    let mut last = crate::util::lock(&state.health);
    if *last == slug {
        return;
    }
    match now {
        Some((reason, detail)) => eprintln!("health degraded: {reason} — {detail}"),
        None => eprintln!("health recovered (was {})", *last),
    }
    *last = slug;
}

/// `GET /catalog/{type}/{id}[/{extra}].json`. The optional extra may carry `country=XX` — the region
/// the Den app forwards for an `auto` install. Public, tokenless; a JustWatch failure degrades to empty
/// rows, never a 5xx, and never touches the dataset.
async fn handle_catalog(
    method: &Method,
    headers: &axum::http::HeaderMap,
    rest: &str,
    config: &Config,
    state: &Arc<AppState>,
) -> Response {
    let started = Instant::now();
    let rest = rest.strip_suffix(".json").unwrap_or(rest);
    let mut parts = rest.splitn(3, '/'); // type / id / optional extra
    let type_ = parts.next().unwrap_or("");
    let id = parts.next().unwrap_or("");
    let extra = parts.next().unwrap_or("");
    if id == titles::CATALOG_ID {
        return handle_title_search(method, headers, type_, extra, state).await;
    }
    // A fixed-country config wins; an `auto` config takes the forwarded `country` extra; else default.
    let forwarded = extra_value(extra, "country");
    let country = config.country(forwarded.as_deref(), &state.default_country);
    let answer = state.catalog.metas_json(id, type_, &country, &config.providers).await;
    // A refresh is what moves the catalog's health, so a change is noticed here as it happens.
    note_health(state);
    match answer {
        Some(r) => {
            // Fresh/stale-good rows cache for an hour; an outage-empty/stale fallback caches briefly so a
            // CDN doesn't pin a broken row past JustWatch's recovery.
            let cc = if r.fresh {
                "public, max-age=3600, stale-while-revalidate=600, stale-if-error=86400"
            } else {
                "public, max-age=60"
            };
            // The phase that produced the row: a JustWatch refresh (timed), or the cache hit that
            // avoided one — and, when the refresh failed, that the body is the last-good copy.
            let mut timing = match r.upstream {
                Some(d) => format!("justwatch;dur={}", ms(d)),
                None => "cache;desc=hit".to_owned(),
            };
            if r.stale {
                timing.push_str(", cache;desc=stale");
            }
            let mut resp = serve_json(method, headers, r.body, cc, None, false).await;
            // Not fresh is the last-good copy or an empty fallback after a failed refresh — the
            // state /health calls `stale_catalog`, so the row carries the same slug.
            if !r.fresh {
                resp.headers_mut().insert(DEGRADED, header::HeaderValue::from_static("stale_catalog"));
            }
            with_timing(resp, &format!("{timing}, total;dur={}", ms(started.elapsed())))
        }
        None => json_response(r#"{"error":"not_found"}"#, StatusCode::NOT_FOUND),
    }
}

/// A label row's page size when the caller doesn't say, and the most one page returns.
const ROW_PAGE: usize = 24;
const MAX_ROW_PAGE: usize = 100;

/// One `/index/…` question, parsed before the index loads, so a malformed path never pays for a load.
enum IndexQuestion {
    Taxonomy,
    Rows {
        media_type: den_index::MediaType,
        mood: bool,
        label: String,
    },
    Similar {
        media_type: den_index::MediaType,
        tmdb_id: u32,
    },
    Neighbours {
        media_type: den_index::MediaType,
        tmdb_id: u32,
    },
    /// Answered in `handle_index`, because it waits on den-embed.
    Search,
    /// Answered in `handle_index`, because a leftover theme waits on den-embed.
    Facets,
}

impl IndexQuestion {
    fn parse(route: &str) -> Option<Self> {
        let parts: Vec<&str> = route.split('/').collect();
        match parts.as_slice() {
            ["taxonomy"] => Some(Self::Taxonomy),
            ["rows", type_, family, label] => {
                let mood = match *family {
                    "subgenre" => false,
                    "mood" => true,
                    _ => return None,
                };
                Some(Self::Rows { media_type: index_media_type(type_)?, mood, label: percent_decode(label) })
            }
            ["similar", type_, id] => {
                Some(Self::Similar { media_type: index_media_type(type_)?, tmdb_id: id.parse().ok()? })
            }
            ["neighbours", type_, id] => {
                Some(Self::Neighbours { media_type: index_media_type(type_)?, tmdb_id: id.parse().ok()? })
            }
            ["search"] => Some(Self::Search),
            ["facets"] => Some(Self::Facets),
            _ => None,
        }
    }

    fn answer(&self, indexes: &crate::queries::Indexes, query: &str) -> String {
        let plot = &indexes.plot;
        let body = match self {
            Self::Taxonomy => serde_json::json!({
                "taxonomyVersion": plot.taxonomy_version(),
                "subgenres": plot.subgenre_labels(),
                "moods": plot.mood_labels(),
            }),
            Self::Rows { media_type, mood, label } => {
                let number = |key: &str, default: usize| {
                    query_param(query, key).and_then(|v| v.parse().ok()).unwrap_or(default)
                };
                let (skip, limit) = (number("skip", 0), number("limit", ROW_PAGE).min(MAX_ROW_PAGE));
                let floor = den_index::DISPLAY_CONFIDENCE_FLOOR;
                let titles = if *mood {
                    plot.titles_with_mood(label, Some(*media_type), floor, skip, limit)
                } else {
                    plot.titles_with_subgenre(label, Some(*media_type), floor, skip, limit)
                };
                serde_json::json!({ "ids": titles.iter().map(|&(id, _)| id).collect::<Vec<u32>>() })
            }
            Self::Similar { media_type, tmdb_id } => serde_json::json!({
                "ids": den_index::more_like_this(Some(plot), indexes.premise.as_ref(), *tmdb_id, *media_type),
            }),
            // The plain plot neighbours the tvOS app splices in after an exact title match.
            Self::Neighbours { media_type, tmdb_id } => {
                let k = query_param(query, "k")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(NEIGHBOUR_K)
                    .min(MAX_NEIGHBOUR_K);
                let ids: Vec<u32> =
                    plot.nearest(*tmdb_id, *media_type, k).iter().map(|n| n.tmdb_id).collect();
                serde_json::json!({ "ids": ids })
            }
            Self::Search | Self::Facets => unreachable!("answered in handle_index"),
        };
        body.to_string()
    }
}

/// A Stremio type in a path, as the index names it.
fn index_media_type(type_: &str) -> Option<den_index::MediaType> {
    match type_ {
        "movie" => Some(den_index::MediaType::Movie),
        "series" => Some(den_index::MediaType::Tv),
        _ => None,
    }
}

/// How many titles semantic search returns (the tvOS app's own `k`); plain neighbours by default and at most;
/// the facet lane's cap; pooled suggestions by default.
const SEMANTIC_K: usize = 24;
const NEIGHBOUR_K: usize = 12;
const MAX_NEIGHBOUR_K: usize = 50;
const FACET_LIMIT: usize = 50;
const SUGGEST_LIMIT: usize = 20;
/// The most titles one POST may name, and the most seeds a suggestion takes (the tvOS app's own cap).
const MAX_TITLES: usize = 500;
const MAX_SEEDS: usize = 8;

fn stremio_type(media_type: den_index::MediaType) -> &'static str {
    match media_type {
        den_index::MediaType::Movie => "movie",
        den_index::MediaType::Tv => "series",
    }
}

/// Titles of mixed types, as `[{"type","id"}]`.
fn titles_json(titles: &[(u32, den_index::MediaType)]) -> serde_json::Value {
    titles
        .iter()
        .map(|&(id, media_type)| serde_json::json!({ "type": stremio_type(media_type), "id": id }))
        .collect()
}

/// A query-string value as text: `+` is a space there (a browser's URLSearchParams writes one), then `%XX`
/// escapes.
fn query_text(query: &str, key: &str) -> String {
    query_param(query, key).map(|v| percent_decode(&v.replace('+', " "))).unwrap_or_default()
}

/// Embed text through den-embed the way `/embed` does — the corpus's own model and quantiser — under the same
/// permits and deadline.
async fn embed_query(state: &AppState, text: &str) -> Result<Vec<i8>, String> {
    #[derive(serde::Deserialize)]
    struct Embedded {
        vector: Vec<i8>,
    }
    let proxy = state.embed.as_ref().ok_or("EMBED_URL is unset")?;
    let _permit = tokio::time::timeout(crate::EMBED_WAIT, proxy.inflight.clone().acquire_owned())
        .await
        .map_err(|_| "den-embed is busy")?
        .map_err(|_| "den-embed permits are closed")?;
    let resp = proxy
        .client
        .post(format!("{}/embed", proxy.base))
        .json(&serde_json::json!({ "text": text }))
        .send()
        .await
        .map_err(|e| format!("den-embed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("den-embed answered HTTP {}", resp.status()));
    }
    resp.json::<Embedded>().await.map(|e| e.vector).map_err(|e| format!("den-embed body: {e}"))
}

/// Semantic search in one request: embed the query, then the plot index's nearest titles to it — the tvOS
/// app's `semanticSearch`. The error is why den-embed couldn't answer.
async fn search_answer(
    state: &AppState,
    indexes: &crate::queries::Indexes,
    query: &str,
) -> Result<String, String> {
    let text = query_text(query, "q");
    if text.trim().chars().count() < 2 {
        return Ok(serde_json::json!({ "titles": [] }).to_string());
    }
    let media_type = query_param(query, "type").and_then(|t| index_media_type(&t));
    let vector = embed_query(state, &text).await?;
    let titles: Vec<(u32, den_index::MediaType)> = indexes
        .plot
        .nearest_to_vector(&vector, media_type, SEMANTIC_K)
        .into_iter()
        .map(|n| (n.tmdb_id, n.media_type))
        .collect();
    Ok(serde_json::json!({ "titles": titles_json(&titles) }).to_string())
}

/// The facet lane — the tvOS app's facet search: titles matching the query's country, decade and type,
/// most-voted first, with any leftover words ranked semantically to the front. `facet` is null when the query
/// names none. Without den-embed the matches still come back, unranked.
async fn facets_answer(state: &AppState, indexes: &crate::queries::Indexes, query: &str) -> String {
    let facet = den_index::FacetQuery::parse(&query_text(query, "q"));
    let (Some(facets), true) = (indexes.facets.as_ref(), facet.has_facet()) else {
        return serde_json::json!({ "facet": null, "titles": [] }).to_string();
    };
    let mut titles = facets.filter(facet.media_type, facet.country, facet.decade);
    if !facet.leftover.is_empty() && !titles.is_empty() {
        match embed_query(state, &facet.leftover).await {
            Ok(vector) => {
                let matched: std::collections::HashSet<_> = titles.iter().copied().collect();
                let head: Vec<_> = indexes
                    .plot
                    .nearest_to_vector(&vector, None, SEMANTIC_K)
                    .into_iter()
                    .map(|n| (n.tmdb_id, n.media_type))
                    .filter(|t| matched.contains(t))
                    .collect();
                let lifted: std::collections::HashSet<_> = head.iter().copied().collect();
                titles = head.into_iter().chain(titles.into_iter().filter(|t| !lifted.contains(t))).collect();
            }
            Err(e) => eprintln!("facet leftover left unranked: {e}"),
        }
    }
    titles.truncate(FACET_LIMIT);
    serde_json::json!({
        "facet": {
            "mediaType": facet.media_type.map(stremio_type),
            "country": facet.country,
            "decade": facet.decade,
            "leftover": facet.leftover,
        },
        "titles": titles_json(&titles),
    })
    .to_string()
}

/// A title named in a POST body: `{"type":"movie"|"series","id":…}`.
#[derive(serde::Deserialize)]
struct TitleRef {
    #[serde(rename = "type")]
    type_: String,
    id: u32,
}

impl TitleRef {
    fn key(&self) -> Option<(u32, den_index::MediaType)> {
        Some((self.id, index_media_type(&self.type_)?))
    }
}

fn keys(titles: &[TitleRef]) -> Vec<(u32, den_index::MediaType)> {
    titles.iter().filter_map(TitleRef::key).collect()
}

fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, String> {
    serde_json::from_slice(body).map_err(|e| e.to_string())
}

fn at_most(titles: &[TitleRef], max: usize, what: &str) -> Result<(), String> {
    if titles.len() > max {
        return Err(format!("at most {max} {what}"));
    }
    Ok(())
}

/// `{"titles":[…]}` → each title's labels (null when the index doesn't hold it), for the top-genres rollup and
/// More Like This's theme rerank.
fn answer_labels(indexes: &crate::queries::Indexes, body: &[u8]) -> Result<serde_json::Value, String> {
    #[derive(serde::Deserialize)]
    struct Body {
        titles: Vec<TitleRef>,
    }
    let body: Body = parse_body(body)?;
    at_most(&body.titles, MAX_TITLES, "titles")?;
    let labels: Vec<serde_json::Value> = body
        .titles
        .iter()
        .map(|t| {
            t.key().and_then(|(id, media_type)| indexes.plot.labels(id, media_type)).map_or(
                serde_json::Value::Null,
                |l| {
                    serde_json::json!({
                        "primaryGenre": l.primary_genre,
                        "animated": l.animated,
                        "subgenres": l.subgenres,
                        "moods": l.moods,
                    })
                },
            )
        })
        .collect();
    Ok(serde_json::json!({ "labels": labels }))
}

/// `{"space"?,"liked","disliked","candidates"}` → per candidate, its taste boost toward the liked titles and
/// its closeness to the disliked ones — both the tvOS app's `TasteVector.boost` (cosine to the centroid,
/// clamped at 0; 0 when the index lacks the title). The client applies its weights. `space` is `plot` (the
/// browse tilt) or `premise` (More Like This; plot when the dataset has no premise index).
fn answer_score(indexes: &crate::queries::Indexes, body: &[u8]) -> Result<serde_json::Value, String> {
    #[derive(serde::Deserialize)]
    struct Body {
        #[serde(default)]
        space: Option<String>,
        #[serde(default)]
        liked: Vec<TitleRef>,
        #[serde(default)]
        disliked: Vec<TitleRef>,
        candidates: Vec<TitleRef>,
    }
    let body: Body = parse_body(body)?;
    for (list, what) in
        [(&body.liked, "liked"), (&body.disliked, "disliked"), (&body.candidates, "candidates")]
    {
        at_most(list, MAX_TITLES, what)?;
    }
    let (space, index) = match body.space.as_deref() {
        None | Some("plot") => ("plot", &indexes.plot),
        Some("premise") => indexes.premise.as_ref().map_or(("plot", &indexes.plot), |p| ("premise", p)),
        Some(other) => return Err(format!("unknown space {other:?}")),
    };
    let liked = index.centroid(&keys(&body.liked));
    let disliked = index.centroid(&keys(&body.disliked));
    let boost = |centroid: &Option<Vec<f64>>, (id, media_type): (u32, den_index::MediaType)| {
        centroid.as_ref().map_or(0.0, |c| index.taste_boost(id, media_type, c))
    };
    let scores: Vec<serde_json::Value> = body
        .candidates
        .iter()
        .map(|t| match t.key() {
            Some(key) => serde_json::json!({ "taste": boost(&liked, key), "dislike": boost(&disliked, key) }),
            None => serde_json::json!({ "taste": 0.0, "dislike": 0.0 }),
        })
        .collect();
    Ok(serde_json::json!({ "space": space, "scores": scores }))
}

/// `{"seeds","exclude"?,"limit"?}` → More Like This for each seed (ids of the seed's type), minus the seeds and
/// the excluded titles, and those lists pooled in seed order — the atlas half of Because you watched and You
/// Might Also Like; the client blends in TMDB's.
fn answer_suggest(indexes: &crate::queries::Indexes, body: &[u8]) -> Result<serde_json::Value, String> {
    #[derive(serde::Deserialize)]
    struct Body {
        seeds: Vec<TitleRef>,
        #[serde(default)]
        exclude: Vec<TitleRef>,
        #[serde(default)]
        limit: Option<usize>,
    }
    let body: Body = parse_body(body)?;
    at_most(&body.seeds, MAX_SEEDS, "seeds")?;
    at_most(&body.exclude, MAX_TITLES, "excluded titles")?;
    let seeds = keys(&body.seeds);
    let mut excluded: std::collections::HashSet<_> = keys(&body.exclude).into_iter().collect();
    excluded.extend(seeds.iter().copied());
    let limit = body.limit.unwrap_or(SUGGEST_LIMIT).min(MAX_TITLES);
    let per_seed: Vec<((u32, den_index::MediaType), Vec<u32>)> = seeds
        .iter()
        .map(|&(id, media_type)| {
            let similar =
                den_index::more_like_this(Some(&indexes.plot), indexes.premise.as_ref(), id, media_type);
            (
                (id, media_type),
                similar.into_iter().filter(|&n| !excluded.contains(&(n, media_type))).collect(),
            )
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut pooled = Vec::new();
    'pool: for &((_, media_type), ref ids) in &per_seed {
        for &id in ids {
            if seen.insert((id, media_type)) {
                pooled.push((id, media_type));
                if pooled.len() == limit {
                    break 'pool;
                }
            }
        }
    }
    let per_seed: Vec<serde_json::Value> = per_seed
        .iter()
        .map(|(seed, ids)| serde_json::json!({ "seed": titles_json(&[*seed])[0], "ids": ids }))
        .collect();
    Ok(serde_json::json!({ "perSeed": per_seed, "pooled": titles_json(&pooled) }))
}

/// `POST /index/labels|score|suggest` — the questions that name many titles at once. JSON in, JSON out,
/// uncached. Off (404) unless `INDEX_QUERIES` is set; a malformed body is a 400.
async fn handle_index_post(state: &Arc<AppState>, rest: &str, req: Request) -> Response {
    let started = Instant::now();
    let question = rest.strip_suffix(".json").unwrap_or(rest);
    let (Some(queries), true) = (state.index.as_ref(), matches!(question, "labels" | "score" | "suggest"))
    else {
        return json_response(r#"{"error":"not_found"}"#, StatusCode::NOT_FOUND);
    };
    let Ok(body) = axum::body::to_bytes(req.into_body(), 64 * 1024).await else {
        return json_response(r#"{"error":"bad_request"}"#, StatusCode::BAD_REQUEST);
    };
    let (indexes, loaded_in) = match queries.get().await {
        Ok(got) => got,
        Err(e) => {
            eprintln!("index load failed: {e}");
            return json_response(r#"{"error":"index_unavailable"}"#, StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    let answer = match question {
        "labels" => answer_labels(&indexes, &body),
        "score" => answer_score(&indexes, &body),
        _ => answer_suggest(&indexes, &body),
    };
    let resp = match answer {
        Ok(value) => json_response(value.to_string(), StatusCode::OK),
        Err(detail) => json_response(
            serde_json::json!({ "error": "bad_request", "detail": detail }).to_string(),
            StatusCode::BAD_REQUEST,
        ),
    };
    let load = loaded_in.map(|d| format!("load;dur={}, ", ms(d))).unwrap_or_default();
    with_timing(resp, &format!("{load}total;dur={}", ms(started.elapsed())))
}

/// `/index/…` — TMDB ids only; the Den apps hydrate titles themselves. Off (404) unless `INDEX_QUERIES` is
/// set. The indexes load on the first query, and that answer's `Server-Timing` says how long it took.
async fn handle_index(
    method: &Method,
    headers: &axum::http::HeaderMap,
    rest: &str,
    query: &str,
    state: &Arc<AppState>,
) -> Response {
    let started = Instant::now();
    let not_found = || json_response(r#"{"error":"not_found"}"#, StatusCode::NOT_FOUND);
    let Some(queries) = state.index.as_ref() else { return not_found() };
    let Some(question) = IndexQuestion::parse(rest.strip_suffix(".json").unwrap_or(rest)) else {
        return not_found();
    };
    let (indexes, loaded_in) = match queries.get().await {
        Ok(got) => got,
        Err(e) => {
            eprintln!("index load failed: {e}");
            return json_response(r#"{"error":"index_unavailable"}"#, StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    // Search and facets can rank through den-embed, so they stay short. Every other answer is the dataset
    // alone, which changes at most once a day: fresh for an hour, and served stale while it revalidates.
    let cache_control = if matches!(question, IndexQuestion::Search | IndexQuestion::Facets) {
        "public, max-age=300"
    } else {
        "public, max-age=3600, stale-while-revalidate=86400"
    };
    let body = match question {
        IndexQuestion::Search => match search_answer(state, &indexes, query).await {
            Ok(body) => body,
            Err(e) => {
                eprintln!("semantic search unavailable: {e}");
                return json_response(r#"{"error":"embed_unavailable"}"#, StatusCode::SERVICE_UNAVAILABLE);
            }
        },
        IndexQuestion::Facets => facets_answer(state, &indexes, query).await,
        question => question.answer(&indexes, query),
    };
    let load = loaded_in.map(|d| format!("load;dur={}, ", ms(d))).unwrap_or_default();
    let resp = serve_json(method, headers, body, cache_control, None, false).await;
    with_timing(resp, &format!("{load}total;dur={}", ms(started.elapsed())))
}

/// `GET /catalog/{movie|series}/den-titles/search={q}.json` — fuzzy title search, answered from memory.
/// Until the first index lands it answers empty with `X-Den-Degraded: title_index_building`; with
/// `TITLE_SEARCH` off the catalog doesn't exist.
async fn handle_title_search(
    method: &Method,
    headers: &axum::http::HeaderMap,
    type_: &str,
    extra: &str,
    state: &Arc<AppState>,
) -> Response {
    let started = Instant::now();
    let media_type = match type_ {
        "movie" => den_titlesearch::MediaType::Movie,
        "series" => den_titlesearch::MediaType::Tv,
        _ => return json_response(r#"{"error":"not_found"}"#, StatusCode::NOT_FOUND),
    };
    let Some(search) = state.titles.as_ref() else {
        return json_response(r#"{"error":"not_found"}"#, StatusCode::NOT_FOUND);
    };
    let Some(index) = search.index() else {
        let mut resp =
            serve_json(method, headers, r#"{"metas":[]}"#.to_owned(), "no-store", None, false).await;
        resp.headers_mut().insert(DEGRADED, header::HeaderValue::from_static("title_index_building"));
        return resp;
    };
    let query = extra_value(extra, "search").map(|q| percent_decode(&q)).unwrap_or_default();
    let body = titles::metas_json(&index, &query, media_type);
    let searched = started.elapsed();
    let resp =
        serve_json(method, headers, body, "public, max-age=3600, stale-while-revalidate=600", None, false)
            .await;
    with_timing(resp, &format!("titles;dur={}, total;dur={}", ms(searched), ms(started.elapsed())))
}

/// The embedded landing/configure page — served through the conditional layer so it gets a strong ETag
/// + `If-None-Match`/304 for free, plus a modest TTL (the page changes only on redeploy).
async fn serve_html(method: &Method, headers: &axum::http::HeaderMap, html: &'static str) -> Response {
    let etag = fnv1a(html);
    let bytes = Bytes::from_static(html.as_bytes());
    let size = bytes.len() as u64;
    serve(
        method,
        headers,
        Servable {
            etag_base: etag,
            content_type: "text/html; charset=utf-8".to_owned(),
            cache_control: "public, max-age=3600, stale-while-revalidate=600".to_owned(),
            last_modified: None,
            size,
            identity: Payload::Memory(bytes),
            gzip: None,
            vary_on_origin: false,
        },
    )
    .await
}

async fn serve_json(
    method: &Method,
    headers: &axum::http::HeaderMap,
    body: String,
    cache_control: &str,
    last_modified: Option<String>,
    vary_on_origin: bool,
) -> Response {
    let etag = fnv1a(&body);
    let bytes = Bytes::from(body.into_bytes());
    let size = bytes.len() as u64;
    serve(
        method,
        headers,
        Servable {
            etag_base: etag,
            content_type: "application/json".to_owned(),
            cache_control: cache_control.to_owned(),
            last_modified,
            size,
            identity: Payload::Memory(bytes),
            gzip: None,
            vary_on_origin,
        },
    )
    .await
}

async fn serve_blob(
    method: &Method,
    headers: &axum::http::HeaderMap,
    query: &str,
    ds: &Dataset,
    blob: &Blob,
) -> Response {
    let started = Instant::now();
    // `?v=<current datasetVersion>` ⇒ immutable for a year; a bare request revalidates.
    let pinned = query_param(query, "v").as_deref() == Some(ds.meta.dataset_version.as_str());
    let cache_control =
        if pinned { "public, max-age=31536000, immutable" } else { "public, max-age=3600" }.to_owned();
    let gzip = blob.gz.as_ref().map(|g| (Payload::VerifiedFile(g.path.clone(), g.identity.clone()), g.size));
    let resp = serve(
        method,
        headers,
        Servable {
            etag_base: blob.sha256.clone(),
            content_type: blob.content_type.to_owned(),
            cache_control,
            last_modified: ds.last_modified.clone(),
            size: blob.size,
            identity: Payload::VerifiedFile(blob.path.clone(), blob.identity.clone()),
            gzip,
            vary_on_origin: false, // a blob body carries no origin-derived URLs
        },
    )
    .await;
    // A blob has one phase — the body streams after this, so `total` is time to the response head.
    with_timing(resp, &format!("total;dur={}", ms(started.elapsed())))
}

/// First value of `key` in a `k=v&k2=v2` query string (the datasetVersion is hex, so no percent-decoding).
fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let mut it = kv.splitn(2, '=');
        (it.next()? == key).then(|| it.next().unwrap_or("").to_owned())
    })
}

/// Split an optional leading `<region>_<codes>` config segment off the path — returning the parsed
/// config (or the operator default) and the remaining route with a leading `/`.
fn split_config(path: &str) -> (Config, String) {
    let trimmed = path.trim_start_matches('/');
    let mut it = trimmed.splitn(2, '/');
    let first = it.next().unwrap_or("");
    if let Some(cfg) = Config::parse(first) {
        (cfg, format!("/{}", it.next().unwrap_or("")))
    } else {
        (Config::default_config(), path.to_owned())
    }
}

/// First non-empty value of `key` in a Stremio path-extra like `country=SE&genre=Action`.
fn extra_value(extra: &str, key: &str) -> Option<String> {
    extra.split('&').find_map(|kv| {
        let mut it = kv.splitn(2, '=');
        (it.next()? == key).then(|| it.next().unwrap_or("").to_owned()).filter(|v| !v.is_empty())
    })
}

/// Decode `%XX` escapes in a path-extra value (a search query arrives encoded); a malformed escape is
/// kept as written.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(high), Some(low)) => {
                out.push((high << 4) | low);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The request log must not keep an install's config segment or anything from the query.
    #[test]
    fn a_logged_path_hides_the_install_config_and_the_query() {
        let p = |s: &str| loggable_path(&s.parse::<axum::http::Uri>().unwrap());
        assert_eq!(p("/US_nfx-mxx/catalog/movie/jw-nfx.json?x=1"), "/<config>/catalog/movie/jw-nfx.json");
        assert_eq!(p("/auto_nfx/manifest.json"), "/<config>/manifest.json");
        assert_eq!(p("/auto_nfx"), "/<config>");
        assert_eq!(p("/labels.json?v=abc&token=s3cret"), "/labels.json");
        assert_eq!(p("/health"), "/health");
        // Not a config (no valid region), so the router treats it as a path and so does the log.
        assert_eq!(p("/zz9_nfx/manifest.json"), "/zz9_nfx/manifest.json");
    }

    fn rid_of(value: &str) -> Option<String> {
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-request-id", axum::http::HeaderValue::from_str(value).unwrap());
        request_id(&h)
    }

    #[test]
    fn a_request_line_carries_the_callers_request_id() {
        let line =
            request_line(&Method::GET, "/<config>/catalog/movie/jw-nfx.json", 200, 12, Some("a1b2c3d4"));
        assert_eq!(line, "GET /<config>/catalog/movie/jw-nfx.json 200 12ms rid=a1b2c3d4");
        assert_eq!(rid_of("a1b2-c3_d4").as_deref(), Some("a1b2-c3_d4"));
    }

    #[test]
    fn a_request_line_without_an_id_is_unchanged() {
        assert_eq!(request_line(&Method::HEAD, "/health", 200, 0, None), "HEAD /health 200 0ms");
        assert_eq!(request_id(&axum::http::HeaderMap::new()), None);
    }

    /// The id comes off the network and lands in a log line: nothing may split the line, forge a second
    /// field, or grow it without bound. A value with nothing usable is no id at all.
    #[test]
    fn a_hostile_request_id_is_sanitized_and_truncated() {
        assert_eq!(rid_of("ab cd\tef rid=x").as_deref(), Some("abcdefridx"));
        assert_eq!(rid_of(&"z".repeat(100)).as_deref(), Some("z".repeat(32).as_str()));
        assert_eq!(rid_of("!!! ;;; "), None);
    }

    /// The proxy passes the id on, so den-embed's line for the query can be matched to this one.
    #[tokio::test]
    async fn the_embed_proxy_forwards_the_request_id() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 16 * 1024];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                let body = r#"{"vector":[],"dims":0,"model":"m"}"#;
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        let state = Arc::new(AppState {
            embed: Some(crate::EmbedProxy {
                client: reqwest::Client::new(),
                base: format!("http://{addr}"),
                inflight: Arc::new(tokio::sync::Semaphore::new(1)),
            }),
            ..AppState::for_test(None)
        });
        let req = HttpRequest::builder()
            .method("POST")
            .uri("/embed")
            .header("x-request-id", "q7-search_1 junk")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"text":"heist"}"#))
            .unwrap();
        let resp = handle(State(state), req).await;
        assert_eq!(resp.status(), 200);
        let upstream_head = rx.await.unwrap().to_ascii_lowercase();
        assert!(upstream_head.contains("x-request-id: q7-search_1junk\r\n"), "{upstream_head}");
    }

    #[test]
    fn health_ok_when_dataset_loaded_and_fresh() {
        assert_eq!(health_body(true, true, false), r#"{"status":"ok"}"#);
    }

    #[test]
    fn health_stale_catalog_when_last_refresh_failed() {
        let body = health_body(true, false, false);
        assert!(body.contains(r#""status":"degraded""#));
        assert!(body.contains(r#""reason":"stale_catalog""#));
    }

    /// A partial schema break is a SUCCESSFUL refresh by every other measure — the row is short but
    /// non-empty, so it caches as complete and `fresh()` stays true. Without this state the only
    /// trace was a line on stderr that nothing reads.
    #[test]
    fn health_reports_a_suspected_schema_break() {
        let body = health_body(true, true, true);
        assert!(body.contains(r#""status":"degraded""#), "{body}");
        assert!(body.contains(r#""reason":"catalog_schema_suspect""#), "{body}");
        // It ranks BELOW the two that mean rows are missing entirely.
        assert!(health_body(true, false, true).contains(r#""reason":"stale_catalog""#));
        assert!(health_body(false, true, true).contains(r#""reason":"dataset_unavailable""#));
    }

    #[test]
    fn health_dataset_unavailable_when_dataset_missing() {
        // Dataset-unavailable outranks stale: even with a fresh catalog, no dataset is the reported reason.
        let body = health_body(false, true, false);
        assert!(body.contains(r#""reason":"dataset_unavailable""#));
        // …and it still takes precedence when the catalog is also stale (the more severe condition wins).
        assert!(health_body(false, false, true).contains(r#""reason":"dataset_unavailable""#));
    }

    /// The descriptor's Vary depends on ONE bool at its call site, and flipping it left the whole
    /// suite green — the fix was in `http.rs` with nothing checking it was actually wired up. This
    /// goes through `handle`, so the route, the flag and the header are all on the hook.
    #[tokio::test]
    async fn the_descriptor_route_varies_on_the_origin_it_embeds() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;

        let dir = std::env::temp_dir().join(format!("den-atlas-desc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("labels.json"), b"{}").unwrap();
        std::fs::write(dir.join("vectors.bin"), b"\0\0").unwrap();
        std::fs::write(
            dir.join("dataset.meta.json"),
            br#"{"datasetVersion":"t","taxonomyVersion":"t","embeddingModel":"m","dims":2,"count":1,
                 "quantization":"int8",
                 "labelsFile":"labels.json","labelsBytes":2,"labelsSha256":"a",
                 "vectorsFile":"vectors.bin","vectorsBytes":2,"vectorsSha256":"b"}"#,
        )
        .unwrap();
        let ds = crate::dataset::Dataset::load(&dir).expect("fixture dataset must load");

        let state = Arc::new(AppState::for_test(Some(ds)));
        let req = HttpRequest::builder()
            .uri("/dataset.json")
            .header("x-forwarded-host", "atlas.example")
            .body(Body::empty())
            .unwrap();
        let resp = handle(State(state), req).await;

        assert_eq!(resp.status(), 200);
        let vary = resp
            .headers()
            .get("vary")
            .expect("the descriptor embeds the request origin but did not vary on it")
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(vary.contains("x-forwarded-host"), "{vary}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dataset fixture whose two blobs are distinguishable by content, so a route test can prove
    /// WHICH blob it served rather than merely that it served something.
    fn fixture(dir: &std::path::Path) -> crate::dataset::Dataset {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("labels.json"), b"LABELS").unwrap();
        std::fs::write(dir.join("vectors.bin"), b"VECTORS!").unwrap();
        // Every OPTIONAL blob the production dataset ships, not just some of them. With only the two
        // mandatory ones the "every advertised URL is versioned" loop saw two URLs, so dropping the
        // stamp from any optional blob passed; declaring metadata + facets but not the two premise
        // blobs left exactly that hole open for premise, and a premise route serving the wrong blob
        // passed too. The loop's floor below is tied to what this writes.
        std::fs::write(dir.join("meta.json"), b"METADATA").unwrap();
        std::fs::write(dir.join("facets.bin"), b"FACETS").unwrap();
        std::fs::write(dir.join("premise-labels.json"), b"PLABELS").unwrap();
        std::fs::write(dir.join("premise-vectors.bin"), b"PVECTORS").unwrap();
        std::fs::write(
            dir.join("dataset.meta.json"),
            br#"{"datasetVersion":"v9","taxonomyVersion":"t","embeddingModel":"m","dims":2,"count":1,
                 "quantization":"int8",
                 "labelsFile":"labels.json","labelsBytes":6,"labelsSha256":"a",
                 "vectorsFile":"vectors.bin","vectorsBytes":8,"vectorsSha256":"b",
                 "metadataFile":"meta.json","metadataBytes":8,"metadataSha256":"c",
                 "facetsFile":"facets.bin","facetsBytes":6,"facetsSha256":"d",
                 "premiseEmbeddingModel":"pm","premiseDims":2,"premiseCount":1,
                 "premiseLabelsFile":"premise-labels.json","premiseLabelsBytes":7,"premiseLabelsSha256":"e",
                 "premiseVectorsFile":"premise-vectors.bin","premiseVectorsBytes":8,"premiseVectorsSha256":"f"}"#,
        )
        .unwrap();
        crate::dataset::Dataset::load(dir).expect("fixture dataset must load")
    }

    async fn get(state: &Arc<AppState>, uri: &str) -> axum::response::Response {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        handle(State(Arc::clone(state)), HttpRequest::builder().uri(uri).body(Body::empty()).unwrap()).await
    }

    async fn body_of(resp: axum::response::Response) -> String {
        let b = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        String::from_utf8_lossy(&b).into_owned()
    }

    fn title_state() -> Arc<AppState> {
        use den_titlesearch::{MediaType, TitleIndex, TitleRecord};
        let index = TitleIndex::build(vec![TitleRecord {
            tmdb_id: 603,
            media_type: MediaType::Movie,
            title: "The Matrix".into(),
            popularity: 80.0,
        }]);
        Arc::new(AppState {
            titles: Some(Arc::new(crate::titles::TitleSearch::with_index(index))),
            ..AppState::for_test(None)
        })
    }

    /// Title search answers from the index — typos and an encoded query included — per type, and the
    /// manifest declares it.
    #[tokio::test]
    async fn title_search_answers_from_the_index() {
        let state = title_state();
        let hit = get(&state, "/catalog/movie/den-titles/search=the%20matrx.json").await;
        assert_eq!(hit.headers()["cache-control"], "public, max-age=3600, stale-while-revalidate=600");
        let hit = body_of(hit).await;
        assert!(hit.contains(r#""id":"tmdb:603""#), "{hit}");
        let other_type =
            body_of(get(&state, "/catalog/series/den-titles/search=the%20matrix.json").await).await;
        assert_eq!(other_type, r#"{"metas":[]}"#);
        let manifest = body_of(get(&state, "/manifest.json").await).await;
        assert!(manifest.contains(r#""id":"den-titles""#), "{manifest}");
    }

    /// Off by default: no catalog in the manifest, and the route is a 404 like any unknown catalog.
    #[tokio::test]
    async fn title_search_is_off_unless_configured() {
        let state = Arc::new(AppState::for_test(None));
        assert_eq!(get(&state, "/catalog/movie/den-titles/search=x.json").await.status(), 404);
        assert!(!body_of(get(&state, "/manifest.json").await).await.contains("den-titles"));
    }

    /// Before the first build lands the answer is empty and says why, and it isn't cached.
    #[tokio::test]
    async fn title_search_says_it_is_building_before_the_first_index() {
        let search = crate::titles::TitleSearch::new("http://127.0.0.1:9/").unwrap();
        let state = Arc::new(AppState { titles: Some(Arc::new(search)), ..AppState::for_test(None) });
        let resp = get(&state, "/catalog/movie/den-titles/search=matrix.json").await;
        assert_eq!(resp.headers().get(DEGRADED).unwrap(), "title_index_building");
        assert_eq!(resp.headers().get("cache-control").unwrap(), "no-store");
        assert_eq!(body_of(resp).await, r#"{"metas":[]}"#);
    }

    fn index_state(name: &str) -> Arc<AppState> {
        let dir = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let ds = crate::queries::write_fixture(&dir);
        let index = Arc::new(crate::queries::IndexQueries::new(&ds));
        Arc::new(AppState { index: Some(index), ..AppState::for_test(Some(ds)) })
    }

    /// The taxonomy, label rows (paged, per type, an encoded label) and More Like This, from a real fixture
    /// index — and the descriptor says the routes exist.
    #[tokio::test]
    async fn index_queries_answer_from_the_dataset() {
        let state = index_state("den-atlas-index");
        let json = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();
        let taxonomy = get(&state, "/index/taxonomy.json").await;
        assert_eq!(taxonomy.headers()["cache-control"], "public, max-age=3600, stale-while-revalidate=86400");
        let taxonomy = json(body_of(taxonomy).await);
        assert_eq!(taxonomy["subgenres"], serde_json::json!(["Heist", "Campy/Cult"]));
        assert_eq!(taxonomy["moods"], serde_json::json!(["Tense"]));

        for (path, want) in [
            ("/index/rows/movie/subgenre/Heist.json", serde_json::json!([1, 2, 3])),
            ("/index/rows/movie/subgenre/Heist.json?skip=1&limit=1", serde_json::json!([2])),
            ("/index/rows/series/subgenre/Heist.json", serde_json::json!([4])),
            ("/index/rows/movie/subgenre/Campy%2FCult.json", serde_json::json!([3])),
            ("/index/rows/movie/mood/Tense.json", serde_json::json!([1])),
            // Premise leads: 3 (its premise score, +¼ as the plot agrees, −¼ for another genre) beats 2.
            ("/index/similar/movie/1.json", serde_json::json!([3, 2])),
        ] {
            assert_eq!(json(body_of(get(&state, path).await).await)["ids"], want, "{path}");
        }
        assert!(body_of(get(&state, "/dataset.json").await).await.contains(r#""queries":true"#));
    }

    /// Off by default, and a malformed question is a 404 that never loads the index.
    #[tokio::test]
    async fn index_queries_are_off_unless_configured_and_refuse_malformed_paths() {
        let off = Arc::new(AppState::for_test(None));
        assert_eq!(get(&off, "/index/taxonomy.json").await.status(), 404);
        let state = index_state("den-atlas-index-bad");
        for path in [
            "/index/rows/movie/genre/Heist.json",
            "/index/rows/anime/subgenre/Heist.json",
            "/index/similar/movie/abc.json",
            "/index/nope.json",
        ] {
            assert_eq!(get(&state, path).await.status(), 404, "{path}");
        }
    }

    /// A stand-in den-embed answering every request with `vector`.
    async fn fake_embed(vector: &'static str) -> crate::EmbedProxy {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 16 * 1024];
                let _ = sock.read(&mut buf).await;
                let body = format!(r#"{{"vector":{vector},"dims":3,"model":"m"}}"#);
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        crate::EmbedProxy {
            client: reqwest::Client::new(),
            base: format!("http://{addr}"),
            inflight: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }

    async fn post(state: &Arc<AppState>, uri: &str, body: &str) -> axum::response::Response {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        let req = HttpRequest::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap();
        handle(State(Arc::clone(state)), req).await
    }

    /// Plain neighbours, semantic search through den-embed, and the facet lane — with and without a theme to
    /// rank the matches by.
    #[tokio::test]
    async fn neighbours_semantic_search_and_the_facet_lane() {
        let dir = std::env::temp_dir().join(format!("den-atlas-search-{}", std::process::id()));
        let ds = crate::queries::write_fixture(&dir);
        let index = Arc::new(crate::queries::IndexQueries::new(&ds));
        let state = Arc::new(AppState {
            index: Some(index),
            embed: Some(fake_embed("[0,100,0]").await),
            ..AppState::for_test(Some(ds))
        });
        let json = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();

        let neighbours = json(body_of(get(&state, "/index/neighbours/movie/1.json").await).await);
        assert_eq!(neighbours["ids"], serde_json::json!([2, 3]));
        // The embedded query sits on movie 3.
        let search = get(&state, "/index/search.json?q=campy+fun").await;
        assert_eq!(search.headers()["cache-control"], "public, max-age=300", "an embedded query stays short");
        let search = json(body_of(search).await);
        assert_eq!(search["titles"][0], serde_json::json!({"type": "movie", "id": 3}));

        let korean = json(body_of(get(&state, "/index/facets.json?q=korean%20movies").await).await);
        assert_eq!(korean["facet"]["country"], "KR");
        assert_eq!(
            korean["titles"],
            serde_json::json!([{"type": "movie", "id": 2}, {"type": "movie", "id": 1}])
        );
        // By votes the Korean titles are 2, 4, 1; the leftover theme ranks them toward the query: 2, 1, 4.
        let themed = json(body_of(get(&state, "/index/facets.json?q=korean+heist").await).await);
        assert_eq!(
            themed["titles"],
            serde_json::json!([{"type": "movie", "id": 2}, {"type": "movie", "id": 1}, {"type": "series", "id": 4}])
        );
        let none = json(body_of(get(&state, "/index/facets.json?q=heist").await).await);
        assert_eq!(none["facet"], serde_json::Value::Null);
    }

    /// Batch labels, taste scores and suggestions, and how a bad POST is refused.
    #[tokio::test]
    async fn labels_scores_and_suggestions_answer_by_post() {
        let state = index_state("den-atlas-post");
        let json = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();

        let labels = r#"{"titles":[{"type":"movie","id":3},{"type":"movie","id":99}]}"#;
        let labels = json(body_of(post(&state, "/index/labels.json", labels).await).await);
        assert_eq!(labels["labels"][0]["primaryGenre"], "Comedy");
        assert_eq!(labels["labels"][0]["subgenres"][1], serde_json::json!(["Campy/Cult", 0.9]));
        assert_eq!(labels["labels"][1], serde_json::Value::Null);

        let score = r#"{"liked":[{"type":"movie","id":1}],"disliked":[{"type":"movie","id":3}],
                        "candidates":[{"type":"movie","id":2},{"type":"movie","id":3}]}"#;
        let score = json(body_of(post(&state, "/index/score.json", score).await).await);
        assert_eq!(score["space"], "plot");
        assert!(score["scores"][0]["taste"].as_f64().unwrap() > 0.9, "2 sits by the liked 1");
        assert!(
            (score["scores"][1]["dislike"].as_f64().unwrap() - 1.0).abs() < 1e-9,
            "3 is the disliked one"
        );
        let premise = json(
            body_of(post(&state, "/index/score.json", r#"{"space":"premise","candidates":[]}"#).await).await,
        );
        assert_eq!(premise["space"], "premise");

        let suggest = r#"{"seeds":[{"type":"movie","id":1}],"exclude":[{"type":"movie","id":2}]}"#;
        let suggest = json(body_of(post(&state, "/index/suggest.json", suggest).await).await);
        assert_eq!(suggest["perSeed"][0]["seed"], serde_json::json!({"type": "movie", "id": 1}));
        assert_eq!(suggest["perSeed"][0]["ids"], serde_json::json!([3]));
        assert_eq!(suggest["pooled"], serde_json::json!([{"type": "movie", "id": 3}]));

        assert_eq!(post(&state, "/index/labels.json", "not json").await.status(), 400);
        assert_eq!(post(&state, "/index/score.json", r#"{"space":"x","candidates":[]}"#).await.status(), 400);
        assert_eq!(post(&state, "/index/nope.json", "{}").await.status(), 404);
    }

    #[test]
    fn the_log_hides_a_search_query() {
        let uri: axum::http::Uri = "/catalog/movie/den-titles/search=the%20matrix.json".parse().unwrap();
        assert_eq!(loggable_path(&uri), "/catalog/movie/den-titles/search=<query>.json");
        let uri: axum::http::Uri = "/US_nfx/catalog/movie/x/search=q&skip=5.json".parse().unwrap();
        assert_eq!(loggable_path(&uri), "/<config>/catalog/movie/x/search=<query>&skip=5.json");
        let uri: axum::http::Uri = "/catalog/movie/jw-nfx.json".parse().unwrap();
        assert_eq!(loggable_path(&uri), "/catalog/movie/jw-nfx.json");
    }

    #[test]
    fn percent_decodes_the_search_extra() {
        assert_eq!(percent_decode("the%20matrix"), "the matrix");
        assert_eq!(percent_decode("am%C3%A9lie"), "amélie");
        assert_eq!(percent_decode("100%zz%2"), "100%zz%2");
    }

    /// What each route SERVES, not merely that it answers. Every test in this file anchored a
    /// previously-found bug, so swapping the labels and vectors routes, or serving every blob as
    /// `immutable` regardless of `?v=`, or returning 200 for a missing dataset, all passed.
    #[tokio::test]
    async fn each_route_serves_what_it_advertises() {
        let dir = std::env::temp_dir().join(format!("den-atlas-routes-{}", std::process::id()));
        let state = Arc::new(AppState::for_test(Some(fixture(&dir))));

        let labels = get(&state, "/labels.json").await;
        assert_eq!(labels.status(), 200);
        assert_eq!(body_of(labels).await, "LABELS", "the labels route served another blob");

        let vectors = get(&state, "/vectors.bin").await;
        assert_eq!(vectors.status(), 200);
        assert_eq!(body_of(vectors).await, "VECTORS!", "the vectors route served another blob");

        // ...and the optional blobs, whose routes were entirely uncovered.
        let facets = get(&state, "/facets.bin").await;
        assert_eq!(facets.status(), 200);
        assert_eq!(body_of(facets).await, "FACETS", "the facets route served another blob");
        let meta = get(&state, "/meta.json").await;
        assert_eq!(meta.status(), 200);
        assert_eq!(body_of(meta).await, "METADATA", "the metadata route served another blob");
        let pl = get(&state, "/premise-labels.json").await;
        assert_eq!(pl.status(), 200);
        assert_eq!(body_of(pl).await, "PLABELS", "the premise-labels route served another blob");
        let pv = get(&state, "/premise-vectors.bin").await;
        assert_eq!(pv.status(), 200);
        assert_eq!(body_of(pv).await, "PVECTORS", "the premise-vectors route served another blob");

        // `?v=<current version>` pins for a year; a bare request must revalidate instead.
        let pinned = get(&state, "/labels.json?v=v9").await;
        let cc = pinned.headers().get("cache-control").unwrap().to_str().unwrap().to_owned();
        assert!(cc.contains("immutable"), "a version-pinned blob was not immutable: {cc}");
        let bare = get(&state, "/labels.json").await;
        let cc = bare.headers().get("cache-control").unwrap().to_str().unwrap().to_owned();
        assert!(!cc.contains("immutable"), "an unpinned blob was served immutable for a year: {cc}");

        // EVERY advertised URL carries the version stamp, or the pin above is unusable for that
        // blob. `contains("?v=v9")` is not enough — one stamped URL hides an unstamped sibling.
        let desc = body_of(get(&state, "/dataset.json").await).await;
        let urls: Vec<&str> = desc
            .match_indices("\"url\":\"")
            .map(|(i, m)| {
                let rest = &desc[i + m.len()..];
                &rest[..rest.find('"').unwrap_or(0)]
            })
            .collect();
        // Exact, not a floor: a floor of 4 was satisfied by the mandatory pair plus metadata and
        // facets, so the two premise URLs the fixture did not declare were never looked at.
        assert_eq!(urls.len(), 6, "the descriptor did not advertise every fixture blob: {desc}");
        for u in &urls {
            assert!(u.contains("?v=v9"), "an advertised URL is unversioned: {u} (all: {urls:?})");
        }
        // WHICH blob each FIELD points at. Collecting the six names and sorting them destroys the
        // binding that matters: transposing the premise labels/vectors entries — two adjacent,
        // near-identical literals, the likeliest bug in that block — left the sorted set identical
        // and passed. The sha and byte count travel with the URL, so they are checked together;
        // a transposition moves all three.
        let d: serde_json::Value = serde_json::from_str(&desc).expect("descriptor must be JSON");
        for (path, name, sha, bytes) in [
            (&["labels"][..], "labels.json", "a", 6u64),
            (&["vectors"][..], "vectors.bin", "b", 8),
            (&["metadata"][..], "meta.json", "c", 8),
            (&["facets"][..], "facets.bin", "d", 6),
            (&["premise", "labels"][..], "premise-labels.json", "e", 7),
            (&["premise", "vectors"][..], "premise-vectors.bin", "f", 8),
        ] {
            let mut node = &d;
            for k in path {
                node = node.get(k).unwrap_or_else(|| panic!("descriptor has no {path:?}: {desc}"));
            }
            let url = node["url"].as_str().unwrap_or_default();
            assert_eq!(url, format!("http://localhost/{name}?v=v9"), "{path:?} advertises the wrong blob");
            assert_eq!(node["sha256"].as_str().unwrap_or_default(), sha, "{path:?} carries the wrong sha");
            assert_eq!(node["bytes"].as_u64().unwrap_or_default(), bytes, "{path:?} carries the wrong size");
        }
        assert!(desc.contains("\"count\":1"), "{desc}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing dataset must be a 503, not a 200 with nothing in it — the manifest still advertises
    /// the resource, and the app needs to tell "no dataset here" from "an empty dataset".
    #[tokio::test]
    async fn a_missing_dataset_is_unavailable_not_empty() {
        let state = Arc::new(AppState::for_test(None));
        assert_eq!(get(&state, "/dataset.json").await.status(), 503);
    }

    /// The manifest is the contract the app reads first; dropping a resource or the country extra
    /// silently removes a feature rather than breaking it.
    #[tokio::test]
    async fn the_manifest_advertises_the_dataset_resource() {
        let dir = std::env::temp_dir().join(format!("den-atlas-mf-{}", std::process::id()));
        let state = Arc::new(AppState::for_test(Some(fixture(&dir))));
        let m = body_of(get(&state, "/manifest.json").await).await;
        assert!(m.contains("\"dataset\""), "the manifest stopped advertising the dataset: {m}");
        assert!(m.contains("catalog"), "{m}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An upstream that always answers, but slowly. The catalog's own test fake lives in that
    /// module; this one exists here because a request deadline can only be seen through `handle`.
    struct SlowSource {
        dwell: std::time::Duration,
    }

    #[async_trait::async_trait]
    impl crate::justwatch::TrendingSource for SlowSource {
        async fn popular(
            &self,
            _p: &str,
            _o: crate::justwatch::ObjectType,
            _c: &str,
            _s: &str,
        ) -> Result<Vec<crate::justwatch::TrendingItem>, ()> {
            tokio::time::sleep(self.dwell).await;
            Ok(vec![crate::justwatch::TrendingItem {
                imdb: "tt1".into(),
                moviedb: Some(42),
                title: "A".into(),
                rank: 0,
                rating: None,
                year: None,
            }])
        }
        async fn new_titles(
            &self,
            p: &str,
            o: crate::justwatch::ObjectType,
            c: &str,
        ) -> Result<Vec<crate::justwatch::TrendingItem>, ()> {
            self.popular(p, o, c, "").await
        }
        async fn packages(&self, _c: &str) -> Result<Vec<(i64, String)>, ()> {
            tokio::time::sleep(self.dwell).await;
            Ok(vec![(8, "nfx".into()), (119, "prv".into()), (1899, "mxx".into()), (531, "pmp".into())])
        }
    }

    /// Answers the first chart and fails every one after, so a test reaches the real
    /// serve-stale-on-error path: a good row cached, then a refresh that fails.
    struct FlakySource {
        charts: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::justwatch::TrendingSource for FlakySource {
        async fn popular(
            &self,
            _p: &str,
            _o: crate::justwatch::ObjectType,
            _c: &str,
            _s: &str,
        ) -> Result<Vec<crate::justwatch::TrendingItem>, ()> {
            if self.charts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
                return Err(());
            }
            Ok(vec![crate::justwatch::TrendingItem {
                imdb: "tt1".into(),
                moviedb: Some(42),
                title: "A".into(),
                rank: 0,
                rating: None,
                year: None,
            }])
        }
        async fn new_titles(
            &self,
            _p: &str,
            _o: crate::justwatch::ObjectType,
            _c: &str,
        ) -> Result<Vec<crate::justwatch::TrendingItem>, ()> {
            Err(())
        }
        async fn packages(&self, _c: &str) -> Result<Vec<(i64, String)>, ()> {
            Ok(vec![(8, "nfx".into())])
        }
    }

    fn timing_of(resp: &axum::response::Response) -> String {
        resp.headers().get("server-timing").map(|v| v.to_str().unwrap().to_owned()).unwrap_or_default()
    }

    /// Server-Timing names the phase that produced the row — the JustWatch fetch on a miss, the
    /// cache on a hit — and the total either way.
    #[tokio::test]
    async fn a_catalog_row_names_its_phases_in_server_timing() {
        let state = Arc::new(AppState::for_test_with_source(Arc::new(SlowSource {
            dwell: std::time::Duration::ZERO,
        })));
        let t = timing_of(&get(&state, "/catalog/movie/jw-nfx.json").await);
        assert!(t.starts_with("justwatch;dur=") && t.contains(", total;dur="), "{t}");
        let t = timing_of(&get(&state, "/catalog/movie/jw-nfx.json").await);
        assert!(t.starts_with("cache;desc=hit") && t.contains(", total;dur="), "{t}");
        assert!(!t.contains("justwatch"), "a cache hit claimed an upstream fetch: {t}");
    }

    /// X-Den-Degraded marks the stale answer and only that: a fresh row carries none, and the
    /// last-good copy served after a failed refresh says `stale_catalog`, /health's slug for it.
    #[tokio::test]
    async fn a_stale_catalog_answer_says_so_and_a_fresh_one_does_not() {
        let state = Arc::new(AppState {
            // A zero TTL, so the row the first request caches is already due for a refresh.
            catalog: crate::catalog::CatalogState::new(
                Arc::new(FlakySource { charts: Default::default() }),
                std::time::Duration::ZERO,
            ),
            ..AppState::for_test(None)
        });
        let fresh = get(&state, "/catalog/movie/jw-nfx.json").await;
        assert_eq!(fresh.status(), 200);
        assert!(fresh.headers().get(DEGRADED).is_none(), "a fresh row was flagged degraded");

        let stale = get(&state, "/catalog/movie/jw-nfx.json").await;
        assert_eq!(stale.headers().get(DEGRADED).map(|v| v.to_str().unwrap()), Some("stale_catalog"));
        let t = timing_of(&stale);
        assert!(t.contains("justwatch;dur=") && t.contains("cache;desc=stale"), "{t}");
        assert!(body_of(stale).await.contains("tt1"), "the stale answer was not the last-good row");
    }

    /// A source whose charts look like schema breaks, so the /health wiring can be exercised end to
    /// end rather than a step at a time.
    struct SuspectSource {
        age: Option<std::time::Duration>,
    }

    #[async_trait::async_trait]
    impl crate::justwatch::TrendingSource for SuspectSource {
        async fn popular(
            &self,
            _p: &str,
            _o: crate::justwatch::ObjectType,
            _c: &str,
            _s: &str,
        ) -> Result<Vec<crate::justwatch::TrendingItem>, ()> {
            Err(())
        }
        async fn new_titles(
            &self,
            _p: &str,
            _o: crate::justwatch::ObjectType,
            _c: &str,
        ) -> Result<Vec<crate::justwatch::TrendingItem>, ()> {
            Err(())
        }
        async fn packages(&self, _c: &str) -> Result<Vec<(i64, String)>, ()> {
            Err(())
        }
        fn last_schema_break_age(&self) -> Option<std::time::Duration> {
            self.age
        }
    }

    /// The whole chain: source → CatalogState → /health. Asserting `health_body` in isolation and
    /// the source's counter in isolation left the WIRING between them untested — `schema_suspect()`
    /// could return a constant `false` and every test still passed, which is verbatim the failure
    /// the counter's own doc comment says it exists to prevent.
    #[tokio::test]
    async fn a_suspected_schema_break_reaches_health() {
        let dir = std::env::temp_dir().join(format!("den-atlas-hsuspect-{}", std::process::id()));
        // A dataset has to be present, or `dataset_unavailable` outranks and hides the state.
        let health_for = |age| {
            let mut st = AppState::for_test_with_source(Arc::new(SuspectSource { age }));
            st.dataset = Some(fixture(&dir));
            Arc::new(st)
        };

        // Recent: inside the cache TTL, so the short rows are still being served.
        let state = health_for(Some(std::time::Duration::from_secs(60)));
        let body = body_of(get(&state, "/health").await).await;
        assert!(body.contains("catalog_schema_suspect"), "a live schema break never reached /health: {body}");

        // Old: past the TTL, so every affected row has been refetched since and the claim would be
        // stale. A lifetime count would still be reporting it here, for the life of the process.
        let state = health_for(Some(std::time::Duration::from_secs(7 * 3600)));
        let body = body_of(get(&state, "/health").await).await;
        assert!(!body.contains("catalog_schema_suspect"), "an expired schema break still reported: {body}");

        // Never seen.
        let state = health_for(None);
        let body = body_of(get(&state, "/health").await).await;
        assert!(body.contains(r#""status":"ok""#), "{body}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cold burst must end with every row served and cached, however long the queue gets.
    ///
    /// This is the test that was missing when a 20s `REQUEST_DEADLINE` shipped in `handle`. The
    /// catalog takes two permits in sequence and `Semaphore` is FIFO-fair, so completions cluster at
    /// the end of the drain: cutting the queue off part-way discards nearly everything rather than a
    /// proportional slice. At this size the drain is ~45s of upstream work, so reintroducing any
    /// deadline in the 20s range turns all 600 rows into 5xx and leaves the cache empty — which is
    /// worse than the slow answer, because nothing cached means the next wave is equally cold.
    ///
    /// Virtual time (`start_paused`), so 45s of simulated queue costs no wall clock. A deadline
    /// built on `tokio::time::timeout` fires against the same clock, so the mutation is still caught.
    #[tokio::test(start_paused = true)]
    async fn a_cold_burst_is_served_whole_however_long_the_queue_gets() {
        const KEYS: usize = 600;
        let state = Arc::new(AppState::for_test_with_source(Arc::new(SlowSource {
            dwell: std::time::Duration::from_millis(300),
        })));

        let mut set = tokio::task::JoinSet::new();
        for i in 0..KEYS {
            let state = Arc::clone(&state);
            // A distinct country per request, so every one is a distinct cold cache key with its own
            // single-flight gate — the shape that made the queue long in the first place.
            let country = format!("{}{}", (b'A' + (i / 26) as u8) as char, (b'A' + (i % 26) as u8) as char);
            set.spawn(async move {
                let resp = get(&state, &format!("/catalog/movie/jw-trending/country={country}.json")).await;
                let status = resp.status().as_u16();
                (status, body_of(resp).await.contains("tt1"))
            });
        }

        let mut served = 0;
        let mut empty = 0;
        let mut refused = 0;
        while let Some(j) = set.join_next().await {
            match j.unwrap() {
                (200, true) => served += 1,
                (200, false) => empty += 1,
                _ => refused += 1,
            }
        }
        assert_eq!(refused, 0, "{refused} of {KEYS} cold rows were refused instead of served");
        assert_eq!(empty, 0, "{empty} of {KEYS} cold rows came back empty");
        assert_eq!(served, KEYS, "only {served} of {KEYS} cold rows carried titles");
        assert_eq!(state.catalog.cache_len(), KEYS, "the cache did not fill, so the next wave re-fans out");
        assert!(state.catalog.fresh(), "/health went degraded from this server's own queueing");
    }

    async fn get_metrics(state: &Arc<AppState>, auth: Option<&str>) -> axum::response::Response {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        let mut req = HttpRequest::builder().uri("/metrics");
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        handle(State(Arc::clone(state)), req.body(Body::empty()).unwrap()).await
    }

    /// An unknown path and a refused /metrics are the same 404, byte for byte, so a prober cannot
    /// tell a configured-but-refused /metrics from one that does not exist.
    #[tokio::test]
    async fn an_unknown_path_and_a_refused_metrics_are_the_same_404() {
        let mut st = AppState::for_test(None);
        st.metrics_token = Some("s3cret".to_owned());
        let state = Arc::new(st);
        for (what, resp) in [
            ("unknown path", get(&state, "/nope").await),
            ("refused /metrics", get_metrics(&state, Some("Bearer wrong")).await),
        ] {
            assert_eq!(resp.status(), 404, "{what}");
            assert_eq!(resp.headers().get("content-type").unwrap(), "application/json", "{what}");
            assert_eq!(resp.headers().get("cache-control").unwrap(), "no-store", "{what}");
            assert_eq!(body_of(resp).await, r#"{"error":"not_found"}"#, "{what}");
        }
    }

    /// The wildcard origin is on EVERY response, not just those built by a helper that remembered
    /// it. /metrics built its own and went out without one, so a browser saw a CORS failure instead
    /// of the 200 or 404 that was actually sent.
    #[tokio::test]
    async fn every_response_allows_any_origin() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;

        let dir = std::env::temp_dir().join(format!("den-atlas-cors-{}", std::process::id()));
        let mut st = AppState::for_test(Some(fixture(&dir)));
        st.metrics_token = Some("s3cret".to_owned());
        let state = Arc::new(st);
        let send = |method: &str, uri: &str, header: Option<(&str, String)>, body: &'static str| {
            let mut req = HttpRequest::builder().method(method).uri(uri);
            if let Some((k, v)) = header {
                req = req.header(k, v);
            }
            handle(State(Arc::clone(&state)), req.body(Body::from(body)).unwrap())
        };

        let etag = get(&state, "/manifest.json").await.headers()["etag"].to_str().unwrap().to_owned();
        for (what, resp, status) in [
            ("/metrics", get_metrics(&state, Some("Bearer s3cret")).await, 200),
            ("refused /metrics", get_metrics(&state, Some("Bearer wrong")).await, 404),
            ("unknown path", get(&state, "/nope").await, 404),
            ("/health", get(&state, "/health").await, 200),
            ("blob", get(&state, "/labels.json").await, 200),
            ("304", send("GET", "/manifest.json", Some(("if-none-match", etag)), "").await, 304),
            ("unconfigured /embed", send("POST", "/embed", None, r#"{"text":"x"}"#).await, 503),
            ("wrong method", send("PUT", "/health", None, "").await, 405),
            ("preflight", send("OPTIONS", "/manifest.json", None, "").await, 204),
        ] {
            assert_eq!(resp.status(), status, "{what}");
            let acao: Vec<_> = resp.headers().get_all("access-control-allow-origin").iter().collect();
            assert_eq!(acao, ["*"], "{what} did not carry exactly one wildcard origin");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No token configured means no route, whatever the request carries.
    #[tokio::test]
    async fn metrics_are_off_without_a_token() {
        let state = Arc::new(AppState::for_test(None));
        assert_eq!(get_metrics(&state, None).await.status(), 404);
        assert_eq!(get_metrics(&state, Some("Bearer ")).await.status(), 404);
        assert_eq!(get_metrics(&state, Some("Bearer anything")).await.status(), 404);
    }

    #[tokio::test]
    async fn metrics_refuse_a_wrong_token() {
        let mut st = AppState::for_test(None);
        st.metrics_token = Some("s3cret".to_owned());
        let state = Arc::new(st);
        for auth in
            [None, Some("Bearer wrong!"), Some("Bearer s3cre"), Some("Bearer s3cret2"), Some("s3cret")]
        {
            assert_eq!(get_metrics(&state, auth).await.status(), 404, "{auth:?} was let in");
        }
    }

    #[tokio::test]
    async fn metrics_answer_the_right_token() {
        let dir = std::env::temp_dir().join(format!("den-atlas-metrics-{}", std::process::id()));
        let mut st = AppState::for_test(Some(fixture(&dir)));
        st.metrics_token = Some("s3cret".to_owned());
        let state = Arc::new(st);

        // Whitespace around the token is trimmed, as every den addon does.
        assert_eq!(get_metrics(&state, Some("Bearer  s3cret ")).await.status(), 200);
        let resp = get_metrics(&state, Some("Bearer s3cret")).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get("content-type").unwrap(), "text/plain; version=0.0.4; charset=utf-8");
        let body = body_of(resp).await;
        let build = format!("atlas_build_info{{version=\"{}\"}} 1\n", env!("CARGO_PKG_VERSION"));
        assert!(body.contains(&build), "{body}");
        assert!(
            body.contains(
                "atlas_dataset_info{dataset_version=\"v9\",taxonomy=\"t\",embedding_model=\"m\"} 1\n"
            ),
            "{body}"
        );
        assert!(body.contains("atlas_dataset_loaded 1\n"), "{body}");
        assert!(body.contains("atlas_dataset_titles 1\n"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
