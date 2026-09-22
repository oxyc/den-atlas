//! Fail-closed guard at the HTTP serving boundary. Dataset publication already rejects expressive prose fields;
//! this catches a future dynamic route (especially an MCP/TMDB hydration path) before the response leaves atlas.
//!
//! `verify_file` used to sit beside it, auditing the TMDB-derived metadata sidecar once at load because that
//! file went out verbatim under `trusted` and so never reached `guard_response`. Nothing is served verbatim
//! any more (#113): every TMDB-derived value atlas emits is built by a route and passes through
//! `guard_response` below, so the boundary check is the whole guard.
//!
//! What it audits is decided here and nowhere else: every response whose body is, or may be, JSON — whatever
//! `+json` name it goes by, or none — except on the two exact paths `exempt` names. A new route is guarded
//! by default and cannot opt out without changing `exempt` and the test that pins it.

use axum::body::{to_bytes, Body};
use axum::http::{header, StatusCode};
use axum::response::Response;
use serde_json::Value;
use std::collections::BTreeSet;

const MAX_DYNAMIC_JSON: usize = 2 * 1024 * 1024;
const PROHIBITED: &[&str] = &[
    "overview",
    "summary",
    "synopsis",
    "description",
    "plot",
    "plotsummary",
    "tagline",
    "storyline",
    "premise",
    "abstract",
    "blurb",
    "logline",
    "review",
];

fn normalized(key: &str) -> String {
    key.chars().filter(char::is_ascii_alphanumeric).flat_map(char::to_lowercase).collect()
}

fn walk(value: &Value, found: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if PROHIBITED.contains(&normalized(key).as_str()) {
                    found.insert(key.clone());
                }
                walk(child, found);
            }
        }
        Value::Array(array) => array.iter().for_each(|child| walk(child, found)),
        _ => {}
    }
}

pub fn prohibited(value: &Value) -> Vec<String> {
    let mut found = BTreeSet::new();
    walk(value, &mut found);
    found.into_iter().collect()
}

/// The routes whose bodies are exempt, by exact path (the install config segment already removed). Both carry
/// atlas's own prose under a key the guard refuses — the Stremio manifest's `description` is required by that
/// protocol — and neither holds anything derived from TMDB. Exact paths, not prefixes, so no route can be hung
/// under an exemption by accident.
pub fn exempt(route: &str) -> bool {
    matches!(route, "/manifest.json" | "/dataset.json")
}

/// Whether a body is audited: any JSON media type (`application/json`, `text/json`, any `+json` suffix), and a
/// body whose type is absent or unreadable, since nothing then says it is not JSON.
fn audited(content_type: Option<&header::HeaderValue>) -> bool {
    let Some(Ok(content_type)) = content_type.map(|value| value.to_str()) else { return true };
    let essence = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    essence == "application/json" || essence == "text/json" || essence.ends_with("+json")
}

fn refused(detail: &str) -> Response {
    crate::util::json_response(
        serde_json::json!({ "error": "unsafe_response", "detail": detail }).to_string(),
        StatusCode::INTERNAL_SERVER_ERROR,
    )
}

