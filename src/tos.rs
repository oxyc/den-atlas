//! Fail-closed guard at the HTTP serving boundary. Dataset publication already rejects expressive prose fields;
//! this catches a future dynamic route (especially an MCP/TMDB hydration path) before the response leaves atlas.

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

const TRUSTED: HeaderName = HeaderName::from_static("x-den-internal-prose-source");
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

/// Verify a JSON artifact whose bytes will later be served verbatim. This is used for the TMDB-derived metadata
/// sidecar; unlike the generated labels, it is not covered by the producer's ship guard.
pub fn verify_file(path: &Path) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let value: Value = serde_json::from_reader(file).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let keys = prohibited(&value);
    if keys.is_empty() {
        Ok(())
    } else {
        Err(format!("{} carries prohibited prose field(s): {}", path.display(), keys.join(", ")))
    }
}

/// Mark a response from a deliberately exempt source: the addon manifest/descriptor, provider catalog, or an
/// already-audited immutable artifact. The marker is private and removed before the response leaves atlas.
pub fn trusted(mut response: Response) -> Response {
    response.headers_mut().insert(TRUSTED, HeaderValue::from_static("1"));
    response
}

/// Inspect every other JSON response at the last common boundary. A future route cannot accidentally return a
/// TMDB overview merely because its author forgot to call a route-specific helper.
pub async fn guard_response(mut response: Response) -> Response {
    if response.headers_mut().remove(&TRUSTED).is_some() {
        return response;
    }
    let json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if !json || response.status() == StatusCode::NOT_MODIFIED {
        return response;
    }
    let (parts, body) = response.into_parts();
    let bytes = match to_bytes(body, MAX_DYNAMIC_JSON).await {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("serving guard refused an oversized JSON response: {error}");
            return crate::util::json_response(
                r#"{"error":"unsafe_response","detail":"response could not be audited for prohibited prose"}"#,
                StatusCode::INTERNAL_SERVER_ERROR,
            );
        }
    };
    if bytes.is_empty() {
        return Response::from_parts(parts, Body::empty());
    }
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("serving guard refused invalid JSON: {error}");
            return crate::util::json_response(
                r#"{"error":"unsafe_response","detail":"response was not valid JSON"}"#,
                StatusCode::INTERNAL_SERVER_ERROR,
            );
        }
    };
    let keys = prohibited(&value);
    if !keys.is_empty() {
        eprintln!("serving guard refused prohibited prose field(s): {}", keys.join(", "));
        return crate::util::json_response(
            r#"{"error":"unsafe_response","detail":"response contained prohibited prose fields"}"#,
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    }
    Response::from_parts(parts, Body::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prose_keys_are_caught_but_values_are_not() {
        let unsafe_value = serde_json::json!({"titles": [{"title": "X", "plotSummary": "text"}]});
        assert_eq!(prohibited(&unsafe_value), ["plotSummary"]);
        assert!(prohibited(&serde_json::json!({"title": "Overview", "label": "Plot"})).is_empty());
    }

    #[tokio::test]
    async fn the_final_response_boundary_refuses_prose() {
        let response = crate::util::json_response(r#"{"title":"X","overview":"TMDB prose"}"#, StatusCode::OK);
        let response = guard_response(response).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("unsafe_response"));
    }

    #[tokio::test]
    async fn trusted_markers_are_internal() {
        let response =
            trusted(crate::util::json_response(r#"{"description":"our manifest"}"#, StatusCode::OK));
        let response = guard_response(response).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(&TRUSTED));
    }
}
