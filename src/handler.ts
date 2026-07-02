/**
 * Den Atlas — the runtime-agnostic core. A single `handleAtlas(request, deps)` over Web `Request`/
 * `Response`, so it runs unchanged on Node (via `@hono/node-server`) or a CF Worker; `deps` injects the
 * loaded dataset (mocked in tests).
 *
 * Non-personal by design: Atlas serves ONE shared, derived, ToS-clean dataset to everyone — no per-user
 * state, no token, no config. The app downloads it, caches it on-device (checksum-gated), and refreshes.
 *
 * Routes (served at the service root — no config prefix, unlike scout):
 *   GET /                     → a tiny landing page with the install URL
 *   GET /health               → { status: "ok" }
 *   GET /manifest.json        → the `dataset`-resource manifest
 *   GET /dataset.json         → the descriptor (absolute blob URLs from the request origin)
 *   GET /labels-<tax>.json    → the derived labels blob
 *   GET /vectors-<embed>.bin  → the quantized int8 vectors blob
 */
import { buildManifest } from "./manifest.js";
import { buildDescriptor } from "./descriptor.js";
import type { DatasetArtifacts, DatasetBlob } from "./dataset.js";
import { json, html, publicOrigin } from "./util.js";

export interface AtlasDeps {
  dataset: DatasetArtifacts;
  /** Override the origin used in descriptor blob URLs (e.g. a CDN in front). Else derived from headers. */
  publicBaseUrl?: string;
}

export function handleAtlas(request: Request, deps: AtlasDeps): Response {
  const path = new URL(request.url).pathname;
  const origin = publicOrigin(request, deps.publicBaseUrl);

  if (path === "/" || path === "/configure" || path === "/configure/") return html(landingPage(origin, deps.dataset));
  if (path === "/health") return json({ status: "ok" });
  if (path === "/manifest.json") return json(buildManifest());
  if (path === "/dataset.json") return json(buildDescriptor(origin, deps.dataset));
  if (path === `/${deps.dataset.labels.name}`) return blobResponse(request, deps.dataset.labels);
  if (path === `/${deps.dataset.vectors.name}`) return blobResponse(request, deps.dataset.vectors);
  return json({ error: "not_found" }, 404);
}

/** Serve a blob with a sha256 `ETag` + conditional-GET support. The Den app skips re-downloading via the
 * descriptor checksum, so a refresh usually costs nothing; `If-None-Match` is the belt for any client that
 * does re-request. */
function blobResponse(request: Request, blob: DatasetBlob): Response {
  const etag = `"${blob.sha256}"`;
  if (request.headers.get("if-none-match") === etag) {
    return new Response(null, { status: 304, headers: { etag } });
  }
  return new Response(blob.bytes, {
    status: 200,
    headers: {
      "content-type": blob.contentType,
      "content-length": String(blob.size),
      etag,
      "cache-control": "public, max-age=86400",
    },
  });
}

function landingPage(origin: string, dataset: DatasetArtifacts): string {
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

function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
}