/// Inspect every response at the last common boundary. A future route cannot accidentally return a TMDB
/// overview merely because its author forgot to call a route-specific helper.
pub async fn guard_response(response: Response, exempt: bool) -> Response {
    if exempt || response.status() == StatusCode::NOT_MODIFIED {
        return response;
    }
    if !audited(response.headers().get(header::CONTENT_TYPE)) {
        return response;
    }
    // A byte range of a JSON body cannot be audited: the keys that would refuse it may lie outside the slice,
    // while the slice itself — a bare string value, say — still parses. The whole body is the only thing the
    // guard can judge, so a partial one is refused rather than passed.
    if response.status() == StatusCode::PARTIAL_CONTENT {
        eprintln!("serving guard refused a byte range of a JSON response");
        return refused("a byte range of a JSON response cannot be audited for prohibited prose");
    }
    let (parts, body) = response.into_parts();
    let bytes = match to_bytes(body, MAX_DYNAMIC_JSON).await {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("serving guard refused an oversized JSON response: {error}");
            return refused("response could not be audited for prohibited prose");
        }
    };
    if bytes.is_empty() {
        return Response::from_parts(parts, Body::empty());
    }
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("serving guard refused invalid JSON: {error}");
            return refused("response was not valid JSON");
        }
    };
    let keys = prohibited(&value);
    if !keys.is_empty() {
        eprintln!("serving guard refused prohibited prose field(s): {}", keys.join(", "));
        return refused("response contained prohibited prose fields");
    }
    Response::from_parts(parts, Body::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn guarded(content_type: Option<&str>, status: StatusCode, body: &'static str) -> StatusCode {
        let mut response = Response::builder().status(status);
        if let Some(content_type) = content_type {
            response = response.header(header::CONTENT_TYPE, content_type);
        }
        guard_response(response.body(Body::from(body)).unwrap(), false).await.status()
    }

    #[test]
    fn prose_keys_are_caught_but_values_are_not() {
        let unsafe_value = serde_json::json!({"titles": [{"title": "X", "plotSummary": "text"}]});
        assert_eq!(prohibited(&unsafe_value), ["plotSummary"]);
        assert!(prohibited(&serde_json::json!({"title": "Overview", "label": "Plot"})).is_empty());
    }

    #[tokio::test]
    async fn the_final_response_boundary_refuses_prose() {
        let response = crate::util::json_response(r#"{"title":"X","overview":"TMDB prose"}"#, StatusCode::OK);
        let response = guard_response(response, false).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("unsafe_response"));
    }

    #[tokio::test]
    async fn an_exempt_route_passes_unread() {
        let response = crate::util::json_response(r#"{"description":"our manifest"}"#, StatusCode::OK);
        assert_eq!(guard_response(response, true).await.status(), StatusCode::OK);
    }

    /// The provider catalog and the title-search catalog live under `/catalog/`, and so would any route
    /// someone later hung beside them; an MCP route is guarded wherever it is mounted. Only the two exact
    /// paths whose own prose the guard would refuse are exempt.
    #[test]
    fn only_the_manifest_and_the_descriptor_are_exempt() {
        assert!(exempt("/manifest.json"));
        assert!(exempt("/dataset.json"));
        for route in [
            "/catalog/movie/jw-nfx.json",
            "/catalog/movie/den-titles/search=x.json",
            "/catalog/mcp.json",
            "/mcp",
            "/mcp/tools/call",
            "/index/query.json",
            "/index/schema.json",
            "/manifest.json/mcp",
            "/dataset.json/mcp",
            "/",
        ] {
            assert!(!exempt(route), "{route} escapes the prose guard");
        }
    }

    /// JSON under any of its names is JSON, and a body that names no type is audited rather than trusted.
    #[tokio::test]
    async fn json_by_any_name_is_audited() {
        let prose = r#"{"overview":"TMDB prose"}"#;
        for content_type in [
            Some("application/json"),
            Some("application/json; charset=utf-8"),
            Some("Application/JSON"),
            Some("application/problem+json"),
            Some("application/vnd.api+json"),
            Some("text/json"),
            None,
        ] {
            let status = guarded(content_type, StatusCode::OK, prose).await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{content_type:?} passed prose");
        }
        assert_eq!(
            guarded(Some("text/html; charset=utf-8"), StatusCode::OK, "<p>x</p>").await,
            StatusCode::OK
        );
        assert_eq!(guarded(Some("text/plain"), StatusCode::OK, "den_up 1").await, StatusCode::OK);
    }

    /// `Range: bytes=…` on a JSON route slices the body before the guard sees it. The slice of a refused
    /// body can be a bare string that parses cleanly and holds exactly the prose the whole was refused for.
    #[tokio::test]
    async fn a_byte_range_of_json_is_refused() {
        let slice = r#""the value of a prohibited key""#;
        let status = guarded(Some("application/json"), StatusCode::PARTIAL_CONTENT, slice).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(guarded(Some("application/json"), StatusCode::OK, slice).await, StatusCode::OK);
    }
}
