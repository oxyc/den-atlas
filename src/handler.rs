//! Routing — the port of `handleAtlas`. A single fallback handler matches on the path (exact, like the TS),
//! so unknown paths 404 and non-GET/HEAD 405.

use crate::catalog::all_providers;
use crate::config::{Config, Region};
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

    // A leading `<region>_<codes>` path segment is a per-install config (Stremio config-URL pattern);
    // strip it and route on the remainder. Absent/garbled → the operator-default config. The dataset,
    // health, and blob routes are config-independent — the config only shapes manifest + catalog.
    let (config, route) = split_config(&path);
    let route = route.as_str();

    if route == "/" || route == "/configure" || route == "/configure/" {
        return html_response(configure_page(&origin, &config, ds));
    }
    if route == "/health" {
        // Standard Den addon health shape (ADDON-02): 200 for liveness, but report `degraded` so the
        // app's Plugins screen (and any monitor) can see a problem.
        return json_response(health_body(ds.is_some(), state.catalog.fresh()), StatusCode::OK);
    }
    if route == "/manifest.json" {
        return serve_json(&method, &headers, manifest_json(&config), "public, max-age=3600", None).await;
    }
    if route == "/dataset.json" {
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
        if route == format!("/{}", ds.labels.name) {
            return serve_blob(&method, &headers, &query, ds, &ds.labels).await;
        }
        if route == format!("/{}", ds.vectors.name) {
            return serve_blob(&method, &headers, &query, ds, &ds.vectors).await;
        }
    }
    if let Some(rest) = route.strip_prefix("/catalog/") {
        return handle_catalog(&method, &headers, rest, &config, &state).await;
    }
    json_response(r#"{"error":"not_found"}"#, StatusCode::NOT_FOUND)
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

/// Countries offered in the `/configure` dropdown (major JustWatch-covered markets). "Auto" is added
/// first in the HTML. Not exhaustive — a fixed code JustWatch doesn't cover just yields empty rows.
const COUNTRIES: &[(&str, &str)] = &[
    ("US", "United States"), ("GB", "United Kingdom"), ("CA", "Canada"), ("AU", "Australia"),
    ("IE", "Ireland"), ("NZ", "New Zealand"),
    ("SE", "Sweden"), ("NO", "Norway"), ("DK", "Denmark"), ("FI", "Finland"), ("IS", "Iceland"),
    ("DE", "Germany"), ("AT", "Austria"), ("CH", "Switzerland"), ("NL", "Netherlands"),
    ("BE", "Belgium"), ("FR", "France"), ("ES", "Spain"), ("PT", "Portugal"), ("IT", "Italy"),
    ("PL", "Poland"), ("CZ", "Czechia"), ("GR", "Greece"), ("TR", "Turkey"),
    ("BR", "Brazil"), ("MX", "Mexico"), ("AR", "Argentina"), ("CL", "Chile"),
    ("JP", "Japan"), ("KR", "South Korea"), ("IN", "India"), ("ID", "Indonesia"),
    ("SG", "Singapore"), ("ZA", "South Africa"),
];

/// The `/configure` page: pick a region (Auto or a country) + the streaming services you care about,
/// and it builds the install URL live. Reflects the current config when editing an existing install.
fn configure_page(origin: &str, config: &Config, ds: Option<&Dataset>) -> String {
    let current_region = match &config.region {
        Region::Auto => "auto".to_owned(),
        Region::Fixed(cc) => cc.clone(),
    };
    let auto_sel = if current_region == "auto" { " selected" } else { "" };
    let country_options: String = COUNTRIES
        .iter()
        .map(|&(code, name)| {
            let sel = if current_region == code { " selected" } else { "" };
            format!("<option value='{code}'{sel}>{}</option>", escape_html(name))
        })
        .collect();
    let checked: Vec<&str> = config.providers.iter().map(|p| p.code).collect();
    let provider_inputs: String = all_providers()
        .iter()
        .map(|p| {
            let ck = if checked.contains(&p.code) { " checked" } else { "" };
            format!(
                "<label class=prov><input type=checkbox value='{}'{ck}> {}</label>",
                p.code,
                escape_html(p.name)
            )
        })
        .collect();
    let status = match ds {
        Some(d) => format!("<p class=muted>Feature store: <b>{}</b> titles.</p>", group_thousands(d.meta.count)),
        None => "<p class=muted>Dataset unavailable — catalog rows still work.</p>".to_owned(),
    };
    let origin_js = serde_json::to_string(origin).unwrap_or_else(|_| "\"\"".to_owned());
    [
        "<!doctype html><html><head><meta charset=utf-8>",
        "<meta name=viewport content='width=device-width,initial-scale=1'>",
        "<title>Den Atlas — Configure</title>",
        "<style>",
        "body{font:16px/1.5 system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem;color:#222}",
        "h1{margin-bottom:.2rem}.muted{color:#666;font-size:.9rem}",
        "fieldset{border:1px solid #ddd;border-radius:8px;margin:1.2rem 0;padding:1rem}",
        "legend{font-weight:600;padding:0 .4rem}select{font:inherit;padding:.3rem}",
        ".prov{display:block;margin:.3rem 0}",
        "code,input#url{background:#f2f2f2;padding:.4rem .5rem;border-radius:4px;word-break:break-all;",
        "font:14px ui-monospace,monospace;width:100%;border:1px solid #ddd;box-sizing:border-box}",
        "a.btn,button{font:inherit;padding:.5rem .9rem;border-radius:6px;border:1px solid #ccc;",
        "background:#fafafa;cursor:pointer;text-decoration:none;color:#222;display:inline-block;margin:.4rem .4rem 0 0}",
        "a.install{background:#6f42c1;color:#fff;border-color:#6f42c1}#warn{color:#b00;display:none}",
        "</style></head><body>",
        "<h1>Den Atlas</h1>",
        "<p class=muted>Derived labels + semantic vectors for Den, plus \u{201c}most popular\u{201d} streaming rows (data from JustWatch).</p>",
        &status,
        "<fieldset><legend>Region</legend>",
        "<p class=muted>\u{201c}Auto\u{201d} uses your Apple TV\u{2019}s country automatically. Or pin one:</p>",
        &format!("<select id=region><option value=auto{auto_sel}>Auto (device region)</option>{country_options}</select>"),
        "</fieldset>",
        "<fieldset><legend>Streaming services</legend>",
        "<p class=muted>\u{201c}Popular on\u{2026}\u{201d} rows for the services you pick. Uncheck all to turn these rows off.</p>",
        &provider_inputs,
        "</fieldset>",
        "<p><b>Install URL</b> (add in Den \u{2192} Settings \u{2192} Plugins):</p>",
        "<input id=url readonly>",
        "<p id=warn>No services selected \u{2014} the \u{201c}most popular\u{201d} rows are off; only the dataset is served.</p>",
        "<p><a class='btn install' id=install>Install in Stremio</a>",
        "<button type=button onclick=copyUrl()>Copy URL</button></p>",
        "<script>",
        &format!("const ORIGIN={origin_js};"),
        "function rebuild(){",
        "var region=document.getElementById('region').value;",
        "var codes=Array.prototype.slice.call(document.querySelectorAll('.prov input:checked')).map(function(c){return c.value});",
        "var url=ORIGIN+'/'+region+'_'+codes.join('-')+'/manifest.json';",
        "document.getElementById('url').value=url;",
        "document.getElementById('install').href=url.replace(/^https?:/,'stremio:');",
        "document.getElementById('warn').style.display=codes.length?'none':'block';}",
        "function copyUrl(){var u=document.getElementById('url');u.select();try{document.execCommand('copy')}catch(e){}}",
        "document.addEventListener('input',rebuild);window.addEventListener('load',rebuild);",
        "</script></body></html>",
    ]
    .join("")
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
