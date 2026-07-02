/**
 * Den Atlas — the runtime-agnostic core. A single `handleAtlas(request, deps)` over Web `Request`/
 * `Response`, so it runs unchanged on Node (via `@hono/node-server`) or a CF Worker; `deps` injects the
 * loaded dataset (mocked in tests).
 *
 * Non-personal by design: Atlas serves ONE shared, derived, ToS-clean dataset to everyone — no per-user
 * state, no token, no config. The app downloads it, caches it on-device (checksum-gated), and refreshes.
 *
 * Caching (see http.ts): every response carries a strong `ETag` and honors `If-None-Match` /
 * `If-Modified-Since` (→ 304) + `HEAD`; blobs add `Range`/206 + gzip. Blob URLs from the descriptor are
 * version-stamped, so a `?v=<current>` hit is served `immutable`; a bare hit revalidates.
 *
 * Routes (served at the service root — no config prefix, unlike scout):
 *   GET|HEAD /                     → a tiny landing page with the install URL
 *   GET|HEAD /health               → { status: "ok" }
 *   GET|HEAD /manifest.json        → the `dataset`-resource manifest
 *   GET|HEAD /dataset.json         → the descriptor (absolute, version-stamped blob URLs)
 *   GET|HEAD /labels-<tax>.json    → the derived labels blob (gzip-negotiated)
 *   GET|HEAD /vectors-<embed>.bin  → the quantized int8 vectors blob (range-resumable)
 */
import { buildManifest } from "./manifest.js";
import { buildDescriptor } from "./descriptor.js";
import { json, html, publicOrigin, fnv1a } from "./util.js";
import { serveBytes } from "./http.js";
export function handleAtlas(request, deps) {
    const method = request.method.toUpperCase();
    if (method !== "GET" && method !== "HEAD") {
        return json({ error: "method_not_allowed" }, 405);
    }
    const url = new URL(request.url);
    const path = url.pathname;
    const origin = publicOrigin(request, deps.publicBaseUrl);
    if (path === "/" || path === "/configure" || path === "/configure/")
        return html(landingPage(origin, deps.dataset));
    if (path === "/health")
        return json({ status: "ok" });
    if (path === "/manifest.json")
        return serveJson(request, buildManifest(), "public, max-age=3600");
    if (path === "/dataset.json") {
        // Short TTL + ETag: this is the freshness signal the app polls, so a revisit that finds nothing new is
        // a cheap 304, but a new version is noticed within the window.
        return serveJson(request, buildDescriptor(origin, deps.dataset), "public, max-age=300", deps.dataset.lastModified);
    }
    if (path === `/${deps.dataset.labels.name}`)
        return serveBlob(request, deps.dataset, deps.dataset.labels, url);
    if (path === `/${deps.dataset.vectors.name}`)
        return serveBlob(request, deps.dataset, deps.dataset.vectors, url);
    return json({ error: "not_found" }, 404);
}
/** A blob with full validators. `?v=<current datasetVersion>` ⇒ immutable for a year (the URL is unique to
 * the content); a bare request revalidates (a republish reuses the path) — the ETag makes that a 304. */
function serveBlob(request, dataset, blob, url) {
    const pinned = url.searchParams.get("v") === dataset.meta.datasetVersion;
    const cacheControl = pinned ? "public, max-age=31536000, immutable" : "public, max-age=3600";
    return serveBytes(request, {
        bytes: blob.bytes,
        etag: blob.sha256,
        contentType: blob.contentType,
        cacheControl,
        lastModified: dataset.lastModified,
        gzip: blob.gzip,
    });
}
function serveJson(request, body, cacheControl, lastModified) {
    const text = JSON.stringify(body);
    return serveBytes(request, {
        bytes: new TextEncoder().encode(text),
        etag: fnv1a(text),
        contentType: "application/json",
        cacheControl,
        lastModified,
    });
}
function landingPage(origin, dataset) {
    const manifestURL = `${escapeHtml(origin)}/manifest.json`;
    const { count, taxonomyVersion, embeddingModel } = dataset.meta;
    return [
        "<!doctype html><html><head><meta charset=utf-8>",
        "<meta name=viewport content='width=device-width,initial-scale=1'>",
        "<title>Den Atlas</title>",
        "<style>body{font:16px/1.5 system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem;color:#222}",
        "code{background:#f2f2f2;padding:.15rem .35rem;border-radius:4px;word-break:break-all}</style></head><body>",
        "<h1>Den Atlas</h1>",
        "<p>A self-hosted <b>dataset addon</b> for Den — derived labels + semantic vectors for the whole catalog.</p>",
        `<p>Currently serving <b>${count.toLocaleString("en-US")}</b> titles ` +
            `(<code>${escapeHtml(taxonomyVersion)}</code> / <code>${escapeHtml(embeddingModel)}</code>).</p>`,
        "<p>Add this URL in Den → Settings → Plugins:</p>",
        `<p><code>${manifestURL}</code></p>`,
        "</body></html>",
    ].join("");
}
function escapeHtml(s) {
    return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}
