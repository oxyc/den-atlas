//! Routing — the port of `handleAtlas`. A single fallback handler matches on the path (exact, like the TS),
//! so unknown paths 404 and non-GET/HEAD 405.

use crate::config::Config;
use crate::descriptor::build_descriptor;
use crate::http::{serve, Servable};
use crate::manifest::manifest_json;
use crate::titles;
use crate::util::{fnv1a, json_response, unavailable_response, RELOAD_WAIT};
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
    let exempt = crate::tos::exempt(&split_config(req.uri().path()).1);
    let mut resp = crate::tos::guard_response(route(State(state), req).await, exempt).await;
    resp.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, header::HeaderValue::from_static("*"));
    // The debug headers readable too: a cross-origin fetch sees only the CORS-safelisted headers unless
    // Expose-Headers names more, and Resource Timing hides Server-Timing without Timing-Allow-Origin.
    resp.headers_mut().insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        // Retry-After and ETag too, or a browser can neither wait as asked nor revalidate.
        header::HeaderValue::from_static("Server-Timing, X-Den-Degraded, Retry-After, ETag"),
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
        } else if route == "/recommend" {
            handle_recommend(&state, config, req).await
        } else if route == "/playground/import.json" {
            handle_playground_import(&state, req).await
        } else {
            json_response(r#"{"error":"method_not_allowed"}"#, StatusCode::METHOD_NOT_ALLOWED)
        };
    }
    if method != Method::GET && method != Method::HEAD {
        return json_response(r#"{"error":"method_not_allowed"}"#, StatusCode::METHOD_NOT_ALLOWED);
    }
    let headers = req.headers().clone();
    let query = req.uri().query().unwrap_or("").to_owned();
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
            health_body(
                ds.is_some(),
                state.catalog.fresh(),
                state.catalog.schema_suspect(),
                state.index.as_ref().is_some_and(|index| index.facts_unusable()),
                state.index.as_ref().is_some_and(|index| index.store_unusable()),
                state.index.as_ref().is_some_and(|index| index.rows_unusable()),
                state.index.as_ref().is_some_and(|index| index.votes_unusable()),
            ),
            StatusCode::OK,
        );
    }
    // `/ready` — the alertable one. `/health` answers 200 whatever is wrong, by the addon convention
    // (ADDON-02), which is right for a liveness probe and useless for waking someone. This returns 503 when a
    // feature class is off, so a plain `curl -f`, a systemd timer or an uptime check is enough to notice —
    // no scraper, no rules file. Cosmetic degradation still answers 200: a monitor that cries wolf over a
    // stale catalog is a monitor that gets muted before the outage it was meant to catch.
    if route == "/ready" {
        note_health(&state);
        let now = health_state(
            ds.is_some(),
            state.catalog.fresh(),
            state.catalog.schema_suspect(),
            state.index.as_ref().is_some_and(|index| index.facts_unusable()),
            state.index.as_ref().is_some_and(|index| index.store_unusable()),
            state.index.as_ref().is_some_and(|index| index.rows_unusable()),
            state.index.as_ref().is_some_and(|index| index.votes_unusable()),
        );
        let serious = now.is_some_and(|(reason, _)| loses_a_feature(reason));
        let body = health_body(
            ds.is_some(),
            state.catalog.fresh(),
            state.catalog.schema_suspect(),
            state.index.as_ref().is_some_and(|index| index.facts_unusable()),
            state.index.as_ref().is_some_and(|index| index.store_unusable()),
            state.index.as_ref().is_some_and(|index| index.rows_unusable()),
            state.index.as_ref().is_some_and(|index| index.votes_unusable()),
        );
        let status = if serious { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK };
        return json_response(body, status);
    }
    if route == "/manifest.json" {
        return serve_json(
            &method,
            &headers,
            manifest_json(&config, state.titles.is_some(), state.motn.enabled()),
            "public, max-age=3600, stale-while-revalidate=600, stale-if-error=86400",
            None,
        )
        .await;
    }
    if route == "/dataset.json" {
        return match ds {
            Some(ds) => {
                // No Last-Modified. The dataset's date is not this body's date: the embed/index flags
                // change the body under the same date, so a client revalidating with If-Modified-Since
                // alone would be told a changed descriptor had not changed. The ETag covers every byte.
                serve_json(
                    &method,
                    &headers,
                    build_descriptor(ds, state.embed.is_some(), state.index.is_some()),
                    "public, max-age=300, stale-while-revalidate=3600, stale-if-error=86400",
                    None,
                )
                .await
            }
            None => unavailable_response(
                r#"{"error":"dataset_unavailable","detail":"the dataset failed to load (missing/old dataset.meta.json); refresh it with scripts/fetch-dataset.sh"}"#,
                RELOAD_WAIT,
            ),
        };
    }
    if let Some(rest) = route.strip_prefix("/catalog/") {
        return handle_catalog(&method, &headers, rest, &config, &state).await;
    }
    if let Some(rest) = route.strip_prefix("/index/") {
        return handle_index(&method, &headers, rest, &query, &state).await;
    }
    if route == "/playground" || route.starts_with("/playground/") {
        return handle_playground(&method, &headers, route, &query, &state).await;
    }
    json_response(r#"{"error":"not_found"}"#, StatusCode::NOT_FOUND)
}

/// `POST /playground/import.json` — a playground file (`playground::FILE_FORMAT`) read back to the state it
/// holds: `{"state", "query", "removed"}`, where `query` is that state as the page's address carries it and
/// `removed` names the knobs an older file had that no longer exist. A 400 names what it refused. Pure JSON:
/// nothing is ranked, so the indexes are not loaded.
async fn handle_playground_import(state: &Arc<AppState>, req: Request) -> Response {
    if !(state.playground && state.index.is_some()) {
        return json_response(r#"{"error":"not_found"}"#, StatusCode::NOT_FOUND);
    }
    let bad_request = |detail: String| {
        json_response(
            serde_json::json!({ "error": "bad_request", "detail": detail }).to_string(),
            StatusCode::BAD_REQUEST,
        )
    };
    // A full export of twelve seeds at 200 titles each is well under this.
    let Ok(body) = axum::body::to_bytes(req.into_body(), 8 * 1024 * 1024).await else {
        return bad_request("the file is larger than 8 MB".to_owned());
    };
    let file: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(file) => file,
        Err(e) => return bad_request(format!("not JSON: {e}")),
    };
    match crate::playground::import(&file) {
        Ok((restored, removed)) => json_response(
            serde_json::json!({ "state": restored.to_json(), "query": restored.query(), "removed": removed })
                .to_string(),
            StatusCode::OK,
        ),
        Err(detail) => bad_request(detail),
    }
}

