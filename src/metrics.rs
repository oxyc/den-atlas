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
    // The scheme is case-insensitive (RFC 9110 §11.1); the token is not. Surrounding whitespace is
    // trimmed, as every den addon does.
    match value.get(..7) {
        Some(scheme) if scheme.eq_ignore_ascii_case("bearer ") => {
            constant_time_eq(value[7..].trim().as_bytes(), token.as_bytes())
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
        &format!("{{version=\"{}\"}}", crate::release_version()),
        1,
    );
    // What /health says, as a number a monitor can alert on. `serious` separates "a feature class is off"
    // from "the rows are a day old": the first deserves waking someone, the second does not, and before this
    // they were indistinguishable to anything but a human reading JSON.
    let health = crate::handler::health_state(
        state.dataset.is_some(),
        state.catalog.fresh(),
        state.catalog.schema_suspect(),
        state.index.as_ref().is_some_and(|index| index.facts_unusable()),
        state.index.as_ref().is_some_and(|index| index.store_unusable()),
        state.index.as_ref().is_some_and(|index| index.rows_unusable()),
        state.index.as_ref().is_some_and(|index| index.votes_unusable()),
        state.index.as_ref().is_some_and(|index| index.votes_pending()),
    );
    let reason = health.map_or("ok", |(reason, _)| reason);
    gauge(
        &mut b,
        "atlas_health",
        "1 for the reason currently reported by /health (reason=\"ok\" when healthy).",
        &format!("{{reason=\"{}\"}}", label(reason)),
        1,
    );
    gauge(
        &mut b,
        "atlas_degraded_feature_loss",
        "1 when a degraded reason means a whole capability is off, not merely that answers are stale.",
        "",
        u64::from(health.is_some_and(|(reason, _)| crate::handler::loses_a_feature(reason))),
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
        // The store's own row count, not the manifest's claim: it is the number the bytes were checked
        // against, and the only one that cannot silently mean a different population than it says.
        let titles = u64::try_from(ds.store_rows).unwrap_or(u64::MAX);
        gauge(&mut b, "atlas_dataset_titles", "Titles in the served dataset.", "", titles);
    }
    let maturity_coverage = state.maturity_providers.coverage();
    if !maturity_coverage.is_empty() {
        b.push_str(
            "# HELP atlas_maturity_provider_titles Runtime maturity-provider title coverage by authority, media type, and status.\n\
             # TYPE atlas_maturity_provider_titles gauge\n",
        );
    }
    for coverage in maturity_coverage {
        let media_type = if coverage.media_type == den_index::MediaType::Tv { "series" } else { "movie" };
        let labels = |status: &str| {
            format!(
                "{{authority=\"{}\",media_type=\"{media_type}\",status=\"{status}\"}}",
                label(coverage.authority)
            )
        };
        for (status, value) in
            [("catalog", coverage.catalog), ("checked", coverage.checked), ("matched", coverage.matched)]
        {
            b.push_str(&format!(
                "atlas_maturity_provider_titles{} {}\n",
                labels(status),
                u64::try_from(value).unwrap_or(u64::MAX)
            ));
        }
    }
    let maturity_status = state.maturity_providers.status();
    if !maturity_status.is_empty() {
        b.push_str(
            "# HELP atlas_maturity_provider_requests_total Upstream requests made by a runtime maturity provider.\n\
             # TYPE atlas_maturity_provider_requests_total counter\n\
             # HELP atlas_maturity_provider_errors_total Failed refresh attempts by a runtime maturity provider.\n\
             # TYPE atlas_maturity_provider_errors_total counter\n\
             # HELP atlas_maturity_provider_last_success_seconds Unix time of the last refresh that checked titles.\n\
             # TYPE atlas_maturity_provider_last_success_seconds gauge\n\
             # HELP atlas_maturity_provider_last_error_seconds Unix time of the last failed refresh.\n\
             # TYPE atlas_maturity_provider_last_error_seconds gauge\n\
             # HELP atlas_maturity_provider_last_refresh_milliseconds Duration of the last refresh attempt.\n\
             # TYPE atlas_maturity_provider_last_refresh_milliseconds gauge\n\
             # HELP atlas_maturity_provider_retry_seconds Seconds left in an authority-requested or exponential backoff.\n\
             # TYPE atlas_maturity_provider_retry_seconds gauge\n\
             # HELP atlas_maturity_provider_snapshot_age_seconds Age of a provider's in-memory bulk snapshot, or zero without one.\n\
             # TYPE atlas_maturity_provider_snapshot_age_seconds gauge\n\
             # HELP atlas_maturity_provider_available Whether the configured authority initialized successfully.\n\
             # TYPE atlas_maturity_provider_available gauge\n\
             # HELP atlas_maturity_provider_persists_http_no_store Whether normalized results are deliberately persisted despite the authority's HTTP no-store directive.\n\
             # TYPE atlas_maturity_provider_persists_http_no_store gauge\n",
        );
    }
    for status in maturity_status {
        let labels = format!("{{authority=\"{}\"}}", label(status.authority));
        for (name, value) in [
            ("atlas_maturity_provider_requests_total", status.requests),
            ("atlas_maturity_provider_errors_total", status.errors),
            ("atlas_maturity_provider_last_success_seconds", status.last_success),
            ("atlas_maturity_provider_last_error_seconds", status.last_error),
            ("atlas_maturity_provider_last_refresh_milliseconds", status.last_refresh_ms),
            ("atlas_maturity_provider_retry_seconds", status.retry_seconds),
            ("atlas_maturity_provider_snapshot_age_seconds", status.snapshot_age_seconds),
            ("atlas_maturity_provider_available", u64::from(status.available)),
            ("atlas_maturity_provider_persists_http_no_store", u64::from(status.persists_http_no_store)),
        ] {
            b.push_str(&format!("{name}{labels} {value}\n"));
        }
    }
    let cache = state.maturity_providers.cache_status();
    b.push_str(
        "# HELP atlas_maturity_cache_writes_total Successful maturity-cache file replacements since process start.\n\
         # TYPE atlas_maturity_cache_writes_total counter\n\
         # HELP atlas_maturity_cache_write_bytes_total Bytes written to maturity-cache files since process start.\n\
         # TYPE atlas_maturity_cache_write_bytes_total counter\n\
         # HELP atlas_maturity_cache_write_errors_total Failed maturity-cache serialization or write attempts.\n\
         # TYPE atlas_maturity_cache_write_errors_total counter\n\
         # HELP atlas_maturity_cache_bytes Current serialized maturity-cache file size.\n\
         # TYPE atlas_maturity_cache_bytes gauge\n\
         # HELP atlas_maturity_cache_last_write_milliseconds Duration of the last successful durable cache replacement.\n\
         # TYPE atlas_maturity_cache_last_write_milliseconds gauge\n",
    );
    b.push_str(&format!(
        "atlas_maturity_cache_writes_total {}\natlas_maturity_cache_write_bytes_total {}\natlas_maturity_cache_write_errors_total {}\natlas_maturity_cache_bytes {}\natlas_maturity_cache_last_write_milliseconds {}\n",
        cache.writes, cache.bytes, cache.errors, cache.size, cache.last_write_ms
    ));
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
