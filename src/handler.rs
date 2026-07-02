//! Routing — the port of `handleAtlas`. A single fallback handler matches on the path (exact, like the TS),
//! so unknown paths 404 and non-GET/HEAD 405.

use crate::dataset::{Blob, Dataset};
use crate::descriptor::build_descriptor;
use crate::http::{serve, Payload, Servable};
use crate::manifest::manifest_json;
use crate::util::{
    escape_html, fnv1a, group_thousands, html_response, json_response, public_origin,
};
use crate::AppState;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use std::sync::Arc;

pub async fn handle(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let method = req.method().clone();
    // CORS preflight for browser-based Stremio clients (public, credential-free data).
    if method == Method::OPTIONS {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, HEAD, OPTIONS")
            .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "*")
            .body(Body::empty())
            .unwrap();
    }
    if method != Method::GET && method != Method::HEAD {
        return json_response(r#"{"error":"method_not_allowed"}"#, StatusCode::METHOD_NOT_ALLOWED);
    }
    let headers = req.headers().clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or("").to_owned();
    let origin = public_origin(&headers, state.public_base.as_deref());
    let ds = state.dataset.as_ref();

    if path == "/" || path == "/configure" || path == "/configure/" {
        return html_response(landing_page(&origin, ds));
    }
    if path == "/health" {
        return json_response(r#"{"status":"ok"}"#, StatusCode::OK);
    }
    if path == "/manifest.json" {
        return serve_json(&method, &headers, manifest_json(), "public, max-age=3600", None).await;
    }
    if path == "/dataset.json" {
        return match ds {
            Some(ds) => {
                serve_json(&method, &headers, build_descriptor(&origin, ds), "public, max-age=300", ds.last_modified.clone()).await
            }
            None => json_response(
                r#"{"error":"dataset_unavailable","detail":"the dataset failed to load (missing/old dataset.meta.json); refresh it with scripts/fetch-dataset.sh"}"#,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        };
    }
    // Blob routes exist only when the dataset loaded (their names come from the meta).
    if let Some(ds) = ds {
        if path == format!("/{}", ds.labels.name) {
            return serve_blob(&method, &headers, &query, ds, &ds.labels).await;
        }
        if path == format!("/{}", ds.vectors.name) {
            return serve_blob(&method, &headers, &query, ds, &ds.vectors).await;
        }
    }
    if let Some(rest) = path.strip_prefix("/catalog/") {
        return handle_catalog(&method, &headers, rest, &state).await;
    }
    json_response(r#"{"error":"not_found"}"#, StatusCode::NOT_FOUND)
}

/// `GET /catalog/{type}/{id}.json` (a trailing `/{extra}` segment is tolerated and ignored). Public,
/// tokenless; a JustWatch failure degrades to empty rows, never a 5xx, and never touches the dataset.
async fn handle_catalog(
    method: &Method,
    headers: &axum::http::HeaderMap,
    rest: &str,
    state: &Arc<AppState>,
) -> Response {
    let rest = rest.strip_suffix(".json").unwrap_or(rest);
    let mut parts = rest.splitn(3, '/'); // type / id / optional extra
    let type_ = parts.next().unwrap_or("");
    let id = parts.next().unwrap_or("");
    match state.catalog.metas_json(id, type_).await {
        Some(r) => {
            // Fresh/stale-good rows cache for an hour; an outage-empty/stale fallback caches briefly so a
            // CDN doesn't pin a broken row past JustWatch's recovery.
            let cc = if r.fresh { "public, max-age=3600" } else { "public, max-age=60" };
            serve_json(method, headers, r.body, cc, None).await
        }
        None => json_response(r#"{"error":"not_found"}"#, StatusCode::NOT_FOUND),
    }
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

fn landing_page(origin: &str, ds: Option<&Dataset>) -> String {
    let manifest_url = format!("{}/manifest.json", escape_html(origin));
    let status = match ds {
        Some(d) => format!(
            "<p>Currently serving <b>{}</b> titles (<code>{}</code> / <code>{}</code>).</p>",
            group_thousands(d.meta.count),
            escape_html(&d.meta.taxonomy_version),
            escape_html(&d.meta.embedding_model)
        ),
        None => "<p><b>Dataset unavailable</b> — the feature-store blobs failed to load (refresh with <code>scripts/fetch-dataset.sh</code>). Catalog rows still work.</p>".to_owned(),
    };
    [
        "<!doctype html><html><head><meta charset=utf-8>",
        "<meta name=viewport content='width=device-width,initial-scale=1'>",
        "<title>Den Atlas</title>",
        "<style>body{font:16px/1.5 system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem;color:#222}",
        "code{background:#f2f2f2;padding:.15rem .35rem;border-radius:4px;word-break:break-all}</style></head><body>",
        "<h1>Den Atlas</h1>",
        "<p>A self-hosted <b>dataset addon</b> for Den — derived labels + semantic vectors for the whole catalog.</p>",
        &status,
        "<p>Add this URL in Den → Settings → Plugins:</p>",
        &format!("<p><code>{manifest_url}</code></p>"),
        "<p>Also serves \u{201c}most popular\u{201d} streaming catalog rows. Catalog data from JustWatch.</p>",
        "</body></html>",
    ]
    .join("")
}