/// `/playground` (the page), `/playground/params.json` (the knobs and production's values),
/// `/playground/similar/{movie|series}/{id}.json?<knob>=…&limit=` (a tuned More Like This),
/// `/playground/rows.json?seeds=…&<knob>=…&limit=&judged=` (the same for up to twelve seeds in one request,
/// with `judged=1` also the judged-set scores) and
/// `/playground/judged.json?<knob>=…` (those knobs scored against the judged set). All 404 unless
/// `PLAYGROUND` and `INDEX_QUERIES` are both on (`playground.rs`). Answers are `no-store`: a tuned row is an
/// experiment, and nothing should keep one where a production row is looked for.
async fn handle_playground(
    method: &Method,
    headers: &axum::http::HeaderMap,
    route: &str,
    query: &str,
    state: &Arc<AppState>,
) -> Response {
    let started = Instant::now();
    let not_found = || json_response(r#"{"error":"not_found"}"#, StatusCode::NOT_FOUND);
    let (true, Some(queries)) = (state.playground, state.index.as_ref()) else { return not_found() };
    if route == "/playground" {
        return serve_html(method, headers, crate::playground::PAGE).await;
    }
    if route == "/playground/params.json" {
        return serve_json(method, headers, crate::playground::params_json(), "no-store", None).await;
    }
    if route == "/playground/titles.json" {
        // A 404 without the title index (`TITLE_SEARCH` off, or its first build not landed): the page then
        // searches with `/index/query.json`.
        let Some(export) = state.titles.as_ref().and_then(|t| t.index()) else { return not_found() };
        let (q, _) = crate::playground::take_param(query, "q");
        let q = q.unwrap_or_default();
        let (indexes, _) = match queries.get(|| warm_embed(state)).await {
            Ok(got) => got,
            Err(e) => {
                eprintln!("index load failed: {e}");
                return unavailable_response(r#"{"error":"index_unavailable"}"#, RELOAD_WAIT);
            }
        };
        let body = crate::playground::titles(&indexes, &export, &q).to_string();
        return serve_json(method, headers, body, "no-store", None).await;
    }
    enum Ask {
        Judged,
        Similar(den_index::MediaType, u32),
        Rows { judged: bool },
        Export { full: bool, judged: bool },
    }
    let bad_request = |detail: String| {
        json_response(
            serde_json::json!({ "error": "bad_request", "detail": detail }).to_string(),
            StatusCode::BAD_REQUEST,
        )
    };
    // Only `rows.json` and `export.json` read `seeds` and `suggest`, only `export.json` reads `full`, and only
    // those two read `judged`; everywhere else they are left in the query, where `parse` refuses them.
    let (ask, query) = match route {
        "/playground/judged.json" => (Ask::Judged, query.to_owned()),
        "/playground/rows.json" => {
            // `judged=1` adds the judged set's scores to the answer, so the page's one request per change
            // also carries how the change scores.
            let (judged, rest) = crate::playground::take_param(query, "judged");
            (Ask::Rows { judged: judged.as_deref() == Some("1") }, rest)
        }
        "/playground/export.json" => {
            let (full, rest) = crate::playground::take_param(query, "full");
            let (judged, rest) = crate::playground::take_param(&rest, "judged");
            (Ask::Export { full: full.as_deref() == Some("1"), judged: judged.as_deref() == Some("1") }, rest)
        }
        _ => {
            let Some(rest) = route.strip_prefix("/playground/similar/").and_then(|r| r.strip_suffix(".json"))
            else {
                return not_found();
            };
            let Some((media_type, tmdb_id)) = rest
                .split_once('/')
                .and_then(|(type_, id)| Some((index_media_type(type_)?, id.parse::<u32>().ok()?)))
            else {
                return not_found();
            };
            (Ask::Similar(media_type, tmdb_id), query.to_owned())
        }
    };
    let parsed = match ask {
        Ask::Rows { .. } | Ask::Export { .. } => crate::playground::State::parse(&query),
        _ => crate::playground::parse(&query).map(|tuning| crate::playground::State {
            tuning,
            seeds: Vec::new(),
            suggest: Vec::new(),
        }),
    };
    let playground_state = match parsed {
        Ok(parsed) => parsed,
        Err(detail) => return bad_request(detail),
    };
    if let (Ask::Rows { .. }, Err(detail)) =
        (&ask, crate::playground::rows_limit(playground_state.tuning.limit))
    {
        return bad_request(detail);
    }
    let meta = state.dataset.as_ref().map(|ds| ds.meta.clone());
    let titles = state.titles.clone();
    let (indexes, loaded_in) = match queries.get(|| warm_embed(state)).await {
        Ok(got) => got,
        Err(e) => {
            eprintln!("index load failed: {e}");
            return unavailable_response(r#"{"error":"index_unavailable"}"#, RELOAD_WAIT);
        }
    };
    let ranking = Instant::now();
    let answered = tokio::task::spawn_blocking(move || {
        let export = titles.as_ref().and_then(|t| t.index());
        let sources = crate::playground::Sources { indexes: &indexes, export: export.as_deref() };
        let s = &playground_state;
        match ask {
            Ask::Similar(media_type, tmdb_id) => {
                crate::playground::answer(&sources, media_type, tmdb_id, &s.tuning).to_string()
            }
            Ask::Rows { judged } => {
                let mut answer = crate::playground::rows(&sources, &s.seeds, &s.suggest, &s.tuning);
                if judged {
                    answer["judged"] = crate::playground::judged(&sources, &s.tuning);
                }
                answer.to_string()
            }
            Ask::Judged => crate::playground::judged(&sources, &s.tuning).to_string(),
            Ask::Export { full, judged } => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let provenance = crate::playground::Provenance {
                    dataset_version: meta.as_ref().map_or("", |m| m.dataset_version.as_str()),
                    store_sha256: meta.as_ref().and_then(|m| m.store_sha256.as_deref()),
                    now,
                };
                crate::playground::export(&sources, s, &provenance, full, judged).to_string()
            }
        }
    })
    .await;
    let body = match answered {
        Ok(body) => body,
        Err(e) => {
            eprintln!("playground answer failed: {e}");
            return json_response(r#"{"error":"index_failed"}"#, StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let load = loaded_in.map(|d| format!("load;dur={}, ", ms(d))).unwrap_or_default();
    let resp = serve_json(method, headers, body, "no-store", None).await;
    with_timing(
        resp,
        &format!("{load}rank;dur={}, total;dur={}", ms(ranking.elapsed()), ms(started.elapsed())),
    )
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
        // As long again as it just waited for a slot.
        return unavailable_response(
            r#"{"error":"embed_busy","detail":"too many concurrent embeds; retry shortly"}"#,
            crate::EMBED_WAIT,
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
            // den-embed's own wait hint, when it gives one, reaches the client.
            let retry_after = resp.headers().get(header::RETRY_AFTER).cloned();
            let bytes = resp.bytes().await.unwrap_or_default();
            let mut out = Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                // A POST proxy of per-query vectors — never let a heuristic/intermediary cache these.
                .header(header::CACHE_CONTROL, "no-store")
                .body(Body::from(bytes))
                .unwrap();
            if let Some(value) = retry_after {
                out.headers_mut().insert(header::RETRY_AFTER, value);
            }
            out
        }
        Err(e) => {
            crate::util::log_throttled!("embed upstream error: {e}");
            json_response(r#"{"error":"embed_upstream_failed"}"#, StatusCode::BAD_GATEWAY)
        }
    };
    with_timing(resp, &format!("embed;dur={}, total;dur={}", ms(upstream.elapsed()), ms(started.elapsed())))
}

/// Whether a degraded reason means a FEATURE CLASS IS OFF, rather than that the answers are merely older.
///
/// `stale_catalog` serves yesterday's rows: worth knowing, not worth waking anyone. `facts_unusable` and
/// `dataset_unavailable` silently remove whole capabilities — people search, imdbId, countries, /recommend's
/// reading of a title — while every request still answers 200. That is the state that sat unnoticed for
/// nineteen minutes and surfaced as bad search results rather than as a signal, so the two must not look
/// alike to a monitor.
pub(crate) fn loses_a_feature(reason: &str) -> bool {
    matches!(
        reason,
        "dataset_unavailable" | "facts_unusable" | "store_unusable" | "rows_unusable" | "votes_unusable"
    )
}

/// What `/health` reports: `None` when healthy, else the reason slug and a one-sentence detail.
/// Dataset-unavailable outranks a stale catalog (no dataset is the more severe condition): no dataset ⇒
/// `dataset_unavailable`; else a failed last JustWatch refresh ⇒ `stale_catalog`; then a suspected schema break;
/// then a declared facts file the last index load couldn't read ⇒ `facts_unusable`. One function feeds both the
/// body and the state-change log line, so the two cannot disagree.
pub(crate) fn health_state(
    dataset_loaded: bool,
    catalog_fresh: bool,
    schema_suspect: bool,
    facts_unusable: bool,
    store_unusable: bool,
    rows_unusable: bool,
    votes_unusable: bool,
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
    } else if store_unusable {
        // The store is what More Like This ranks on. Without it the rail silently falls back to the
        // pre-pooled scorer — every request still answers, and answers worse, which is the shape of
        // failure `den-update`'s probe exists to catch and cannot see unless it is named here.
        //
        // "no store" covers both ways it happens, deliberately: a manifest that declares one atlas
        // cannot read, and a manifest that declares none. The detail line is the one sentence an
        // operator gets, and naming only the first would misdescribe the case that actually fires when
        // an atlas that wants a store meets a generation published before stores existed.
        Some((
            "store_unusable",
            "no usable store; More Like This falls back to the pre-pooled scorer (premise index only)",
        ))
    } else if facts_unusable {
        // Everything still answers, from labels and facets alone — which is why it is invisible without this:
        // recommendations lose their dates, people and countries, and search its people and other titles.
        Some(("facts_unusable", "the dataset's facts did not read; /recommend and search run without facts"))
    } else if rows_unusable {
        // The store can read perfectly and its twelve `facet_*` sections still be missing or mis-typed.
        // Every browse row then answers `{"titles":[],"total":0}` — a whole screen blank — with nothing
        // else wrong anywhere, which is why this needed a reason of its own rather than folding into the
        // two above.
        Some(("rows_unusable", "the dataset's facet rows did not read; every browse row is empty"))
    } else if votes_unusable {
        // A row that is FULL and in the wrong order, which is the one failure here that looks like a
        // working addon. Neither vote source has a count — no TMDB counts are kept (`tmdb.rs`) and the
        // store's `votes` column reads nothing — so every browse row falls back to tmdb-id order, which
        // is how *La Job* came to sit beside *Game of Thrones*. It shipped once, silently, because
        // `votes_of` answered 0 for every title and said nothing.
        Some((
            "votes_unusable",
            "no vote counts kept from TMDB or in the store; every browse row falls back to tmdb-id order",
        ))
    } else {
        None
    }
}

/// The `/health` JSON body (ADDON-02). Always paired with 200 + `no-store` — liveness never fails; the
/// body carries the real state. Pure, so the decision is unit-testable without an HTTP round-trip.
fn health_body(
    dataset_loaded: bool,
    catalog_fresh: bool,
    schema_suspect: bool,
    facts_unusable: bool,
    store_unusable: bool,
    rows_unusable: bool,
    votes_unusable: bool,
) -> String {
    match health_state(
        dataset_loaded,
        catalog_fresh,
        schema_suspect,
        facts_unusable,
        store_unusable,
        rows_unusable,
        votes_unusable,
    ) {
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
pub(crate) fn note_catalog_health(state: &AppState) {
    note_health(state);
}

fn note_health(state: &AppState) {
    let now = health_state(
        state.dataset.is_some(),
        state.catalog.fresh(),
        state.catalog.schema_suspect(),
        state.index.as_ref().is_some_and(|index| index.facts_unusable()),
        state.index.as_ref().is_some_and(|index| index.store_unusable()),
        state.index.as_ref().is_some_and(|index| index.rows_unusable()),
        state.index.as_ref().is_some_and(|index| index.votes_unusable()),
    );
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
    // Stremio pages with `skip`. Anything unparseable is the first page rather than an error: a row is
    // worth serving whole to a client that asked for it oddly.
    let skip = extra_value(extra, "skip").and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
    let answer = state.catalog.metas_json(id, type_, &country, &config.providers).await;
    // A refresh is what moves the catalog's health, so a change is noticed here as it happens.
    note_health(state);
    match answer {
        Some(r) => {
            // Fresh/stale-good rows cache for an hour, and for a day after that a browser shows the row it has
            // while it asks again — a chart a day old is still the chart; an outage-empty/stale fallback caches
            // briefly so a CDN doesn't pin a broken row past JustWatch's recovery.
            let cc = if r.fresh {
                "public, max-age=3600, stale-while-revalidate=86400, stale-if-error=86400"
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
            let mut resp =
                serve_json(method, headers, crate::catalog::page_of(&r.body, skip), cc, None).await;
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
pub(crate) const ROW_PAGE: usize = 24;
pub(crate) const MAX_ROW_PAGE: usize = 100;

/// One `/index/…` question, parsed before the index loads, so a malformed path never pays for a load.
enum IndexQuestion {
    Taxonomy,
    Schema,
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
    /// Search in one request (`search.rs`). Answered in `handle_index`, because it waits on den-embed.
    Query,
    /// A row of the plot facets named in the query (`?ending=bittersweet&tone=bleak`).
    Plot {
        media_type: den_index::MediaType,
    },
    /// A filter question (`filter.rs`): counts, titles or one kind's values under a selection. Its query is
    /// read in `handle_index`, before the load, so a malformed one never pays for it. Answered there.
    Filter {
        media_type: den_index::MediaType,
        route: crate::filter::Route,
    },
}

impl IndexQuestion {
    fn parse(route: &str) -> Option<Self> {
        let parts: Vec<&str> = route.split('/').collect();
        match parts.as_slice() {
            ["taxonomy"] => Some(Self::Taxonomy),
            ["schema"] => Some(Self::Schema),
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
            ["filter", type_, question] => {
                let route = match *question {
                    "counts" => crate::filter::Route::Counts,
                    "titles" => crate::filter::Route::Titles,
                    _ => return None,
                };
                Some(Self::Filter { media_type: index_media_type(type_)?, route })
            }
            ["filter", type_, "values", kind] => {
                let spec = crate::filter::spec(kind).filter(|s| s.searchable())?;
                Some(Self::Filter {
                    media_type: index_media_type(type_)?,
                    route: crate::filter::Route::Values(spec),
                })
            }
            ["query"] => Some(Self::Query),
            ["plot" | "row", type_] => Some(Self::Plot { media_type: index_media_type(type_)? }),
            _ => None,
        }
    }

    /// `export`, TMDB's daily title export, ranks a row's titles facets.bin has no votes for.
    fn answer(
        &self,
        indexes: &crate::queries::Indexes,
        export: Option<&den_titlesearch::TitleIndex>,
        query: &str,
    ) -> String {
        let plot = &indexes.plot;
        let body = match self {
            // The label names only, in the order the TV app builds its browse rows from; it decodes exactly
            // these three fields, so the route keeps its shape. It carries no counts, no coverage and none
            // of the plot-facet axes: `schema` names the route that describes the dataset, so a client that
            // finds this one first is sent there rather than taking a name list for the vocabulary.
            Self::Taxonomy => serde_json::json!({
                "schema": "/index/schema.json",
                "taxonomyVersion": plot.taxonomy_version(),
                "subgenres": plot.subgenre_labels(),
                "moods": plot.mood_labels(),
            }),
            Self::Schema => crate::schema::document(indexes),
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
                let total = if *mood {
                    plot.count_with_mood(label, Some(*media_type), floor)
                } else {
                    plot.count_with_subgenre(label, Some(*media_type), floor)
                };
                let family = if *mood { "mood" } else { "subgenre" };
                let constraints = [(family.to_owned(), label.clone())];
                serde_json::json!({
                    "ids": titles.iter().map(|&(id, _)| id).collect::<Vec<u32>>(),
                    "total": total,
                    "coverage": crate::schema::row_coverage(indexes, *media_type, &constraints),
                })
            }
            Self::Similar { media_type, tmdb_id } => {
                // The row is computed once and memoised, so a later page is a slice rather than a rescore.
                // `skip`/`limit` because the rail is scrolled: a fixed twenty where hundreds exist reads as
                // broken. Absent both, the answer is the first screenful, which is what every existing
                // caller already expects.
                let row = indexes.more_like_this(*tmdb_id, *media_type);
                let skip = query_param(query, "skip").and_then(|v| v.parse().ok()).unwrap_or(0);
                let limit = query_param(query, "limit")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(SIMILAR_PAGE)
                    .min(den_index::MAX_ROW);
                let page: Vec<u32> = row.iter().copied().skip(skip).take(limit).collect();
                serde_json::json!({"ids": page, "total": row.len()})
            }
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
            Self::Plot { media_type } => {
                let number = |key: &str, default: usize| {
                    query_param(query, key).and_then(|v| v.parse().ok()).unwrap_or(default)
                };
                let constraints: Vec<(String, String)> = query
                    .split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .filter(|(key, _)| !matches!(*key, "skip" | "limit"))
                    // The taste is not an axis. Every tilt parameter carries one prefix so a household's
                    // `tilt.liked` can never be read as a constraint, whatever axes the store grows.
                    .filter(|(key, _)| !key.starts_with(crate::plotrows::TILT_PREFIX))
                    // Form-encoded, as a browser's URLSearchParams writes it: a space is a `+`.
                    .map(|(key, value)| (percent_decode(key), percent_decode(&value.replace('+', " "))))
                    .collect();
                crate::plotrows::row(
                    indexes,
                    export,
                    *media_type,
                    &constraints,
                    crate::plotrows::Tilt::parse(query).as_ref(),
                    number("skip", 0),
                    number("limit", ROW_PAGE).min(MAX_ROW_PAGE),
                )
            }
            Self::Search | Self::Facets | Self::Query | Self::Filter { .. } => {
                unreachable!("answered in handle_index")
            }
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
pub(crate) const SEMANTIC_K: usize = 24;
pub(crate) const NEIGHBOUR_K: usize = 12;
/// One screenful of More Like This, when the caller asks for no page.
pub(crate) const SIMILAR_PAGE: usize = 20;
pub(crate) const MAX_NEIGHBOUR_K: usize = 50;
pub(crate) const FACET_LIMIT: usize = 50;
const SUGGEST_LIMIT: usize = 20;
/// The most titles one POST may name, and the most seeds a suggestion takes (the tvOS app's own cap).
pub(crate) const MAX_TITLES: usize = 500;
pub(crate) const MAX_SEEDS: usize = 8;

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
/// permits and deadline, answered from `embed_memo` when this text was embedded for this dataset's space
/// already. Only a vector as wide as the dataset's is kept, so a den-embed answering in another width is asked
/// again rather than remembered.
async fn embed_query(state: &AppState, text: &str) -> Result<Vec<i8>, String> {
    let key = state.dataset.as_ref().map(|ds| {
        (crate::cache::EmbedMemo::key(&ds.meta.embedding_model, ds.meta.dims, text), ds.meta.dims as usize)
    });
    if let Some(vector) = key.as_ref().and_then(|(key, _)| state.embed_memo.get(key)) {
        return Ok(vector);
    }
    let vector = embed_upstream(state, text).await?;
    if let Some((key, dims)) = key {
        if vector.len() == dims {
            state.embed_memo.put(key, vector.clone());
        }
    }
    Ok(vector)
}

/// One den-embed call, never from the memo.
async fn embed_upstream(state: &AppState, text: &str) -> Result<Vec<i8>, String> {
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

/// Wake den-embed as the indexes load. Both drop their memory after ten idle minutes, so a load follows the spell
/// in which den-embed unloaded its model too, and the search that comes next would wait on that model otherwise.
fn warm_embed(state: &Arc<AppState>) {
    if state.embed.is_none() {
        return;
    }
    let state = Arc::clone(state);
    // Past the memo: a remembered "warm" would wake nothing.
    tokio::spawn(async move {
        if let Err(e) = embed_upstream(&state, "warm").await {
            eprintln!("den-embed warm-up failed: {e}");
        }
    });
}

/// Semantic search in one request: embed the query, then the plot index's nearest titles to it — the tvOS
/// app's `semanticSearch` — each with its score, and the mean and standard deviation of the whole scan, so a
/// client can drop what barely stands out from this query's own spread. The error is why den-embed couldn't
/// answer.
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
    let (neighbours, stats) =
        indexes.plot.scan_vector(&vector, |_, kind| media_type.is_none_or(|want| want == kind), SEMANTIC_K);
    let titles: Vec<serde_json::Value> = neighbours
        .iter()
        .map(|n| serde_json::json!({ "type": stremio_type(n.media_type), "id": n.tmdb_id, "score": n.score }))
        .collect();
    Ok(serde_json::json!({ "titles": titles, "mean": stats.mean, "sd": stats.sd }).to_string())
}

/// `GET /index/query.json?q=&type=&skip=&limit=` — search in one request (`search.rs`), and how long its parts took
/// as Server-Timing entries. The query is embedded only when den-embed answers; without it the plot vectors simply
/// don't score. `early` is the whole query's embedding, started as the request arrived (`handle_index`): it is what
/// the vectors are asked about unless the query names something, and a cold den-embed then loads its model while
/// the indexes load rather than after. The ranking itself runs off the request threads. The flag says den-embed was
/// asked and did not answer, so the ranking went without the vectors.
async fn query_answer(
    state: &AppState,
    indexes: Arc<crate::queries::Indexes>,
    query: &str,
    early: Option<tokio::task::JoinHandle<Result<Vec<i8>, String>>>,
) -> Result<(String, String, bool), String> {
    let text = query_text(query, "q");
    let media_type = query_param(query, "type").and_then(|t| index_media_type(&t));
    let number =
        |key: &str, default: usize| query_param(query, key).and_then(|v| v.parse().ok()).unwrap_or(default);
    let (skip, limit) =
        (number("skip", 0), number("limit", crate::search::PAGE).min(crate::search::MAX_PAGE));
    let parsing = Instant::now();
    let mut parsed = crate::search::parse(&text, &indexes);
    // `year_min` / `year_max` — the STRUCTURED way to bound a release year, for a caller that has already
    // worked out what the user meant. Atlas reads a handful of words itself ("recent", "classic", a bare
    // year) because the TV's search box has nothing else; it deliberately does not grow a natural-language
    // date parser, because an LLM client turns "something from before I was born" or "the last five years"
    // into these two numbers far better than a word list ever will. Given explicitly, they win over anything
    // the text said.
    if let Some(min) = query_param(query, "year_min").and_then(|v| v.parse().ok()) {
        parsed.set_year_min(min);
    }
    if let Some(max) = query_param(query, "year_max").and_then(|v| v.parse().ok()) {
        parsed.set_year_max(max);
    }
    // `language` is a parameter and never a word. A demonym read from prose picks the wrong axis about half
    // the time — "spanish" as a country misses 1,138 Spanish-language titles made outside Spain — and only
    // the caller knows which was meant.
    if let Some(code) = query_param(query, "language").filter(|c| c.len() == 2) {
        parsed.set_language(&code);
    }
    if let Some(minutes) = query_param(query, "runtime_max").and_then(|v| v.parse().ok()) {
        parsed.set_runtime_max(minutes);
    }
    // A broadcaster's Wikidata Q-id, with or without the Q ("Q1193900" or "1193900").
    if let Some(qid) = query_param(query, "broadcaster")
        .map(|v| v.trim_start_matches(['Q', 'q']).to_owned())
        .and_then(|v| v.parse().ok())
    {
        parsed.set_broadcaster(qid);
    }
    let parsed_in = parsing.elapsed();
    let embedding = Instant::now();
    let unembedded = |e: String| eprintln!("search query left unembedded: {e}");
    let vector = match (parsed.embed_text(), early) {
        (Some(words), Some(early)) if words == parsed.text() => {
            early.await.map_err(|e| e.to_string()).and_then(|embedded| embedded).map_err(unembedded).ok()
        }
        (Some(words), _) if state.embed.is_some() => embed_query(state, words).await.map_err(unembedded).ok(),
        _ => None,
    };
    let unembedded = vector.is_none() && state.embed.is_some() && parsed.embed_text().is_some();
    let embedded_in = embedding.elapsed();
    let answering = Instant::now();
    let titles = state.titles.as_ref().and_then(|t| t.index());
    let body = tokio::task::spawn_blocking(move || {
        crate::search::answer(
            &indexes,
            titles.as_deref(),
            &parsed,
            media_type,
            vector.as_deref(),
            skip,
            limit,
        )
        .to_string()
    })
    .await
    .map_err(|e| e.to_string())?;
    let timing = format!(
        "parse;dur={}, embed;dur={}, answer;dur={}",
        ms(parsed_in),
        ms(embedded_in),
        ms(answering.elapsed())
    );
    Ok((body, timing, unembedded))
}

/// The facet lane — the tvOS app's facet search: titles matching the query's country, decade and type,
/// most-voted first, with any leftover words ranked semantically to the front. `facet` is null when the query
/// names none. Without den-embed the matches still come back, unranked, and the flag says the ranking was skipped.
async fn facets_answer(state: &AppState, indexes: &crate::queries::Indexes, query: &str) -> (String, bool) {
    // A ruled-out word is neither a facet nor a theme: "crime series not british" read `country: GB`, and
    // `not horror` ranked the facet's horror first.
    let negation = den_index::split_negation(&query_text(query, "q"));
    let facet = den_index::FacetQuery::parse(&negation.kept);
    let (Some(facets), true) = (indexes.facets.as_ref(), facet.has_facet()) else {
        return (serde_json::json!({ "facet": null, "titles": [] }).to_string(), false);
    };
    let mut unranked = false;
    let mut titles = facets.filter(facet.media_type, facet.country, facet.decade);
    let ruled_out: Vec<den_index::FacetQuery> =
        negation.excluded.iter().map(|phrase| den_index::FacetQuery::parse(phrase)).collect();
    titles.retain(|&(id, kind)| {
        let known = facets.title(id, kind);
        !ruled_out.iter().any(|out| {
            out.media_type == Some(kind)
                || out
                    .country
                    .is_some_and(|c| known.and_then(|t| t.country).is_some_and(|k| c.as_bytes() == k))
                || out.decade.is_some_and(|d| known.and_then(|t| t.year).is_some_and(|y| y / 10 * 10 == d))
        })
    });
    if !facet.leftover.is_empty() && !titles.is_empty() {
        match embed_query(state, &facet.leftover).await {
            Ok(vector) => {
                // Ranked within the facet's own titles. Taking the corpus-wide nearest and keeping those that
                // match the facet left "korean heist" with a title or two lifted and the rest in vote order.
                let matched: std::collections::HashSet<_> = titles.iter().copied().collect();
                let head: Vec<_> = indexes
                    .plot
                    .scan_vector(&vector, |id, kind| matched.contains(&(id, kind)), FACET_LIMIT)
                    .0
                    .into_iter()
                    .map(|n| (n.tmdb_id, n.media_type))
                    .collect();
                let lifted: std::collections::HashSet<_> = head.iter().copied().collect();
                titles = head.into_iter().chain(titles.into_iter().filter(|t| !lifted.contains(t))).collect();
            }
            Err(e) => {
                eprintln!("facet leftover left unranked: {e}");
                unranked = true;
            }
        }
    }
    titles.truncate(FACET_LIMIT);
    let body = serde_json::json!({
        "facet": {
            "mediaType": facet.media_type.map(stremio_type),
            "country": facet.country,
            "decade": facet.decade,
            "leftover": facet.leftover,
        },
        "titles": titles_json(&titles),
    })
    .to_string();
    (body, unranked)
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
    let (per_seed, pooled) = suggest_pool(&seeds, &excluded, limit, |id, media_type| {
        indexes.more_like_this(id, media_type).to_vec()
    });
    let per_seed: Vec<serde_json::Value> = per_seed
        .iter()
        .map(|(seed, ids)| serde_json::json!({ "seed": titles_json(&[*seed])[0], "ids": ids }))
        .collect();
    Ok(serde_json::json!({ "perSeed": per_seed, "pooled": titles_json(&pooled) }))
}

/// Each seed's More Like This (`row`) minus the excluded titles, and those rows pooled in seed order up to
/// `limit` — You Might Also Like. The playground pools its tuned rows through this too.
pub(crate) type SuggestPool =
    (Vec<((u32, den_index::MediaType), Vec<u32>)>, Vec<(u32, den_index::MediaType)>);

pub(crate) fn suggest_pool(
    seeds: &[(u32, den_index::MediaType)],
    excluded: &std::collections::HashSet<(u32, den_index::MediaType)>,
    limit: usize,
    row: impl Fn(u32, den_index::MediaType) -> Vec<u32>,
) -> SuggestPool {
    let per_seed: Vec<((u32, den_index::MediaType), Vec<u32>)> = seeds
        .iter()
        .map(|&(id, media_type)| {
            let ids = row(id, media_type).into_iter().filter(|&n| !excluded.contains(&(n, media_type)));
            ((id, media_type), ids.collect())
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
    (per_seed, pooled)
}

/// The most a `/recommend` body may carry: a large library and everything it owns, with room to spare.
const RECOMMEND_BODY: usize = 512 * 1024;

/// `POST /recommend` — the titles a featured surface leads with, ranked (`recommend.rs`). Uncached, and off
/// (404) unless `INDEX_QUERIES` is set, like the other questions that name a library's titles. The install's
/// config decides which services' lists are read when the household has picked none.
async fn handle_recommend(state: &Arc<AppState>, config: Config, req: Request) -> Response {
    let started = Instant::now();
    let rid = request_id(req.headers()).map(|rid| format!(" rid={rid}")).unwrap_or_default();
    let Some(queries) = state.index.as_ref() else {
        return json_response(r#"{"error":"not_found"}"#, StatusCode::NOT_FOUND);
    };
    let bad = |detail: String| {
        json_response(
            serde_json::json!({ "error": "bad_request", "detail": detail }).to_string(),
            StatusCode::BAD_REQUEST,
        )
    };
    let Ok(body) = axum::body::to_bytes(req.into_body(), RECOMMEND_BODY).await else {
        return bad(format!("a body of at most {RECOMMEND_BODY} bytes"));
    };
    let request: crate::recommend::Request = match parse_body(&body) {
        Ok(request) => request,
        Err(detail) => return bad(detail),
    };
    if let Err(detail) = request.check() {
        return bad(detail);
    }
    let (indexes, loaded_in) = match queries.get(|| warm_embed(state)).await {
        Ok(got) => got,
        Err(e) => {
            eprintln!("index load failed: {e}");
            return unavailable_response(r#"{"error":"index_unavailable"}"#, RELOAD_WAIT);
        }
    };
    // Kept for `den-atlas replay` when `RECOMMEND_FIXTURES` names a directory: the body as sent, library and all.
    let fixtures = std::env::var("RECOMMEND_FIXTURES").ok().filter(|dir| !dir.is_empty());
    let raw: Option<serde_json::Value> = fixtures.as_ref().and_then(|_| serde_json::from_slice(&body).ok());
    let listing = Instant::now();
    let lists = crate::recommend::lists(state, &config, &request).await;
    let listed = listing.elapsed();
    let ranking = Instant::now();
    let version = state.dataset.as_ref().map(|ds| ds.meta.dataset_version.clone());
    // TMDB's export popularity, for the titles no client hint describes (`TITLE_SEARCH`).
    let export = state.titles.as_ref().and_then(|t| t.index());
    // More Like This for each seed scans the vectors, so the ranking runs off the request threads.
    let ranked = tokio::task::spawn_blocking(move || {
        let now = request
            .now
            .as_deref()
            .and_then(crate::recommend::parse_now)
            .unwrap_or_else(crate::recommend::today);
        let mut answer = crate::recommend::answer(&indexes, export.as_deref(), &request, &lists, now);
        eprintln!("{}{rid}", crate::recommend::summary(&indexes, &request, &answer));
        if let (Some(dir), Some(raw)) = (&fixtures, &raw) {
            crate::recommend::keep_fixture(std::path::Path::new(dir), raw, &lists, now);
        }
        answer["datasetVersion"] = serde_json::json!(version);
        answer.to_string()
    })
    .await;
    let resp = match ranked {
        Ok(body) => json_response(body, StatusCode::OK),
        Err(e) => {
            eprintln!("recommend failed: {e}");
            json_response(r#"{"error":"recommend_failed"}"#, StatusCode::INTERNAL_SERVER_ERROR)
        }
    };
    let load = loaded_in.map(|d| format!("load;dur={}, ", ms(d))).unwrap_or_default();
    with_timing(
        resp,
        &format!(
            "{load}lists;dur={}, rank;dur={}, total;dur={}",
            ms(listed),
            ms(ranking.elapsed()),
            ms(started.elapsed())
        ),
    )
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
    let (indexes, loaded_in) = match queries.get(|| warm_embed(state)).await {
        Ok(got) => got,
        Err(e) => {
            eprintln!("index load failed: {e}");
            return unavailable_response(r#"{"error":"index_unavailable"}"#, RELOAD_WAIT);
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

/// `/index/filter/<type>/…` (`filter.rs`), off the request threads: the body, its `Cache-Control`, the
/// canonical URL to name in `Content-Location` when the request spelled another, and whether a kind was
/// unavailable.
///
/// A request spelled any way but the canonical one is ANSWERED, not redirected — den-edge's relay drops a
/// redirect's `Location`, so a 308 there reads as an empty answer — but privately and briefly, so no shared
/// cache holds a second copy under a second key, and `Content-Location` names the one URL to ask next time.
/// A degraded answer (a kind this atlas should answer and cannot yet, through a failure at runtime) is kept
/// five minutes rather than an hour.
async fn filter_answer(
    state: &Arc<AppState>,
    indexes: Arc<crate::queries::Indexes>,
    media_type: den_index::MediaType,
    route: crate::filter::Route,
    request: crate::filter::Request,
    rest: &str,
) -> Result<(String, &'static str, Option<String>, bool), String> {
    let export = state.titles.as_ref().and_then(|t| t.index());
    // The path is part of the key too: `counts` without `.json` answers, but as a second spelling.
    let segment = rest.rsplit('/').next().unwrap_or(rest);
    let canonical = request.canonical && segment.ends_with(".json");
    let location = (!canonical)
        .then(|| format!("{}.json{}", segment.strip_suffix(".json").unwrap_or(segment), request.query()));
    let (body, degraded) = tokio::task::spawn_blocking(move || {
        let context = crate::filter::Context::new(&indexes, media_type, export);
        let (body, degraded) = match route {
            crate::filter::Route::Counts => context.counts(&request),
            crate::filter::Route::Titles => context.titles(&request),
            crate::filter::Route::Values(spec) => context.values(spec, &request),
        };
        (body.to_string(), degraded)
    })
    .await
    .map_err(|e| e.to_string())?;
    let cache_control = if !canonical {
        "private, max-age=60"
    } else if degraded {
        "public, max-age=300"
    } else {
        "public, max-age=3600, stale-while-revalidate=86400"
    };
    Ok((body, cache_control, location, degraded))
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
    let filter = match &question {
        IndexQuestion::Filter { media_type, route } => match crate::filter::Request::parse(*route, query) {
            Ok(request) => Some((*media_type, *route, request)),
            Err(detail) => {
                return json_response(
                    serde_json::json!({ "error": "bad_request", "detail": detail }).to_string(),
                    StatusCode::BAD_REQUEST,
                )
            }
        },
        _ => None,
    };
    // A search's whole text goes to den-embed at once, while the indexes are got — and loaded, after an idle
    // spell — so a cold den-embed loads its model alongside them, not after (`query_answer`).
    let early = match (&question, &state.embed) {
        (IndexQuestion::Query, Some(_)) => {
            let whole = crate::search::normalized(&query_text(query, "q"));
            (whole.chars().count() >= 2).then(|| {
                let state = Arc::clone(state);
                tokio::spawn(async move { embed_query(&state, &whole).await })
            })
        }
        _ => None,
    };
    // A search's early embed already wakes den-embed.
    let (indexes, loaded_in) = match queries
        .get(|| {
            if early.is_none() {
                warm_embed(state)
            }
        })
        .await
    {
        Ok(got) => got,
        Err(e) => {
            eprintln!("index load failed: {e}");
            return unavailable_response(r#"{"error":"index_unavailable"}"#, RELOAD_WAIT);
        }
    };
    if let Some((media_type, route, request)) = filter {
        let answer = filter_answer(state, indexes, media_type, route, request, rest).await;
        let load = loaded_in.map(|d| format!("load;dur={}, ", ms(d))).unwrap_or_default();
        return match answer {
            Ok((body, cache_control, location, degraded)) => {
                let mut resp = serve_json(method, headers, body, cache_control, None).await;
                if let Some(location) = location.and_then(|l| header::HeaderValue::from_str(&l).ok()) {
                    resp.headers_mut().insert(header::CONTENT_LOCATION, location);
                }
                if degraded {
                    resp.headers_mut()
                        .insert(DEGRADED, header::HeaderValue::from_static("filter_kinds_unavailable"));
                }
                with_timing(resp, &format!("{load}total;dur={}", ms(started.elapsed())))
            }
            Err(e) => {
                eprintln!("filter answer failed: {e}");
                json_response(r#"{"error":"index_failed"}"#, StatusCode::INTERNAL_SERVER_ERROR)
            }
        };
    }
    // An answer is the dataset's — and, for search, den-embed's vector for the text, which is fixed for the
    // dataset's model — so it changes when the dataset does, at most once a day, and its ETag with it: fresh
    // for an hour, and served stale while it revalidates. A search that should have been ranked through
    // den-embed and wasn't is the exception: it stays short, so the ranked answer replaces it once den-embed
    // is back.
    let long = "public, max-age=3600, stale-while-revalidate=86400";
    // A tilted row is as cacheable as any other — it is a slice of an order fixed for (row, taste, weights),
    // so every page of it revalidates the same way — but its URL carries the household's liked and disliked
    // titles, and that does not belong in a shared cache's key store or a proxy's log. `private` keeps the
    // TTL and the ETag and moves the copy to the client that asked. A row with no taste in its URL keeps
    // `public` exactly as before.
    let mut cache_control = if query.contains(crate::plotrows::TILT_PREFIX) {
        "private, max-age=3600, stale-while-revalidate=86400"
    } else {
        long
    };
    let mut unembedded = |missed: bool| {
        if missed {
            cache_control = "public, max-age=300";
        }
    };
    let mut phases = String::new();
    let body = match question {
        IndexQuestion::Search => match search_answer(state, &indexes, query).await {
            Ok(body) => body,
            Err(e) => {
                eprintln!("semantic search unavailable: {e}");
                return unavailable_response(r#"{"error":"embed_unavailable"}"#, RELOAD_WAIT);
            }
        },
        IndexQuestion::Facets => {
            let (body, unranked) = facets_answer(state, &indexes, query).await;
            unembedded(unranked);
            body
        }
        IndexQuestion::Query => match query_answer(state, Arc::clone(&indexes), query, early).await {
            Ok((body, timing, missed)) => {
                unembedded(missed);
                phases = format!("{timing}, ");
                body
            }
            Err(e) => {
                eprintln!("search failed: {e}");
                return json_response(r#"{"error":"search_failed"}"#, StatusCode::INTERNAL_SERVER_ERROR);
            }
        },
        // The dataset alone, but tens of milliseconds of work for a row or a scan: off the request threads.
        question => {
            let titles = state.titles.as_ref().and_then(|t| t.index());
            let (indexes, query) = (Arc::clone(&indexes), query.to_owned());
            match tokio::task::spawn_blocking(move || question.answer(&indexes, titles.as_deref(), &query))
                .await
            {
                Ok(body) => body,
                Err(e) => {
                    eprintln!("index answer failed: {e}");
                    return json_response(r#"{"error":"index_failed"}"#, StatusCode::INTERNAL_SERVER_ERROR);
                }
            }
        }
    };
    let load = loaded_in.map(|d| format!("load;dur={}, ", ms(d))).unwrap_or_default();
    let resp = serve_json(method, headers, body, cache_control, None).await;
    with_timing(resp, &format!("{load}{phases}total;dur={}", ms(started.elapsed())))
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
        let mut resp = serve_json(method, headers, r#"{"metas":[]}"#.to_owned(), "no-store", None).await;
        resp.headers_mut().insert(DEGRADED, header::HeaderValue::from_static("title_index_building"));
        return resp;
    };
    let query = extra_value(extra, "search").map(|q| percent_decode(&q)).unwrap_or_default();
    let body = titles::metas_json(&index, &query, media_type);
    let searched = started.elapsed();
    let resp =
        serve_json(method, headers, body, "public, max-age=3600, stale-while-revalidate=3600", None).await;
    with_timing(resp, &format!("titles;dur={}, total;dur={}", ms(searched), ms(started.elapsed())))
}

/// The embedded landing/configure page — served through the conditional layer so it gets a strong ETag
/// + `If-None-Match`/304 for free, plus a modest TTL (the page changes only on redeploy).
async fn serve_html(method: &Method, headers: &axum::http::HeaderMap, html: &'static str) -> Response {
    serve(
        method,
        headers,
        Servable {
            etag_base: fnv1a(html),
            content_type: "text/html; charset=utf-8".to_owned(),
            cache_control: "public, max-age=3600, stale-while-revalidate=600".to_owned(),
            last_modified: None,
            body: Bytes::from_static(html.as_bytes()),
        },
    )
    .await
}

/// A JSON answer, always whole: a `Range` is ignored (RFC 9110 lets a server answer it with a 200), because the
/// prose guard can only judge a whole body and refuses a slice of one (`tos::guard_response`).
async fn serve_json(
    method: &Method,
    headers: &axum::http::HeaderMap,
    body: String,
    cache_control: &str,
    last_modified: Option<String>,
) -> Response {
    let mut headers = headers.clone();
    headers.remove(header::RANGE);
    serve(
        method,
        &headers,
        Servable {
            etag_base: fnv1a(&body),
            content_type: "application/json".to_owned(),
            cache_control: cache_control.to_owned(),
            last_modified,
            body: Bytes::from(body.into_bytes()),
        },
    )
    .await
}

/// First value of `key` in a `k=v&k2=v2` query string (the datasetVersion is hex, so no percent-decoding).
pub(crate) fn query_param(query: &str, key: &str) -> Option<String> {
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
pub(crate) fn percent_decode(s: &str) -> String {
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
        assert_eq!(health_body(true, true, false, false, false, false, false), r#"{"status":"ok"}"#);
    }

    #[test]
    fn health_stale_catalog_when_last_refresh_failed() {
        let body = health_body(true, false, false, false, false, false, false);
        assert!(body.contains(r#""status":"degraded""#));
        assert!(body.contains(r#""reason":"stale_catalog""#));
    }

    /// Facts that don't read leave every route answering, from labels and facets alone, so nothing but this says
    /// recommendations and search have lost them.
    #[test]
    fn health_reports_facts_that_did_not_read() {
        let body = health_body(true, true, false, true, false, false, false);
        assert!(body.contains(r#""reason":"facts_unusable""#), "{body}");
        // It ranks below every catalog and dataset state.
        assert!(health_body(true, true, true, true, false, false, false)
            .contains(r#""reason":"catalog_schema_suspect""#));
        assert!(health_body(false, true, false, true, false, false, false)
            .contains(r#""reason":"dataset_unavailable""#));
    }

    /// A partial schema break is a SUCCESSFUL refresh by every other measure — the row is short but
    /// non-empty, so it caches as complete and `fresh()` stays true. Without this state the only
    /// trace was a line on stderr that nothing reads.
    #[test]
    fn health_reports_a_suspected_schema_break() {
        let body = health_body(true, true, true, false, false, false, false);
        assert!(body.contains(r#""status":"degraded""#), "{body}");
        assert!(body.contains(r#""reason":"catalog_schema_suspect""#), "{body}");
        // It ranks BELOW the two that mean rows are missing entirely.
        assert!(health_body(true, false, true, false, false, false, false)
            .contains(r#""reason":"stale_catalog""#));
        assert!(health_body(false, true, true, false, false, false, false)
            .contains(r#""reason":"dataset_unavailable""#));
    }

    #[test]
    fn health_dataset_unavailable_when_dataset_missing() {
        // Dataset-unavailable outranks stale: even with a fresh catalog, no dataset is the reported reason.
        let body = health_body(false, true, false, false, false, false, false);
        assert!(body.contains(r#""reason":"dataset_unavailable""#));
        // …and it still takes precedence when the catalog is also stale (the more severe condition wins).
        assert!(health_body(false, false, true, false, false, false, false)
            .contains(r#""reason":"dataset_unavailable""#));
    }

    /// The descriptor used to embed absolute blob URLs built from the request's own forwarded
    /// host/scheme, so it named those headers in `Vary` or a shared cache handed one requester's
    /// chosen origin to everyone. It points at nothing now, so the body is the same for every
    /// requester and a `Vary` on it would split a cache for no reason.
    #[tokio::test]
    async fn the_descriptor_is_the_same_for_every_requester() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;

        let dir = std::env::temp_dir().join(format!("den-atlas-desc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        store_in(&dir);
        std::fs::write(
            dir.join("dataset.meta.json"),
            br#"{"datasetVersion":"t","taxonomyVersion":"t","embeddingModel":"m","dims":2,
                 "quantization":"int8","storeFile":"s.store"}"#,
        )
        .unwrap();
        let ds = crate::dataset::Dataset::load(&dir).expect("fixture dataset must load");

        let state = Arc::new(AppState::for_test(Some(ds)));
        let ask = |host: &'static str| {
            let req = HttpRequest::builder()
                .uri("/dataset.json")
                .header("x-forwarded-host", host)
                .header("x-forwarded-proto", "https")
                .body(Body::empty())
                .unwrap();
            handle(State(Arc::clone(&state)), req)
        };
        let mine = ask("atlas.example").await;
        assert_eq!(mine.status(), 200);
        assert!(mine.headers().get("vary").is_none(), "the descriptor still varies on the request");
        let mine = body_of(mine).await;
        assert!(!mine.contains("atlas.example"), "the descriptor still embeds the request origin: {mine}");
        assert_eq!(body_of(ask("someone.else").await).await, mine);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A one-title store, so a `Dataset::load` in a route test has the artifact a dataset needs. The
    /// tests it serves are about the manifest and the descriptor, not about what is in the store.
    fn store_in(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        let title = crate::store::fixture::Title {
            media: 0,
            tmdb_id: 1,
            plot: vec![0, 0],
            premise: vec![0, 0],
            ..crate::store::fixture::Title::default()
        };
        crate::store::fixture::write(&dir.join("s.store"), "v9", 2, &[title], &[]);
    }

    /// A loadable dataset for a route test. The old manifest shape, sidecar declarations and all:
    /// those keys are no longer read, and a release that still carries them must still serve.
    fn fixture(dir: &std::path::Path) -> crate::dataset::Dataset {
        store_in(dir);
        std::fs::write(
            dir.join("dataset.meta.json"),
            br#"{"datasetVersion":"v9","taxonomyVersion":"t","embeddingModel":"m","dims":2,
                 "quantization":"int8","storeFile":"s.store",
                 "labelsFile":"labels.json","labelsBytes":6,"labelsSha256":"a",
                 "vectorsFile":"vectors.bin","vectorsBytes":8,"vectorsSha256":"b",
                 "metadataFile":"meta.json","metadataBytes":2,"metadataSha256":"c",
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
        assert_eq!(hit.headers()["cache-control"], "public, max-age=3600, stale-while-revalidate=3600");
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

    /// Off by default: every playground path is a 404, whatever it carries, exactly like an unknown route.
    #[tokio::test]
    async fn the_playground_is_off_unless_enabled() {
        let state = index_state("den-atlas-playground-off");
        for path in [
            "/playground",
            "/playground/params.json",
            "/playground/similar/movie/1.json",
            "/playground/similar/movie/1.json?w_maker=2",
            "/playground/judged.json",
            "/playground/rows.json",
        ] {
            assert_eq!(get(&state, path).await.status(), 404, "{path}");
        }
        // Enabled without the index routes it is off too: there is nothing to rank with.
        let bare = Arc::new(AppState { playground: true, ..AppState::for_test(None) });
        assert_eq!(get(&bare, "/playground").await.status(), 404);
    }

    /// The page's title autocomplete: the fuzzy title index over both types in one answer, kept to titles
    /// the store has, with the store's title and year. Without the title index it is a 404, which the page
    /// reads as "search with /index/query.json".
    #[tokio::test]
    async fn the_playground_title_search_uses_the_title_index_when_there_is_one() {
        use den_titlesearch::{MediaType, TitleIndex, TitleRecord};
        let json = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();
        let Ok(mut state) = Arc::try_unwrap(index_state("den-atlas-playground-titles")) else {
            panic!("fresh state has one owner")
        };
        state.playground = true;
        let without = Arc::new(state);
        assert_eq!(get(&without, "/playground/titles.json?q=").await.status(), 404);
        let Ok(mut state) = Arc::try_unwrap(without) else { panic!("one owner") };
        let record = |tmdb_id, media_type, title: &str, popularity| TitleRecord {
            tmdb_id,
            media_type,
            title: title.into(),
            popularity,
        };
        let index = TitleIndex::build(vec![
            record(1, MediaType::Movie, "One", 10.0),
            // More popular, but not in the store: it can have no row, so it is not offered.
            record(999, MediaType::Movie, "One Piece", 90.0),
        ]);
        state.titles = Some(Arc::new(crate::titles::TitleSearch::with_index(index)));
        let on = Arc::new(state);
        let probe = get(&on, "/playground/titles.json?q=").await;
        assert_eq!(probe.status(), 200);
        assert_eq!(probe.headers()["cache-control"], "no-store");
        let hits = json(body_of(get(&on, "/playground/titles.json?q=one").await).await);
        let keys: Vec<&str> =
            hits["titles"].as_array().unwrap().iter().map(|t| t["key"].as_str().unwrap()).collect();
        assert_eq!(keys, ["movie:1"], "{hits}");
        assert_eq!(hits["titles"][0]["title"], "One");
    }

    /// On, the page and its knobs are served, a tuned row answers `no-store`, and a bad value is a 400
    /// naming the parameter. The production route never reads an override, on or off.
    #[tokio::test]
    async fn the_playground_tunes_only_its_own_route() {
        let off = index_state("den-atlas-playground-a");
        let on = {
            let Ok(mut state) = Arc::try_unwrap(index_state("den-atlas-playground-b")) else {
                unreachable!()
            };
            state.playground = true;
            Arc::new(state)
        };
        let json = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();

        let page = get(&on, "/playground").await;
        assert_eq!(page.status(), 200);
        assert!(body_of(page).await.contains("Tuning playground"));
        let knobs = json(body_of(get(&on, "/playground/params.json").await).await);
        let maker = knobs["knobs"].as_array().unwrap().iter().find(|k| k["name"] == "w_maker").unwrap();
        assert_eq!(maker["default"], 1.2, "the form is pre-filled with production's value");

        let tuned = get(&on, "/playground/similar/movie/1.json?limit=200").await;
        assert_eq!(tuned.headers()["cache-control"], "no-store");
        let tuned = json(body_of(tuned).await);
        let production = json(body_of(get(&on, "/index/similar/movie/1.json?limit=200").await).await);
        let ids: Vec<&serde_json::Value> =
            tuned["titles"].as_array().unwrap().iter().map(|t| &t["id"]).collect();
        assert_eq!(serde_json::json!(ids), production["ids"], "default parameters are production's row");
        assert_eq!(tuned["changed"], serde_json::json!([]));
        assert!(tuned["titles"][0]["signals"]["maker"]["points"].is_number(), "{tuned}");

        let short = json(body_of(get(&on, "/playground/similar/movie/1.json?max_row=1").await).await);
        assert_eq!(short["titles"].as_array().unwrap().len(), 1);
        assert_eq!(short["changed"], serde_json::json!(["max_row"]));

        for (query, name) in [("w_maker=11", "w_maker"), ("w_makr=1", "w_makr"), ("pool_k=1.5", "pool_k")] {
            let resp = get(&on, &format!("/playground/similar/movie/1.json?{query}")).await;
            assert_eq!(resp.status(), 400, "{query}");
            assert!(body_of(resp).await.contains(name), "{query}");
        }

        // The judged-set score: every case, split into its halves, production's means beside the tuned ones.
        let judged = get(&on, "/playground/judged.json").await;
        assert_eq!(judged.headers()["cache-control"], "no-store");
        let judged = json(body_of(judged).await);
        let cases = judged["cases"].as_array().unwrap().len();
        assert_eq!(cases, crate::raileval::embedded().len());
        assert_eq!(
            judged["dev"]["n"].as_u64().unwrap() + judged["test"]["n"].as_u64().unwrap(),
            cases as u64
        );
        assert_eq!(judged["dev"]["tuned"], judged["dev"]["production"], "untuned is production");
        let moved = json(body_of(get(&on, "/playground/judged.json?w_maker=0").await).await);
        assert_eq!(moved["changed"], serde_json::json!(["w_maker"]));
        let bad = get(&on, "/playground/judged.json?w_makr=0").await;
        assert_eq!(bad.status(), 400);

        // Several seeds in one request, each row the one `/playground/similar` gives it alone.
        let rows = get(&on, "/playground/rows.json?seeds=movie:1,series:4&w_maker=0&limit=5").await;
        assert_eq!(rows.headers()["cache-control"], "no-store");
        let rows = json(body_of(rows).await);
        assert_eq!(rows["changed"], serde_json::json!(["w_maker"]));
        let keys: Vec<&serde_json::Value> =
            rows["rows"].as_array().unwrap().iter().map(|r| &r["key"]).collect();
        assert_eq!(serde_json::json!(keys), serde_json::json!(["movie:1", "series:4"]));
        let alone = json(body_of(get(&on, "/playground/similar/movie/1.json?w_maker=0&limit=5").await).await);
        assert_eq!(rows["rows"][0]["titles"], alone["titles"]);
        assert!(rows.get("judged").is_none(), "the judged scores only when asked for");
        let with_judged =
            json(body_of(get(&on, "/playground/rows.json?seeds=movie:1&w_maker=0&judged=1").await).await);
        assert_eq!(with_judged["judged"], moved, "the same answer judged.json gives");
        let defaults = json(body_of(get(&on, "/playground/rows.json").await).await);
        assert_eq!(defaults["rows"].as_array().unwrap().len(), crate::playground::DEFAULT_SEEDS.len());
        assert_eq!(get(&on, "/playground/rows.json?seeds=movie:1&limit=50").await.status(), 200);
        let none = json(body_of(get(&on, "/playground/rows.json?seeds=").await).await);
        assert_eq!(none["rows"], serde_json::json!([]));
        let many = (1..=13).map(|id| format!("movie:{id}")).collect::<Vec<_>>().join(",");
        for query in [
            format!("/playground/rows.json?seeds={many}"),
            "/playground/similar/movie/1.json?seeds=movie:1".into(),
            // Per seed, rows.json shows at most 50, though one seed's route shows up to 200.
            "/playground/rows.json?seeds=movie:1&limit=51".into(),
        ] {
            assert_eq!(get(&on, &query).await.status(), 400, "{query}");
        }

        // Filters and watched titles. Movie 2 is `tone=bleak`, movie 3 `tone=comic`.
        let ids = |answer: &serde_json::Value| -> Vec<u64> {
            answer["titles"].as_array().unwrap().iter().map(|t| t["id"].as_u64().unwrap()).collect()
        };
        let plain = json(body_of(get(&on, "/playground/similar/movie/1.json").await).await);
        assert!(ids(&plain).contains(&2) && ids(&plain).contains(&3), "{plain}");
        let bleak = json(body_of(get(&on, "/playground/similar/movie/1.json?filter.tone=bleak").await).await);
        assert_eq!(ids(&bleak), [2], "a filter keeps only what carries it");
        let seen = json(body_of(get(&on, "/playground/similar/movie/1.json?watched=movie:2").await).await);
        assert!(!ids(&seen).contains(&2), "a watched title is dropped");
        assert_eq!(seen["tilted"], true);
        assert_eq!(plain["tilted"], false);

        // You Might Also Like rides in rows.json: the suggest seeds' pooled rows, minus the seeds.
        let suggested = json(body_of(get(&on, "/playground/rows.json?seeds=&suggest=movie:1").await).await);
        let pooled: Vec<&str> = suggested["suggest"]["titles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["key"].as_str().unwrap())
            .collect();
        assert!(!pooled.is_empty() && !pooled.contains(&"movie:1"), "{suggested}");
        assert_eq!(suggested["suggest"]["titles"][0]["production"], 1, "untuned, production's pool");

        // Export, then import: the same state, and a snapshot the import ignores.
        let query = "seeds=movie:1&suggest=&w_maker=0.5&watched=movie:3&filter.ending=bittersweet&limit=5";
        let exported =
            json(body_of(get(&on, &format!("/playground/export.json?{query}&judged=1")).await).await);
        assert_eq!(exported["format"], "den-atlas-playground");
        assert_eq!(exported["snapshot"]["changed"], serde_json::json!(["w_maker"]));
        assert_eq!(exported["snapshot"]["rows"][0]["key"], "movie:1");
        assert!(exported["snapshot"]["judged"]["dev"]["n"].is_number(), "asked for, so judged");
        assert!(exported["snapshot"]["at"].as_str().unwrap().ends_with('Z'));
        let imported = post(&on, "/playground/import.json", &exported.to_string()).await;
        assert_eq!(imported.status(), 200);
        let imported = json(body_of(imported).await);
        assert_eq!(imported["state"], exported["state"]);
        assert_eq!(
            crate::playground::State::parse(imported["query"].as_str().unwrap()).unwrap(),
            crate::playground::State::parse(query).unwrap()
        );
        let refused = post(&on, "/playground/import.json", r#"{"format":"x"}"#).await;
        assert_eq!(refused.status(), 400);
        assert_eq!(post(&off, "/playground/import.json", "{}").await.status(), 404, "off is off");

        // An override on the production route is ignored with the playground on or off.
        for state in [&on, &off] {
            let plain = body_of(get(state, "/index/similar/movie/1.json").await).await;
            let asked = body_of(get(state, "/index/similar/movie/1.json?max_row=1&w_maker=0").await).await;
            assert_eq!(asked, plain);
        }
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
        // The labelling pass's version is NOT in the store; both routes that name it read it off the
        // index, which the load stamps from the manifest. Miss that and these answer "".
        assert_eq!(taxonomy["taxonomyVersion"], "t02", "{taxonomy}");
        // A bare name list is not the dataset's description; it points at the route that is.
        let pointer = taxonomy["schema"].as_str().expect("taxonomy names the schema route");
        let schema = get(&state, pointer).await;
        assert_eq!(schema.status(), 200, "{pointer}");
        let schema = json(body_of(schema).await);
        assert_eq!(schema["taxonomyVersion"], "t02", "{schema}");
        // 12 titles, 8 of them unlabelled: `count` and `denominator` must differ, or a client reports a
        // fraction of the corpus as though it were the whole of it.
        assert_eq!(schema["population"]["count"], 12);
        assert_eq!(
            schema["fields"]["tone"]["coverage"],
            serde_json::json!({
                "count": 3, "denominator": 12, "ratio": 0.25
            })
        );
        assert_eq!(schema["fields"]["mood"]["coverage"]["count"], 1);

        for (path, want) in [
            ("/index/rows/movie/subgenre/Heist.json", serde_json::json!([1, 2, 3])),
            ("/index/rows/movie/subgenre/Heist.json?skip=1&limit=1", serde_json::json!([2])),
            ("/index/rows/series/subgenre/Heist.json", serde_json::json!([4])),
            ("/index/rows/movie/subgenre/Campy%2FCult.json", serde_json::json!([3])),
            ("/index/rows/movie/mood/Tense.json", serde_json::json!([1])),
            // The POOLED scorer, which is the only one now: the store is the dataset, so there is no
            // store-less arm left for a fixture to fall into. Movie 2 leads — it is a strong PLOT
            // neighbour (90,10,0 against 100,0,0) and shares movie 1's Drama, where movie 3 is a premise
            // neighbour in another genre. Drawing candidates from both spaces is the whole point of
            // pooling; the pre-pooled scorer drew them from the premise index alone and answered 3, 2.
            ("/index/similar/movie/1.json", serde_json::json!([2, 3])),
            // The rail is scrolled, so it pages. Absent both params it answers the first screenful, which
            // is what every existing caller expects.
            ("/index/similar/movie/1.json?skip=1", serde_json::json!([3])),
            ("/index/similar/movie/1.json?limit=1", serde_json::json!([2])),
            ("/index/similar/movie/1.json?skip=1&limit=1", serde_json::json!([3])),
            ("/index/similar/movie/1.json?skip=99", serde_json::json!([])),
        ] {
            let answer = json(body_of(get(&state, path).await).await);
            assert_eq!(answer["ids"], want, "{path}");
            if path.starts_with("/index/similar/") {
                // `total` is the whole row, not the page, so a client knows whether to keep scrolling.
                assert_eq!(answer["total"], 2, "{path}");
            }
            if path.starts_with("/index/rows/") {
                // Titles of that type in the CORPUS: three movies, nine series. It used to read 1 for
                // series, because the denominator came from `facets.bin` and that blob described four of
                // the twelve titles — so "1 of 1 series is a heist" where the honest answer is 1 of 9.
                let denominator = if path.contains("/series/") { 9 } else { 3 };
                assert_eq!(answer["coverage"]["denominator"], denominator, "{path}");
            }
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

    /// A stand-in den-embed answering every request with `vector`, and how many texts other than the warm-up
    /// it was asked to embed.
    async fn fake_embed(vector: &'static str) -> (crate::EmbedProxy, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&asked);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                // Until the JSON body's closing brace: the head and the body can arrive in separate reads.
                let mut seen = Vec::new();
                let mut buf = vec![0u8; 16 * 1024];
                while !seen.contains(&b'}') {
                    match sock.read(&mut buf).await {
                        Ok(n) if n > 0 => seen.extend_from_slice(&buf[..n]),
                        _ => break,
                    }
                }
                if !String::from_utf8_lossy(&seen).contains(r#""text":"warm""#) {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
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
        let proxy = crate::EmbedProxy {
            client: reqwest::Client::new(),
            base: format!("http://{addr}"),
            inflight: Arc::new(tokio::sync::Semaphore::new(4)),
        };
        (proxy, asked)
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
            embed: Some(fake_embed("[0,100,0]").await.0),
            ..AppState::for_test(Some(ds))
        });
        let json = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();

        let neighbours = json(body_of(get(&state, "/index/neighbours/movie/1.json").await).await);
        assert_eq!(neighbours["ids"], serde_json::json!([2, 3]));
        // The embedded query sits on movie 3.
        let search = get(&state, "/index/search.json?q=campy+fun").await;
        assert_eq!(
            search.headers()["cache-control"],
            "public, max-age=3600, stale-while-revalidate=86400",
            "an embedded answer changes only with the dataset"
        );
        let search = json(body_of(search).await);
        assert_eq!(
            (&search["titles"][0]["type"], &search["titles"][0]["id"]),
            (&serde_json::json!("movie"), &serde_json::json!(3))
        );
        // Each hit carries its score, and the scan its spread, so a client can floor what barely stands out.
        assert_eq!(search["titles"][0]["score"], 10_000);
        assert!(search["mean"].is_f64() && search["sd"].as_f64().unwrap() > 0.0, "{search}");

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

    /// A text searched again is answered from the memo, not by another den-embed call — on every route that
    /// embeds it.
    #[tokio::test]
    async fn a_repeated_search_embeds_once() {
        let dir = std::env::temp_dir().join(format!("den-atlas-memo-{}", std::process::id()));
        let ds = crate::queries::write_fixture(&dir);
        let index = Arc::new(crate::queries::IndexQueries::new(&ds));
        let (proxy, asked) = fake_embed("[0,100,0]").await;
        let state =
            Arc::new(AppState { index: Some(index), embed: Some(proxy), ..AppState::for_test(Some(ds)) });
        let asked = || asked.load(std::sync::atomic::Ordering::SeqCst);
        let first = get(&state, "/index/search.json?q=campy+fun").await;
        assert_eq!(first.status(), 200);
        assert_eq!(asked(), 1);
        let again = body_of(get(&state, "/index/search.json?q=campy+fun").await).await;
        assert_eq!(body_of(first).await, again, "the remembered vector ranked differently");
        assert_eq!(asked(), 1, "the same text was embedded twice");
        get(&state, "/index/search.json?q=something+else").await;
        assert_eq!(asked(), 2, "a new text was answered from the memo");
        assert_eq!(state.embed_memo.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A search den-embed should have ranked but could not is cached briefly, so the ranked answer replaces it
    /// once den-embed is back; one it had nothing to rank is cached like any dataset answer.
    #[tokio::test]
    async fn an_unembedded_search_is_cached_briefly() {
        let dir = std::env::temp_dir().join(format!("den-atlas-unembedded-{}", std::process::id()));
        let ds = crate::queries::write_fixture(&dir);
        let index = Arc::new(crate::queries::IndexQueries::new(&ds));
        // Nothing listens on the discard port, so every embed fails at once.
        let down = crate::EmbedProxy {
            client: reqwest::Client::new(),
            base: "http://127.0.0.1:9".to_owned(),
            inflight: Arc::new(tokio::sync::Semaphore::new(4)),
        };
        let state =
            Arc::new(AppState { index: Some(index), embed: Some(down), ..AppState::for_test(Some(ds)) });
        for path in ["/index/query.json?q=zzzz", "/index/facets.json?q=korean+heist"] {
            let resp = get(&state, path).await;
            assert_eq!(resp.status(), 200, "{path}");
            assert_eq!(resp.headers()["cache-control"], "public, max-age=300", "{path}");
        }
        let unthemed = get(&state, "/index/facets.json?q=korean+movies").await;
        assert_eq!(unthemed.headers()["cache-control"], "public, max-age=3600, stale-while-revalidate=86400");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Search in one request: an exact title returns only scored query matches, a typo still finds its title, a
    /// country filters, a label and a plot facet each propose and lift their titles — and the reading comes back.
    #[tokio::test]
    async fn query_searches_titles_facets_labels_and_plot_facets_in_one_ranking() {
        let state = index_state("den-atlas-query");
        let json = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();
        let ask = |q: &str| format!("/index/query.json?q={}", q.replace(' ', "+"));
        let keys = |answer: &serde_json::Value| -> Vec<String> {
            answer["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| format!("{}:{}", h["type"].as_str().unwrap(), h["id"]))
                .collect()
        };

        // "One" is exact. More Like This for movie 1 is 3 then 2, but neither matched this query and search
        // must not reinsert them with score zero merely to fill the page.
        let one = json(body_of(get(&state, &ask("one")).await).await);
        assert_eq!(keys(&one), ["movie:1"], "{one}");
        assert!(one["hits"].as_array().unwrap().iter().all(|h| h["score"].as_f64().unwrap() > 0.0));
        assert_eq!(one["hits"][0]["title"], "One");
        assert_eq!(one["hits"][0]["posterPath"], "/1.jpg");
        assert_eq!(one["hits"][0]["imdbId"], "tt0000001", "from the facts, so a client needn't ask TMDB");
        assert!(one["hits"][0]["f"]["t"].as_f64().unwrap() >= 0.6);

        let typo = json(body_of(get(&state, &ask("thre")).await).await);
        assert_eq!(keys(&typo)[0], "movie:3", "{typo}");

        // Korean films only, most voted first; the Korean series is another type.
        let korean = json(body_of(get(&state, &ask("korean movies")).await).await);
        assert_eq!(keys(&korean), ["movie:2", "movie:1"], "{korean}");
        assert_eq!(korean["parse"]["country"], "KR");

        // The Heist label proposes all four; the series carries it most confidently.
        let heist = json(body_of(get(&state, &ask("heist")).await).await);
        assert_eq!(keys(&heist)[0], "series:4", "{heist}");
        assert_eq!(heist["parse"]["labels"], serde_json::json!(["Heist"]));

        // Bleak: movie 1 is sure of it, movie 2 only a little; nothing else is proposed.
        let bleak = json(body_of(get(&state, &ask("bleak")).await).await);
        assert_eq!(keys(&bleak), ["movie:1", "movie:2"], "{bleak}");
        assert_eq!(bleak["parse"]["plotFacets"], serde_json::json!(["tone=bleak"]));

        let empty = json(body_of(get(&state, &ask("zzzz")).await).await);
        assert_eq!(empty["hits"], serde_json::json!([]));

        // Someone behind a title: the title, and who they are.
        let director = json(body_of(get(&state, &ask("a director")).await).await);
        assert_eq!(keys(&director)[0], "movie:1", "{director}");
        assert_eq!(director["people"][0]["name"], "A Director");
        assert_eq!(director["people"][0]["id"], 11);
        assert_eq!(director["hits"][0]["f"]["p"], 1.0);

        // A title by another of its names.
        let uno = json(body_of(get(&state, &ask("uno")).await).await);
        assert_eq!(keys(&uno)[0], "movie:1", "{uno}");
        assert_eq!(uno["people"], serde_json::json!([]));
    }

    /// Unified query search scans the optional premise index with the same query vector, admits a title only
    /// that representation found, and remains plot-semantic when a dataset does not ship the optional pair.
    #[tokio::test]
    async fn query_search_uses_premise_semantics_and_preserves_plot_only_fallback() {
        let answer = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();
        let dir = std::env::temp_dir().join(format!("den-atlas-query-premise-{}", std::process::id()));
        let ds = crate::queries::write_fixture(&dir);
        let index = Arc::new(crate::queries::IndexQueries::new(&ds));
        let state = Arc::new(AppState {
            index: Some(index),
            embed: Some(fake_embed("[0,100,0]").await.0),
            ..AppState::for_test(Some(ds))
        });
        let with_premise = answer(body_of(get(&state, "/index/query.json?q=zzzz").await).await);
        let first = &with_premise["hits"][0];
        assert_eq!((&first["type"], &first["id"]), (&serde_json::json!("movie"), &serde_json::json!(2)));
        assert_eq!(first["f"]["semPlot"], 0.0, "movie 2 is not a strong plot-vector answer");
        assert!(first["f"]["semPremise"].as_f64().unwrap() > 0.0, "{with_premise}");

        let dir = std::env::temp_dir().join(format!("den-atlas-query-plot-only-{}", std::process::id()));
        // A store with no premise vectors. Dropping the premise BLOBS from the dataset used to do this;
        // the premise index is a section of the store now, so the store is what has to lack it.
        let ds = crate::queries::write_fixture_plot_only(&dir);
        let index = Arc::new(crate::queries::IndexQueries::new(&ds));
        let state = Arc::new(AppState {
            index: Some(index),
            embed: Some(fake_embed("[0,100,0]").await.0),
            ..AppState::for_test(Some(ds))
        });
        let plot_only = answer(body_of(get(&state, "/index/query.json?q=zzzz").await).await);
        let first = &plot_only["hits"][0];
        assert_eq!((&first["type"], &first["id"]), (&serde_json::json!("movie"), &serde_json::json!(3)));
        assert!(first["f"]["semPlot"].as_f64().unwrap() > 0.0, "{plot_only}");
        assert_eq!(first["f"]["semPremise"], 0.0);
    }

    /// A search's `total` is a retrieval pool, and the answer itself must say so and say how much of the corpus
    /// could answer each constraint: a parameter filters, a word read as a country only discounts, a plot facet
    /// only lifts — each out of the titles of the type asked for, and a series-only fact out of the series.
    #[tokio::test]
    async fn a_search_reports_each_constraint_and_what_its_coverage_is_out_of() {
        let state = index_state("den-atlas-query-coverage");
        let json = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();
        let answer = json(
            body_of(
                get(
                    &state,
                    "/index/query.json?q=bleak+korean+movies&language=ko&runtime_max=90&broadcaster=Q7",
                )
                .await,
            )
            .await,
        );
        assert_eq!(answer["semantics"]["total"], "retrievedCandidatesNotCorpusCount", "{answer}");
        let coverage = &answer["coverage"];
        assert_eq!(coverage["population"], 12);
        assert_eq!(coverage["mediaType"], "movie");
        assert_eq!(coverage["denominator"], 3, "the fixture's three films");
        let field = |name: &str| &coverage["fields"][name];
        assert_eq!(field("country")["value"], "KR");
        assert_eq!(field("country")["applied"], "discount");
        assert_eq!((&field("country")["count"], &field("country")["denominator"]), (&3.into(), &3.into()));
        assert_eq!(field("language")["applied"], "filter");
        assert_eq!(field("mediaType")["applied"], "filter");
        assert_eq!(field("tone")["value"], serde_json::json!(["bleak"]));
        assert_eq!(field("tone")["applied"], "boost");
        assert_eq!(field("tone")["count"], 3, "every film has a tone on record");
        assert_eq!(field("runtimeMinutes")["appliesTo"], "movie");
        assert_eq!(field("broadcaster")["applied"], "require");
        assert_eq!(field("broadcaster")["denominator"], 9, "out of the series, whatever type was asked for");

        // Nothing named: no constraint, and the whole corpus is the denominator.
        let plain = json(body_of(get(&state, "/index/query.json?q=zzzz").await).await);
        assert_eq!(plain["coverage"]["fields"], serde_json::json!({}));
        assert_eq!(plain["coverage"]["denominator"], 12);
        assert_eq!(plain["coverage"]["mediaType"], serde_json::Value::Null);
    }

    /// Every route the schema lists with an example resolves, so the document cannot advertise a path the router
    /// does not answer.
    #[tokio::test]
    async fn every_route_the_schema_lists_resolves() {
        let state = index_state("den-atlas-schema-routes");
        let schema: serde_json::Value =
            serde_json::from_str(&body_of(get(&state, "/index/schema.json").await).await).unwrap();
        let examples: Vec<&str> =
            schema["routes"].as_array().unwrap().iter().filter_map(|r| r["example"].as_str()).collect();
        assert!(examples.len() >= 8, "{examples:?}");
        for example in examples {
            let status = get(&state, example).await.status();
            // Semantic search answers 503 without den-embed, which this state has none of: routed, not missing.
            assert!(status == 200 || status == 503, "{example} answered {status}");
        }
    }

    /// A JSON answer is always whole. A slice of one cannot be audited for prose, so a Range is answered with
    /// the full body rather than a 206 the guard would have to refuse.
    #[tokio::test]
    async fn a_range_on_a_json_route_answers_the_whole_body() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        let state = index_state("den-atlas-json-range");
        let req = HttpRequest::builder().uri("/index/schema.json").header("range", "bytes=0-9");
        let resp = handle(State(Arc::clone(&state)), req.body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), 200);
        assert!(serde_json::from_str::<serde_json::Value>(&body_of(resp).await).is_ok());
    }

    /// A word may be both a facet and a title. Reading `brazil` as country BR must not multiply the exact
    /// title's whole score by WRONG_TEXT_FACET merely because Gilliam's film is recorded under other countries.
    #[tokio::test]
    async fn query_search_keeps_the_exact_title_floor_across_an_inferred_facet_collision() {
        let dir = std::env::temp_dir().join(format!("den-atlas-query-brazil-{}", std::process::id()));
        let ds = crate::queries::write_fixture_titled(&dir, "Brazil");
        let index = Arc::new(crate::queries::IndexQueries::new(&ds));
        let state = Arc::new(AppState {
            index: Some(index),
            embed: Some(fake_embed("[0,0,0]").await.0),
            ..AppState::for_test(Some(ds))
        });
        let answer: serde_json::Value =
            serde_json::from_str(&body_of(get(&state, "/index/query.json?q=brazil").await).await).unwrap();
        let first = &answer["hits"][0];
        assert_eq!((&first["type"], &first["id"]), (&serde_json::json!("movie"), &serde_json::json!(1)));
        assert!(first["score"].as_f64().unwrap() >= 1.2, "{answer}");
        assert_eq!(first["f"]["phi"], 1.0, "{answer}");
    }

    /// A plot facet row: the titles carrying every facet named, most confident then most voted, drawn as cards
    /// with what a client's hide rules read, paged — and empty, not an error, for a facet nobody carries.
    #[tokio::test]
    async fn plot_rows_answer_from_the_plot_facets() {
        let state = index_state("den-atlas-plot-rows");
        let json = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();
        let ids = |answer: &serde_json::Value| -> Vec<u64> {
            answer["titles"].as_array().unwrap().iter().map(|t| t["id"].as_u64().unwrap()).collect()
        };
        // High confidence first (2 and 3), then by votes: 2 has 500, 3 has 50; movie 1 is only medium.
        let bittersweet = json(body_of(get(&state, "/index/plot/movie.json?ending=bittersweet").await).await);
        assert_eq!(ids(&bittersweet), vec![2, 3, 1]);
        assert_eq!(bittersweet["total"], 3);
        assert_eq!(bittersweet["titles"][0]["title"], "Two");
        assert_eq!(bittersweet["titles"][0]["posterPath"], "/2.jpg");
        // Movie 1: primary genre Drama, and crime and drama from its facts.
        assert_eq!(bittersweet["titles"][2]["genreIds"], serde_json::json!([18, 80]));
        // `primaryGenre` is the one name a client DISPLAYS, beside the ids its hide rules filter on. The
        // key is always present, `null` for a title the corpus does not label, so a client can tell "atlas
        // does not know this one" from "this atlas is too old to say".
        assert_eq!(bittersweet["titles"][0]["primaryGenre"], "Drama");
        assert_eq!(bittersweet["titles"][1]["primaryGenre"], "Comedy", "movie 3 is the Comedy one");
        assert!(
            bittersweet["titles"][0].as_object().unwrap().contains_key("primaryGenre"),
            "the key is present even when its value is null"
        );
        let paged = json(
            body_of(get(&state, "/index/plot/movie.json?ending=bittersweet&skip=1&limit=1").await).await,
        );
        assert_eq!(ids(&paged), vec![3]);
        let bleak =
            json(body_of(get(&state, "/index/plot/movie.json?ending=bittersweet&tone=bleak").await).await);
        assert_eq!(ids(&bleak), vec![1, 2], "movie 1 is medium on both; movie 2 is low on tone");
        let series = json(body_of(get(&state, "/index/plot/series.json?ending=bittersweet").await).await);
        assert_eq!(ids(&series), vec![4]);
        for path in ["/index/plot/movie.json?ending=sad", "/index/plot/movie.json"] {
            let resp = get(&state, path).await;
            assert_eq!(resp.status(), 200, "{path}");
            assert_eq!(json(body_of(resp).await)["titles"], serde_json::json!([]), "{path}");
        }
        assert_eq!(get(&state, "/index/plot/anime.json?ending=happy").await.status(), 404);
    }

    /// A merged row answers like any row: the union of its members, in the row order, paged, with the
    /// axis's coverage. Every drawable fixture title ends bittersweet, so `unhappy` is that row exactly.
    #[tokio::test]
    async fn a_merged_row_answers_and_pages_like_any_row() {
        let state = index_state("den-atlas-merged-rows");
        let json = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();
        let ids = |answer: &serde_json::Value| -> Vec<u64> {
            answer["titles"].as_array().unwrap().iter().map(|t| t["id"].as_u64().unwrap()).collect()
        };
        let member = json(body_of(get(&state, "/index/row/movie.json?ending=bittersweet").await).await);
        let merged = json(body_of(get(&state, "/index/row/movie.json?ending=unhappy").await).await);
        assert_eq!(ids(&merged), vec![2, 3, 1]);
        assert_eq!(merged["titles"], member["titles"]);
        assert_eq!(merged["total"], 3);
        assert_eq!(merged["coverage"]["fields"]["ending"], member["coverage"]["fields"]["ending"]);
        let mut paged = Vec::new();
        for skip in 0..4 {
            let page = json(
                body_of(
                    get(&state, &format!("/index/row/movie.json?ending=unhappy&skip={skip}&limit=1")).await,
                )
                .await,
            );
            assert_eq!(page["total"], 3, "every page names the whole row's total");
            paged.extend(ids(&page));
        }
        assert_eq!(paged, vec![2, 3, 1], "the pages tile the row");
        let series = json(body_of(get(&state, "/index/row/series.json?ending=unhappy").await).await);
        assert_eq!(ids(&series), vec![4]);
        let bleak = json(body_of(get(&state, "/index/row/movie.json?ending=unhappy&tone=bleak").await).await);
        assert_eq!(ids(&bleak), vec![1, 2]);
        let none = json(body_of(get(&state, "/index/row/movie.json?ending=unresolved").await).await);
        assert_eq!(
            (none["titles"].clone(), none["total"].clone()),
            (serde_json::json!([]), serde_json::json!(0))
        );
    }

    /// A row may name a mood or subgenre, alone or with the plot facets: the surest first, then the most voted.
    #[tokio::test]
    async fn rows_name_labels_alone_or_with_plot_facets() {
        let state = index_state("den-atlas-label-rows");
        let json = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();
        let ids = |answer: &serde_json::Value| -> Vec<u64> {
            answer["titles"].as_array().unwrap().iter().map(|t| t["id"].as_u64().unwrap()).collect()
        };
        // Heist: movies 1 (0.9) and 2 (0.8) are sure of it, 2 has more votes; movie 3 only just (0.6).
        let heist = json(body_of(get(&state, "/index/row/movie.json?subgenre=Heist").await).await);
        assert_eq!(ids(&heist), vec![2, 1, 3], "{heist}");
        assert_eq!(heist["total"], 3);
        assert_eq!(heist["titles"][0]["title"], "Two");
        assert_eq!(heist["titles"][1]["imdbId"], "tt0000001");
        assert!(heist["titles"][0].get("imdbId").is_none(), "a title the facts give no IMDb id carries none");
        // With bleak: movie 1 is sure of both, movie 2 low on tone; movie 3 isn't bleak.
        let bleak = json(body_of(get(&state, "/index/row/movie.json?subgenre=Heist&tone=bleak").await).await);
        assert_eq!(ids(&bleak), vec![1, 2], "{bleak}");
        let tense = json(body_of(get(&state, "/index/row/movie.json?mood=Tense").await).await);
        assert_eq!(ids(&tense), vec![1]);
        let cult = json(body_of(get(&state, "/index/row/movie.json?subgenre=Campy%2FCult").await).await);
        assert_eq!(ids(&cult), vec![3], "a label with a slash, percent-encoded");
        let series = json(body_of(get(&state, "/index/row/series.json?subgenre=Heist").await).await);
        assert_eq!(ids(&series), vec![4]);
        for path in ["/index/row/series.json?mood=Tense", "/index/row/movie.json?mood=Nope"] {
            assert_eq!(
                json(body_of(get(&state, path).await).await)["titles"],
                serde_json::json!([]),
                "{path}"
            );
        }
    }

    /// Filter counts at the URLs Den Web builds: a canonical one is public, carries an ETag and revalidates to
    /// a 304; any other spelling of the same question is answered — never redirected, since den-edge's relay
    /// drops `Location` — with the same body, privately and briefly, naming the canonical URL in
    /// `Content-Location`; a malformed one is a 400 before anything loads.
    #[tokio::test]
    async fn filter_counts_answer_every_spelling_and_name_the_canonical_one() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        let state = index_state("den-atlas-filter-counts");
        let json = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();

        let all = get(&state, "/index/filter/movie/counts.json").await;
        assert_eq!(all.status(), 200);
        assert_eq!(
            all.headers()[header::CACHE_CONTROL],
            "public, max-age=3600, stale-while-revalidate=86400"
        );
        assert!(all.headers().get(DEGRADED).is_none(), "every kind this state offers answers");
        let all = json(body_of(all).await);
        assert_eq!(all["total"], 3);
        assert_eq!(all["kinds"]["country"]["values"], serde_json::json!({ "DK": 1, "ES": 1, "KR": 2 }));
        assert_eq!(all["kinds"]["country"]["mode"], "and");
        assert_eq!(all["kinds"]["decade"]["mode"], "single");
        assert_eq!(all["ignored"], serde_json::json!([]));

        // The web's own URL for Korea and Heist, as `facetCountsUrl` writes it.
        let canonical = "/index/filter/movie/counts.json?sel=country:KR,subgenre:Heist";
        let korean = get(&state, canonical).await;
        let etag = korean.headers()[header::ETAG].clone();
        assert!(korean.headers().get(header::CONTENT_LOCATION).is_none());
        let korean_body = body_of(korean).await;
        let korean = json(korean_body.clone());
        assert_eq!(korean["total"], 2);
        assert_eq!(korean["kinds"]["country"]["values"], serde_json::json!({ "DK": 1, "KR": 2 }));
        assert_eq!(korean["kinds"]["country"]["selected"], serde_json::json!(["KR"]));
        assert_eq!(korean["kinds"]["mood"]["values"], serde_json::json!({ "Tense": 1 }));
        let req = HttpRequest::builder().uri(canonical).header("if-none-match", etag);
        assert_eq!(handle(State(Arc::clone(&state)), req.body(Body::empty()).unwrap()).await.status(), 304);

        // URLSearchParams' spelling, and an unsorted, lowercased one: the same body, named canonically.
        for other in [
            "/index/filter/movie/counts.json?sel=country%3AKR%2Csubgenre%3AHeist",
            "/index/filter/movie/counts.json?sel=subgenre:Heist,country:kr",
            "/index/filter/movie/counts?sel=country:KR,subgenre:Heist,country:KR",
        ] {
            let resp = get(&state, other).await;
            assert_eq!(resp.status(), 200, "{other}");
            assert_eq!(resp.headers()[header::CACHE_CONTROL], "private, max-age=60", "{other}");
            let location = resp.headers()[header::CONTENT_LOCATION].to_str().unwrap().to_owned();
            assert_eq!(location, "counts.json?sel=country:KR,subgenre:Heist", "{other}");
            assert_eq!(body_of(resp).await, korean_body, "{other}");
        }
        let emptied = get(&state, "/index/filter/movie/counts.json?sel=").await;
        assert_eq!(emptied.headers()[header::CONTENT_LOCATION], "counts.json");
        // The path is part of the key: without `.json` even a canonical query is a second spelling.
        let bare = get(&state, "/index/filter/movie/counts?sel=country:KR,subgenre:Heist").await;
        assert_eq!(bare.headers()[header::CACHE_CONTROL], "private, max-age=60");
        assert_eq!(bare.headers()[header::CONTENT_LOCATION], "counts.json?sel=country:KR,subgenre:Heist");

        // A kind this atlas does not know is answered around, and said so; so is a value its kind lacks.
        let unknown = json(body_of(get(&state, "/index/filter/movie/counts.json?sel=nope:1").await).await);
        assert_eq!((&unknown["total"], &unknown["ignored"]), (&3.into(), &serde_json::json!(["nope"])));
        let typo = json(
            body_of(get(&state, "/index/filter/movie/counts.json?sel=mood:tense,tone:blaek").await).await,
        );
        assert_eq!(typo["total"], 0);
        assert_eq!(typo["unknownValues"], serde_json::json!(["mood:tense", "tone:blaek"]));

        for path in [
            "/index/filter/movie/counts.json?sel=genre:action",
            "/index/filter/movie/counts.json?sel=person:bob",
            "/index/filter/movie/counts.json?sel=genre",
            "/index/filter/movie/counts.json?sel=:1",
            "/index/filter/movie/counts.json?sel=-:x",
        ] {
            let resp = get(&state, path).await;
            assert_eq!(resp.status(), 400, "{path}");
            assert_eq!(json(body_of(resp).await)["error"], "bad_request", "{path}");
        }
        for path in [
            "/index/filter/anime/counts.json",
            "/index/filter/movie/nope.json",
            "/index/filter/movie/values/like.json",
            "/index/facets/movie.json",
            "/index/browse/movie.json",
        ] {
            assert_eq!(get(&state, path).await.status(), 404, "{path}");
        }
    }

    /// The titles carrying a selection: the very cards `/index/row` draws, paged, with the page in the
    /// canonical URL — which is the cache key, so it carries an ETag and revalidates.
    #[tokio::test]
    async fn filter_titles_page_a_selection_by_canonical_url() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        let state = index_state("den-atlas-filter-titles");
        let json = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();
        let ids = |answer: &serde_json::Value| -> Vec<u64> {
            answer["titles"].as_array().unwrap().iter().map(|t| t["id"].as_u64().unwrap()).collect()
        };

        let korean = get(&state, "/index/filter/movie/titles.json?sel=country:KR,subgenre:Heist").await;
        assert_eq!(korean.status(), 200);
        assert_eq!(
            korean.headers()[header::CACHE_CONTROL],
            "public, max-age=3600, stale-while-revalidate=86400"
        );
        let etag = korean.headers()[header::ETAG].clone();
        let korean = json(body_of(korean).await);
        assert_eq!((ids(&korean), &korean["total"]), (vec![2, 1], &serde_json::json!(2)));
        assert!(korean["order"].as_str().is_some_and(|o| o.len() == 16), "{korean}");
        assert_eq!(korean["coverage"]["country"], serde_json::json!({ "count": 3, "denominator": 3 }));

        // The same card, field for field, that a browse row draws for the same title.
        let row = json(body_of(get(&state, "/index/row/movie.json?subgenre=Heist").await).await);
        let card = |answer: &serde_json::Value, id: u64| {
            answer["titles"].as_array().unwrap().iter().find(|t| t["id"] == id).cloned().unwrap()
        };
        assert_eq!(card(&korean, 1), card(&row, 1));
        assert_eq!(card(&korean, 2), card(&row, 2));

        let page = json(
            body_of(get(&state, "/index/filter/movie/titles.json?sel=subgenre:Heist&skip=1&limit=1").await)
                .await,
        );
        assert_eq!((ids(&page), &page["total"]), (vec![1], &serde_json::json!(3)));
        let top = json(body_of(get(&state, "/index/filter/movie/titles.json").await).await);
        assert_eq!(ids(&top), vec![2, 1, 3], "no selection: most voted first");

        // More like movie 1, in its own order, and within it only the comedy.
        let like = json(body_of(get(&state, "/index/filter/movie/titles.json?sel=like:1").await).await);
        let similar = json(body_of(get(&state, "/index/similar/movie/1.json?limit=200").await).await);
        assert_eq!(serde_json::json!(ids(&like)), similar["ids"], "the similar set, in similarity order");
        assert_eq!(like["order"], "like:1");
        let comedy =
            json(body_of(get(&state, "/index/filter/movie/titles.json?sel=genre:35,like:1").await).await);
        assert_eq!(ids(&comedy), vec![3]);

        // Excluded violence: movie 1 depicts it, movie 2 is on record as not, movie 3 was described and
        // scores nothing — known clean.
        let calm =
            json(body_of(get(&state, "/index/filter/movie/titles.json?sel=-warning:violence").await).await);
        assert_eq!(ids(&calm), vec![2, 3]);

        let req = HttpRequest::builder()
            .uri("/index/filter/movie/titles.json?sel=country:KR,subgenre:Heist")
            .header("if-none-match", etag);
        let revalidated = handle(State(Arc::clone(&state)), req.body(Body::empty()).unwrap()).await;
        assert_eq!(revalidated.status(), 304);

        let other =
            get(&state, "/index/filter/movie/titles.json?limit=500&skip=0&sel=subgenre:Heist,country:kr")
                .await;
        assert_eq!(other.status(), 200);
        assert_eq!(other.headers()[header::CACHE_CONTROL], "private, max-age=60");
        assert_eq!(
            other.headers()[header::CONTENT_LOCATION],
            "titles.json?sel=country:KR,subgenre:Heist&limit=100"
        );
        for path in [
            "/index/filter/movie/titles.json?skip=x",
            "/index/filter/movie/titles.json?skip=1&limit=2",
            "/index/filter/movie/titles.json?sel=genre",
        ] {
            assert_eq!(get(&state, path).await.status(), 400, "{path}");
        }
    }

    /// One kind's values under a selection: a prefix search over names and aliases for people, over label
    /// words for the rest — most titles first, labelled.
    #[tokio::test]
    async fn filter_values_search_a_kind_by_prefix() {
        let state = index_state("den-atlas-filter-values");
        let json = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();
        let bong = json(body_of(get(&state, "/index/filter/movie/values/person.json?q=Bong").await).await);
        assert_eq!(bong["kind"], "person");
        assert_eq!(bong["values"], serde_json::json!([{ "id": "Q2", "name": "Lead Actor", "count": 1 }]));
        let director = json(body_of(get(&state, "/index/filter/movie/values/made.json?q=dir").await).await);
        assert_eq!(director["values"][0]["tmdbId"], 11, "a person's TMDB id, where Wikidata has it");
        let cult = json(body_of(get(&state, "/index/filter/movie/values/subgenre.json?q=cult").await).await);
        assert_eq!(
            cult["values"],
            serde_json::json!([{ "id": "Campy/Cult", "name": "Campy/Cult", "count": 1 }])
        );
        let korean =
            json(body_of(get(&state, "/index/filter/movie/values/decade.json?sel=country:KR").await).await);
        assert_eq!(korean["values"].as_array().unwrap().len(), 2, "{korean}");
        assert_eq!(get(&state, "/index/filter/movie/values/person.json?q=b").await.status(), 400);
        // A page of people without a prefix: the top of the counts, the rest counted but not named.
        let two = json(body_of(get(&state, "/index/filter/movie/values/person.json?limit=2").await).await);
        assert_eq!((two["values"].as_array().unwrap().len(), &two["complete"]), (2, &false.into()));
        let all = json(body_of(get(&state, "/index/filter/movie/values/person.json").await).await);
        assert_eq!(all["values"].as_array().unwrap()[..2], two["values"].as_array().unwrap()[..], "a prefix");
    }

    /// A household's taste REORDERS a row and does nothing else: the same total, the same titles, a
    /// different order — and a different set of weights, a different order again.
    #[tokio::test]
    async fn a_tilted_row_is_the_same_row_in_another_order() {
        let state = index_state("den-atlas-tilted-rows");
        let json = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();
        let ids = |answer: &serde_json::Value| -> Vec<u64> {
            answer["titles"].as_array().unwrap().iter().map(|t| t["id"].as_u64().unwrap()).collect()
        };
        let row = "/index/row/movie.json?subgenre=Heist";
        let plain = json(body_of(get(&state, row).await).await);
        assert_eq!(ids(&plain), vec![2, 1, 3], "{plain}");

        // Movie 2 rejected. It leads the untilted row and the squared dislike drops it — but only by the
        // slots the weight is worth, not to the back: burying a title is a hide rule's job. Movie 1 sits
        // almost on top of movie 2 in the vector space (cos 0.99), so it is pushed down nearly as far and
        // ends up behind movie 3, which is barely related (cos 0.11) and barely touched.
        let disliked = json(body_of(get(&state, &format!("{row}&tilt.disliked=m2")).await).await);
        assert_eq!(ids(&disliked), vec![2, 3, 1], "{disliked}");
        assert_eq!(disliked["total"], plain["total"], "a tilt never changes a row's total");
        let (mut before, mut after) = (ids(&plain), ids(&disliked));
        before.sort_unstable();
        after.sort_unstable();
        assert_eq!(after, before, "and nothing entered or left the row");

        // The weights are request parameters, so a tuner can move them: a loud enough embedding weight puts
        // the liked title in front, where the shipped 0.15 leaves it where the row's own order had it.
        let liked = format!("{row}&tilt.liked=m3");
        let gentle = json(body_of(get(&state, &liked).await).await);
        assert_eq!(ids(&gentle), vec![2, 1, 3], "the shipped weight is a nudge: {gentle}");
        let loud = json(body_of(get(&state, &format!("{liked}&tilt.w.embedding=2")).await).await);
        assert_eq!(ids(&loud)[0], 3, "a big enough weight does take the lead: {loud}");
        assert_ne!(gentle["taste"], serde_json::Value::Null, "the page says which order it is a slice of");
        assert_eq!(gentle["taste"], loud["taste"], "same taste, different levers");

        // Every page of one row is a slice of ONE order, so paging state stays valid under infinite scroll.
        let page = json(body_of(get(&state, &format!("{row}&tilt.disliked=m2&skip=1&limit=1")).await).await);
        assert_eq!(page["total"], plain["total"]);
        assert_eq!(ids(&page), vec![ids(&disliked)[1]], "{page}");

        // A taste that names nothing is no taste at all: byte-identical to the row with no taste in its URL.
        let empty = body_of(get(&state, &format!("{row}&tilt.liked=&tilt.disliked=")).await).await;
        assert_eq!(empty, body_of(get(&state, row).await).await);

        // The taste travels in the URL, so a tilted page is cacheable to the client that asked and to
        // nobody else; a row with no taste in it keeps the shared TTL it has always had.
        let tilted = get(&state, &format!("{row}&tilt.disliked=m2")).await;
        assert_eq!(tilted.headers()["cache-control"], "private, max-age=3600, stale-while-revalidate=86400");
        assert_eq!(
            get(&state, row).await.headers()["cache-control"],
            "public, max-age=3600, stale-while-revalidate=86400"
        );
    }

    /// A ranked answer from a real fixture index and facts: never what the library owns, another type, or a
    /// hidden title, and never cached.
    #[tokio::test]
    async fn recommend_ranks_what_the_library_does_not_hold() {
        let state = index_state("den-atlas-recommend");
        let body = r#"{"surface":"movies","now":"2026-09-12T00:00:00Z",
            "library":[{"type":"movie","id":1,"weight":1}],
            "owned":[{"type":"movie","id":1}],
            "hide":{"languages":["es"]},
            "candidates":[
              {"type":"movie","id":2,"hint":{"releaseDate":"2026-09-01","popularity":40,"genreIds":[18]}},
              {"type":"movie","id":1,"rank":0,"of":10},
              {"type":"series","id":4,"rank":1,"of":10},
              {"type":"movie","id":99,"hint":{"releaseDate":"2026-09-10","originalLanguage":"es"}}
            ]}"#;
        let resp = post(&state, "/recommend", body).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["cache-control"], "no-store");
        let answer: serde_json::Value = serde_json::from_str(&body_of(resp).await).unwrap();
        let slides: Vec<(String, u64)> = answer["slides"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["type"].as_str().unwrap().to_owned(), s["id"].as_u64().unwrap()))
            .collect();
        assert_eq!(slides, vec![("movie".to_owned(), 2)], "{answer}");
        let why = &answer["slides"][0]["why"];
        assert!(why["reason"].is_string(), "{answer}");
        for term in ["score", "fit", "profile", "people", "confidence", "fresh", "arrived", "quality", "buzz"]
        {
            assert!(why[term].is_number(), "why.{term} is retained: {answer}");
        }
        assert!(why.get("similar").is_some(), "nullable why.similar is retained: {answer}");
        assert_eq!(answer["facts"], true);
        assert_eq!(answer["datasetVersion"], "v1");
        assert_eq!(answer["libraryUnjudged"], 0);
        // Movie 99 is known to nothing but its hint, which names no genre — and it is hidden, so never asked about.
        assert_eq!(answer["unjudged"], serde_json::json!([]));
        assert_eq!(answer["unjudgedCount"], 1);

        // The log line names the slides from the cards and counts the library without naming it.
        let request: crate::recommend::Request = serde_json::from_str(body).unwrap();
        let indexes = state.index.as_ref().unwrap().get(|| ()).await.unwrap().0;
        let line = crate::recommend::summary(&indexes, &request, &answer);
        assert!(
            line.starts_with(
                "recommend movies: library 1 (0 unjudged, 1 indexed), owned 1, candidates 4, pool 4 ("
            ),
            "{line}"
        );
        assert!(line.contains(" 1 unjudged, 0 personal), 1 slides; 1. Two "), "{line}");
        assert!(line.contains("fit ") && line.contains("fresh "), "{line}");
        assert!(!line.contains("One"), "a library title is never named: {line}");

        // JustWatch's IMDb score rests on TMDB's vote count where the facets hold one. A transient client score on
        // too few votes does not replace it, while a well-counted one may rank this response without being emitted.
        let known = crate::recommend::Knowledge { indexes: &indexes };
        let listed = crate::recommend::Listed {
            key: (den_index::MediaType::Movie, 2),
            imdb_id: None,
            rating: Some(8.0),
            year: None,
        };
        let title = known.title(listed.key, None, Some(&listed));
        assert_eq!((title.rating, title.votes, title.estimated_votes), (Some(8.0), Some(500.0), false));
        let few: crate::recommend::Hint = serde_json::from_str(r#"{"rating":7.2,"votes":18}"#).unwrap();
        let title = known.title((den_index::MediaType::Movie, 99), Some(&few), Some(&listed));
        assert_eq!((title.rating, title.votes, title.estimated_votes), (Some(8.0), Some(200.0), true));
        let enough: crate::recommend::Hint = serde_json::from_str(r#"{"rating":7.2,"votes":180}"#).unwrap();
        let hinted = known.title((den_index::MediaType::Movie, 99), Some(&enough), Some(&listed));
        assert_eq!((hinted.rating, hinted.votes), (Some(7.2), Some(180.0)));
        assert!(answer["slides"]
            .as_array()
            .unwrap()
            .iter()
            .all(|slide| { slide.get("rating").is_none() && slide.get("votes").is_none() }));
    }

    /// Off without `INDEX_QUERIES`, and a malformed or oversized request is a 400.
    #[tokio::test]
    async fn recommend_is_off_unless_configured_and_refuses_a_bad_body() {
        assert_eq!(post(&Arc::new(AppState::for_test(None)), "/recommend", "{}").await.status(), 404);
        let state = index_state("den-atlas-recommend-bad");
        assert_eq!(post(&state, "/recommend", "not json").await.status(), 400);
        let many = format!(
            r#"{{"candidates":[{}]}}"#,
            vec![r#"{"type":"movie","id":1}"#; crate::recommend::MAX_CANDIDATES + 1].join(",")
        );
        assert_eq!(post(&state, "/recommend", &many).await.status(), 400);
    }

    /// A service channel ranks what its service's lists named and what the client offered, still only of the
    /// surface's types, and a service atlas doesn't carry ranks the client's candidates alone.
    #[tokio::test]
    async fn recommend_on_a_service_shows_only_what_is_on_it() {
        use crate::recommend::{answer, parse_now, Listed, Lists, Request};
        use den_index::MediaType;
        let state = index_state("den-atlas-recommend-service");
        let indexes = state.index.as_ref().unwrap().get(|| ()).await.unwrap().0;
        let slides = |answer: &serde_json::Value| -> Vec<(String, u64)> {
            let slides = answer["slides"].as_array().unwrap().iter();
            slides.map(|s| (s["type"].as_str().unwrap().to_owned(), s["id"].as_u64().unwrap())).collect()
        };
        let now = parse_now("1995-07-01T00:00:00Z").unwrap();
        let lists = Lists {
            popular: vec![vec![Listed {
                key: (MediaType::Movie, 3),
                imdb_id: None,
                rating: None,
                year: None,
            }]],
            ..Lists::default()
        };
        let body = |extra: &str| -> Request {
            serde_json::from_str(&format!(
                r#"{{"library":[{{"type":"movie","id":1,"weight":1}}],"owned":[{{"type":"movie","id":1}}],
                    "candidates":[{{"type":"series","id":4}}]{extra}}}"#
            ))
            .unwrap()
        };
        let channel = answer(&indexes, None, &body(r#","service":{"id":8,"country":"FI"}"#), &lists, now);
        assert_eq!(slides(&channel), vec![("series".to_owned(), 4), ("movie".to_owned(), 3)], "{channel}");
        assert_eq!(channel["pool"]["personal"], 0);

        let movies = answer(&indexes, None, &body(r#","surface":"movies","service":{"id":8}"#), &lists, now);
        assert_eq!(slides(&movies), vec![("movie".to_owned(), 3)], "{movies}");

        // A service atlas doesn't carry has no lists (`lists` reads none): the client's candidates alone.
        let unknown = answer(&indexes, None, &body(r#","service":{"id":283}"#), &Lists::default(), now);
        assert_eq!(slides(&unknown), vec![("series".to_owned(), 4)], "{unknown}");
    }

    /// A channel asks JustWatch for its own service in its own country only — Trending Everywhere included, so
    /// another service's trending title never reaches it — and a service this install doesn't carry asks nothing.
    #[tokio::test]
    async fn recommend_lists_for_a_channel_read_its_service_alone() {
        use crate::justwatch::{ObjectType, TrendingItem};
        /// Carries Netflix and Disney+, every row of a service naming one title of its own (TMDB id = its package id),
        /// and notes each row asked for as (service, country, sort).
        #[derive(Default)]
        struct Services {
            asked: std::sync::Mutex<Vec<(String, String, String)>>,
        }
        impl Services {
            fn row(&self, provider: &str, country: &str, sort: &str) -> Vec<TrendingItem> {
                crate::util::lock(&self.asked).push((
                    provider.to_owned(),
                    country.to_owned(),
                    sort.to_owned(),
                ));
                let id = if provider == "nfx" { 8 } else { 337 };
                vec![TrendingItem {
                    imdb: format!("tt{id}"),
                    moviedb: Some(id),
                    title: provider.to_owned(),
                    rank: 0,
                    rating: None,
                    year: None,
                    at: None,
                }]
            }
        }
        #[async_trait::async_trait]
        impl crate::justwatch::TrendingSource for Services {
            async fn popular(
                &self,
                provider: &str,
                _: ObjectType,
                country: &str,
                sort: &str,
            ) -> Result<Vec<TrendingItem>, ()> {
                Ok(self.row(provider, country, sort))
            }
            async fn new_titles(
                &self,
                provider: &str,
                _: ObjectType,
                country: &str,
            ) -> Result<Vec<TrendingItem>, ()> {
                Ok(self.row(provider, country, "NEW"))
            }
            async fn packages(&self, _: &str) -> Result<Vec<(i64, String)>, ()> {
                Ok(vec![(8, "nfx".to_owned()), (337, "dnp".to_owned())])
            }
        }
        let read = |body: &str| serde_json::from_str::<crate::recommend::Request>(body).unwrap();
        let config = Config::default_config();
        let keys = |lists: &crate::recommend::Lists| -> Vec<u32> {
            let all = lists.arrivals.iter().chain(&lists.popular).chain(&lists.charts).flatten();
            let mut ids: Vec<u32> = all.chain(&lists.everywhere).map(|l| l.key.1).collect();
            ids.sort_unstable();
            ids.dedup();
            ids
        };

        let source = Arc::new(Services::default());
        let state = Arc::new(AppState::for_test_with_source(source.clone()));
        let home = crate::recommend::lists(&state, &config, &read(r#"{"surface":"movies"}"#)).await;
        assert_eq!(keys(&home), vec![8, 337]);
        assert!(crate::util::lock(&source.asked).contains(&("dnp".into(), "US".into(), "TRENDING".into())));

        let source = Arc::new(Services::default());
        let state = Arc::new(AppState::for_test_with_source(source.clone()));
        let channel = r#"{"surface":"movies","service":{"id":8,"country":"FI"}}"#;
        let channel = crate::recommend::lists(&state, &config, &read(channel)).await;
        assert_eq!(keys(&channel), vec![8]);
        assert_eq!(channel.everywhere.len(), 1, "Trending Everywhere is Netflix's own trending chart");
        let mut asked = crate::util::lock(&source.asked).clone();
        asked.sort();
        let fi = |sort: &str| ("nfx".to_owned(), "FI".to_owned(), sort.to_owned());
        assert_eq!(asked, vec![fi("NEW"), fi("POPULAR"), fi("TRENDING")]);

        let source = Arc::new(Services::default());
        let state = Arc::new(AppState::for_test_with_source(source.clone()));
        let unknown = crate::recommend::lists(&state, &config, &read(r#"{"service":{"id":283}}"#)).await;
        assert!(keys(&unknown).is_empty() && unknown.arrivals.is_empty() && unknown.popular.is_empty());
        assert!(crate::util::lock(&source.asked).is_empty(), "an unknown service asks JustWatch nothing");
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

    /// `/health` answers 200 whatever is wrong, by the addon convention. `/ready` is the one a monitor
    /// watches, and it must separate "a capability is gone" from "the rows are a day old" — the distinction
    /// that let facts_unusable sit unnoticed behind a healthy-looking 200.
    #[test]
    fn only_losing_a_capability_is_worth_waking_someone() {
        assert!(loses_a_feature("dataset_unavailable"));
        assert!(loses_a_feature("facts_unusable"));
        assert!(loses_a_feature("rows_unusable"));
        assert!(loses_a_feature("votes_unusable"));
        assert!(!loses_a_feature("stale_catalog"), "older rows are not an outage");
        assert!(!loses_a_feature("catalog_schema_suspect"), "short rows are not an outage");
        assert!(!loses_a_feature("ok"));

        // Every reason health_state can produce is classified: a new one must be considered, not defaulted.
        for (loaded, fresh, suspect, facts, store, rows, votes) in [
            (false, true, false, false, false, false, false),
            (true, false, false, false, false, false, false),
            (true, true, true, false, false, false, false),
            (true, true, false, true, false, false, false),
            (true, true, false, false, true, false, false),
            (true, true, false, false, false, true, false),
            (true, true, false, false, false, false, true),
        ] {
            let (reason, _) =
                health_state(loaded, fresh, suspect, facts, store, rows, votes).expect("degraded");
            assert!(
                loses_a_feature(reason) || matches!(reason, "stale_catalog" | "catalog_schema_suspect"),
                "unclassified reason {reason}"
            );
        }
        assert!(health_state(true, true, false, false, false, false, false).is_none());
    }

    /// A corpus no source can order must be REPORTED, and must count as losing a feature.
    ///
    /// This is the one degradation that looks like a working addon: every browse row comes back full,
    /// in tmdb-id order. It shipped once — *La Job* (tv:5) beside *Game of Thrones* — and nothing said a
    /// word, because `votes_of` answered 0 for every title and 0 is a valid vote count.
    #[test]
    fn a_corpus_with_no_vote_counts_is_degraded_and_serious() {
        let (reason, detail) =
            health_state(true, true, false, false, false, false, true).expect("no vote source is degraded");
        assert_eq!(reason, "votes_unusable");
        assert!(detail.contains("tmdb-id order"), "the detail should say what goes wrong: {detail}");
        assert!(loses_a_feature(reason), "den-update must treat it as serious enough to roll back");

        // It ranks below every reason that means rows are missing or empty: a row in the wrong order is
        // still a row.
        assert_eq!(health_state(true, true, false, false, false, true, true).unwrap().0, "rows_unusable");
        assert_eq!(health_state(true, true, false, true, false, false, true).unwrap().0, "facts_unusable");
    }

    /// An unreadable store must be REPORTED, and must count as losing a feature.
    ///
    /// Without this, a store that does not open leaves More Like This ranking on vectors and labels
    /// alone: every request still answers, and answers worse. `den-update` rolls back on its health
    /// probe, so a degradation the probe cannot see is one it cannot roll back from — which is how a
    /// bad deploy stays deployed.
    #[test]
    fn an_unreadable_store_is_degraded_and_serious() {
        let (reason, detail) = health_state(true, true, false, false, true, false, false)
            .expect("an unreadable store is degraded");
        assert_eq!(reason, "store_unusable");
        assert!(detail.contains("More Like This"), "the detail should say what is lost: {detail}");
        assert!(loses_a_feature(reason), "den-update must treat it as serious enough to roll back");

        // It ranks BELOW the reasons that mean rows are missing entirely, and above nothing else.
        assert_eq!(
            health_state(false, true, false, false, true, false, false).unwrap().0,
            "dataset_unavailable"
        );
        assert_eq!(health_state(true, false, false, false, true, false, false).unwrap().0, "stale_catalog");
    }

    #[test]
    fn percent_decodes_the_search_extra() {
        assert_eq!(percent_decode("the%20matrix"), "the matrix");
        assert_eq!(percent_decode("am%C3%A9lie"), "amélie");
        assert_eq!(percent_decode("100%zz%2"), "100%zz%2");
    }

    /// The blob routes are GONE, and a manifest that still declares the sidecars must not bring them
    /// back — not as a route, and not as a URL in the descriptor. A client that still asks gets the
    /// same 404 as any unknown path, which is the honest answer once the files are not published.
    #[tokio::test]
    async fn the_blob_routes_are_gone_and_the_descriptor_names_no_files() {
        let dir = std::env::temp_dir().join(format!("den-atlas-routes-{}", std::process::id()));
        let state = Arc::new(AppState::for_test(Some(fixture(&dir))));

        // The fixture's manifest declares all six. Every one of their paths is an unknown route.
        for gone in [
            "/labels.json",
            "/labels.json?v=v9",
            "/vectors.bin",
            "/meta.json",
            "/facets.bin",
            "/premise-labels.json",
            "/premise-vectors.bin",
        ] {
            let resp = get(&state, gone).await;
            assert_eq!(resp.status(), 404, "{gone} is still served");
            assert_eq!(body_of(resp).await, r#"{"error":"not_found"}"#, "{gone}");
        }

        // ...and the descriptor points at nothing, so no client learns those paths in the first place.
        let desc = body_of(get(&state, "/dataset.json").await).await;
        assert!(!desc.contains("\"url\""), "the descriptor still advertises a blob URL: {desc}");
        let d: serde_json::Value = serde_json::from_str(&desc).expect("descriptor must be JSON");
        for gone in ["labels", "vectors", "metadata", "premise", "facets"] {
            assert!(d.get(gone).is_none(), "the descriptor still carries {gone}: {desc}");
        }
        // What it does say: the store's own row count and the space its vectors live in.
        assert!(desc.contains("\"count\":1"), "{desc}");
        assert!(desc.contains(r#""embeddingModel":"m""#) && desc.contains(r#""dims":2"#), "{desc}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing dataset must be a 503, not a 200 with nothing in it — the manifest still advertises
    /// the resource, and the app needs to tell "no dataset here" from "an empty dataset".
    #[tokio::test]
    async fn a_missing_dataset_is_unavailable_not_empty() {
        let state = Arc::new(AppState::for_test(None));
        assert_eq!(get(&state, "/dataset.json").await.status(), 503);
    }

    /// The manifest and the descriptor ride out an outage from a cache, and the descriptor revalidates by its
    /// ETag alone: its body moves with the origin and the embed/index flags under an unchanged dataset date, so
    /// an If-Modified-Since against that date would 304 a changed body.
    #[tokio::test]
    async fn the_manifest_and_descriptor_cache_and_revalidate_correctly() {
        let dir = std::env::temp_dir().join(format!("den-atlas-desc-cc-{}", std::process::id()));
        let mut ds = fixture(&dir);
        ds.last_modified = Some("Wed, 01 Jul 2026 00:00:00 GMT".to_owned());
        let state = Arc::new(AppState::for_test(Some(ds)));

        let manifest = get(&state, "/manifest.json").await;
        assert_eq!(
            manifest.headers()["cache-control"],
            "public, max-age=3600, stale-while-revalidate=600, stale-if-error=86400"
        );

        let desc = get(&state, "/dataset.json").await;
        assert_eq!(
            desc.headers()["cache-control"],
            "public, max-age=300, stale-while-revalidate=3600, stale-if-error=86400"
        );
        assert!(desc.headers().get("last-modified").is_none(), "the descriptor carried the dataset's date");
        let etag = desc.headers()["etag"].to_str().unwrap().to_owned();
        let tag = etag.trim_matches('"');
        let (hash, len) = tag.split_once('-').unwrap_or_else(|| panic!("{etag}"));
        assert_eq!(hash.len(), 16, "{etag}");
        assert_eq!(usize::from_str_radix(len, 16).unwrap(), body_of(desc).await.len(), "{etag}");

        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        let since = HttpRequest::builder()
            .uri("/dataset.json")
            .header("if-modified-since", "Fri, 01 Jan 2100 00:00:00 GMT")
            .body(Body::empty())
            .unwrap();
        assert_eq!(handle(State(Arc::clone(&state)), since).await.status(), 200);
        let matching = HttpRequest::builder()
            .uri("/dataset.json")
            .header("if-none-match", etag.as_str())
            .body(Body::empty())
            .unwrap();
        assert_eq!(handle(State(Arc::clone(&state)), matching).await.status(), 304);
        let _ = std::fs::remove_dir_all(&dir);
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
                at: None,
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
                at: None,
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
            ("/dataset.json", get(&state, "/dataset.json").await, 200),
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
