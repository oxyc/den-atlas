//! `GET /metrics` — Prometheus text format, written by hand. The format is a dozen lines of printing,
//! and a client library would be a dependency for that alone.
//!
//! Every series is a fact the addon already has: the dataset's descriptor fields, and the same three
//! signals `/health` folds into one status. Nothing here counts or remembers anything, so it is computed
//! only when asked and costs an idle box nothing.
//!
//! Behind a bearer token and OFF when none is configured, the same contract as the other den addons.
//! Unset or wrong both answer the ordinary 404, so an operator who has not configured a token is not
//! told there is something here to poke at.

use crate::AppState;
use axum::http::{header, HeaderMap};

/// Whether the request carries `Authorization: Bearer <token>` for the configured token.
pub fn authorized(headers: &HeaderMap, token: Option<&str>) -> bool {
    let Some(token) = token else { return false };
    let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    // The scheme is case-insensitive (RFC 9110 §11.1); the token is not.
    match value.get(..7) {
        Some(scheme) if scheme.eq_ignore_ascii_case("bearer ") => {
            constant_time_eq(&value.as_bytes()[7..], token.as_bytes())
        }
        _ => false,
    }
}

/// Byte comparison whose time does not depend on WHERE the inputs differ, so the token cannot be
/// recovered a byte at a time from response timings. A length mismatch returns early, which leaks only
/// the length.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

pub fn render(state: &AppState) -> String {
    let mut b = String::with_capacity(1024);
    gauge(
        &mut b,
        "atlas_build_info",
        "Version of the running binary.",
        &format!("{{version=\"{}\"}}", env!("CARGO_PKG_VERSION")),
        1,
    );
    let ds = state.dataset.as_ref();
    gauge(
        &mut b,
        "atlas_dataset_loaded",
        "Dataset loaded (1), or serving manifest and catalog only (0).",
        "",
        ds.is_some() as u64,
    );
    // Absent rather than zero when there is no dataset: a titles gauge of 0 would read as an empty
    // dataset, which is a different failure from none at all.
    if let Some(ds) = ds {
        let m = &ds.meta;
        gauge(
            &mut b,
            "atlas_dataset_info",
            "The dataset being served.",
            &format!(
                "{{dataset_version=\"{}\",taxonomy=\"{}\",embedding_model=\"{}\"}}",
                label(&m.dataset_version),
                label(&m.taxonomy_version),
                label(&m.embedding_model)
            ),
            1,
        );
        gauge(&mut b, "atlas_dataset_titles", "Titles in the served dataset.", "", m.count);
    }
    gauge(
        &mut b,
        "atlas_catalog_fresh",
        "Last JustWatch refresh succeeded (1) or failed and stale rows are served (0).",
        "",
        state.catalog.fresh() as u64,
    );
    gauge(
        &mut b,
        "atlas_catalog_schema_suspect",
        "A JustWatch chart still being served came back looking like a schema break.",
        "",
        state.catalog.schema_suspect() as u64,
    );
    b
}

fn gauge(b: &mut String, name: &str, help: &str, labels: &str, value: u64) {
    b.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name}{labels} {value}\n"));
}

/// A label value escaped for the exposition format. These come from `dataset.meta.json`, which is
/// fetched from a release over the network; a stray quote must not break every series after it.
fn label(v: &str) -> String {
    v.replace('\\', r"\\").replace('"', "\\\"").replace('\n', "\\n")
}
