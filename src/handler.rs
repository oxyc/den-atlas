//! Routing — the port of `handleAtlas`. A single fallback handler matches on the path (exact, like the TS),
//! so unknown paths 404 and non-GET/HEAD 405.

use crate::config::Config;
use crate::dataset::{Blob, Dataset};
use crate::descriptor::build_descriptor;
use crate::http::{serve, Payload, Servable};
use crate::manifest::manifest_json;
use crate::util::{fnv1a, json_response, public_origin};
use crate::AppState;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use std::sync::Arc;

/// The /configure page, embedded so the binary is self-contained. Region + provider choice is plaintext
/// (no secrets to seal); the page's JS builds the `<region>_<codes>` install URL client-side.
const CONFIGURE_PAGE: &str = include_str!("configure.html");

pub async fn handle(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let method = req.method().clone();
    // CORS preflight for browser-based Stremio clients (public, credential-free data).
    if method == Method::OPTIONS {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
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

    if route == "/" || route == "/configure" || route == "/configure/" {
        return serve_html(&method, &headers, CONFIGURE_PAGE).await;
    }
    if route == "/health" {
        // Standard Den addon health shape (ADDON-02): 200 for liveness, but report `degraded` so the
        // app's Plugins screen (and any monitor) can see a problem.
        return json_response(health_body(ds.is_some(), state.catalog.fresh()), StatusCode::OK);
    }
    if route == "/manifest.json" {
        return serve_json(&method, &headers, manifest_json(&config), "public, max-age=3600, stale-while-revalidate=600", None).await;
    }
    if route == "/dataset.json" {
        return match ds {
            Some(ds) => {
                serve_json(&method, &headers, build_descriptor(&origin, ds, state.embed.is_some()), "public, max-age=300", ds.last_modified.clone()).await
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
    json_response(r#"{"error":"not_found"}"#, StatusCode::NOT_FOUND)
}

/// `POST /embed` — the search query-embed proxy. Forwards the JSON body (`{"text":"…"}`) to den-embed and
/// returns its response verbatim (`{"vector":int8[dims],"dims":Int,"model":String}`). den-atlas never runs
/// the model, so a query embeds through the SAME bge-m3 + int8 quantizer as the corpus (the alignment rule),
/// and den-embed stays internal. Absent `DEN_EMBED_URL` ⇒ 503 (dataset serving is unaffected).
async fn handle_embed(state: &Arc<AppState>, req: Request) -> Response {
    let Some(proxy) = state.embed.as_ref() else {
        return json_response(
            r#"{"error":"embed_unavailable","detail":"search embeds are not configured (DEN_EMBED_URL unset)"}"#,
            StatusCode::SERVICE_UNAVAILABLE,
        );
    };
    // A search query is short — cap the body so this can't relay large payloads to the internal service.
    let body = match axum::body::to_bytes(req.into_body(), 64 * 1024).await {
        Ok(b) => b,
        Err(_) => return json_response(r#"{"error":"bad_request"}"#, StatusCode::BAD_REQUEST),
    };
    match proxy
        .client
        .post(format!("{}/embed", proxy.base))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let bytes = resp.bytes().await.unwrap_or_default();
            Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                // A POST proxy of per-query vectors — never let a heuristic/intermediary cache these.
                .header(header::CACHE_CONTROL, "no-store")
                .body(Body::from(bytes))
                .unwrap()
        }
        Err(e) => {
            eprintln!("den-atlas: embed upstream error: {e}");
            json_response(r#"{"error":"embed_upstream_failed"}"#, StatusCode::BAD_GATEWAY)
        }
    }
}

/// The `/health` JSON body (ADDON-02). Always paired with 200 + `no-store` — liveness never fails; the
/// body carries the real state. Dataset-unavailable outranks a stale catalog (no dataset is the more
/// severe condition): no dataset ⇒ `dataset_unavailable`; else a failed last JustWatch refresh ⇒
/// `stale_catalog`; else `ok`. Pure + `&'static str` so the decision is unit-testable without an HTTP round-trip.
fn health_body(dataset_loaded: bool, catalog_fresh: bool) -> &'static str {
    if !dataset_loaded {
        r#"{"status":"degraded","reason":"dataset_unavailable","detail":"dataset failed to load; refresh with scripts/fetch-dataset.sh"}"#
    } else if !catalog_fresh {
        r#"{"status":"degraded","reason":"stale_catalog","detail":"last JustWatch refresh failed; serving stale catalog"}"#
    } else {
        r#"{"status":"ok"}"#
    }
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
    let rest = rest.strip_suffix(".json").unwrap_or(rest);
    let mut parts = rest.splitn(3, '/'); // type / id / optional extra
    let type_ = parts.next().unwrap_or("");
    let id = parts.next().unwrap_or("");
    let extra = parts.next().unwrap_or("");
    // A fixed-country config wins; an `auto` config takes the forwarded `country` extra; else default.
    let forwarded = extra_value(extra, "country");
    let country = config.country(forwarded.as_deref(), &state.default_country);
    match state.catalog.metas_json(id, type_, &country, &config.providers).await {
        Some(r) => {
            // Fresh/stale-good rows cache for an hour; an outage-empty/stale fallback caches briefly so a
            // CDN doesn't pin a broken row past JustWatch's recovery.
            let cc = if r.fresh { "public, max-age=3600, stale-while-revalidate=600" } else { "public, max-age=60" };
            serve_json(method, headers, r.body, cc, None).await
        }
        None => json_response(r#"{"error":"not_found"}"#, StatusCode::NOT_FOUND),
    }
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
            cache_control: "public, max-age=3600".to_owned(),
            last_modified: None,
            size,
            identity: Payload::Memory(bytes),
            gzip: None,
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
    // `?v=<current datasetVersion>` ⇒ immutable for a year; a bare request revalidates.
    let pinned = query_param(query, "v").as_deref() == Some(ds.meta.dataset_version.as_str());
    let cache_control = if pinned {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    }
    .to_owned();
    let gzip = blob
        .gz
        .as_ref()
        .map(|g| (Payload::File(g.path.clone()), g.size));
    serve(
        method,
        headers,
        Servable {
            etag_base: blob.sha256.clone(),
            content_type: blob.content_type.to_owned(),
            cache_control,
            last_modified: ds.last_modified.clone(),
            size: blob.size,
            identity: Payload::File(blob.path.clone()),
            gzip,
        },
    )
    .await
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
        (it.next()? == key)
            .then(|| it.next().unwrap_or("").to_owned())
            .filter(|v| !v.is_empty())
    })
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_ok_when_dataset_loaded_and_fresh() {
        assert_eq!(health_body(true, true), r#"{"status":"ok"}"#);
    }

    #[test]
    fn health_stale_catalog_when_last_refresh_failed() {
        let body = health_body(true, false);
        assert!(body.contains(r#""status":"degraded""#));
        assert!(body.contains(r#""reason":"stale_catalog""#));
    }

    #[test]
    fn health_dataset_unavailable_when_dataset_missing() {
        // Dataset-unavailable outranks stale: even with a fresh catalog, no dataset is the reported reason.
        let body = health_body(false, true);
        assert!(body.contains(r#""reason":"dataset_unavailable""#));
        // …and it still takes precedence when the catalog is also stale (the more severe condition wins).
        assert!(health_body(false, false).contains(r#""reason":"dataset_unavailable""#));
    }
}
