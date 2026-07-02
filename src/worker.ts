/**
 * Cloudflare Worker entry — kept so Atlas CAN run on the edge. Excluded from the Node build
 * (tsconfig.build) and from coverage; the homelab deploy uses server.ts + Docker (like scout /
 * trailer-service). On the edge the ~33 MB blobs don't fit in a Worker bundle, so they live in an R2
 * bucket bound as `env.ATLAS_BLOBS`; the Worker streams them and computes the descriptor from the object
 * metadata. Requires @cloudflare/workers-types to typecheck, which this repo doesn't install by default —
 * it's a reference entry, not the CI path.
 */
// @ts-nocheck
import { buildManifest } from "./manifest.js";
import { json } from "./util.js";

export default {
  async fetch(request: Request, env: Record<string, any>): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname;
    const origin = `${request.headers.get("x-forwarded-proto") ?? "https"}://${request.headers.get("host") ?? url.host}`;

    if (path === "/health") return json({ status: "ok" });
    if (path === "/manifest.json") return json(buildManifest());

    if (path === "/dataset.json") {
      // Read object metadata (custom sha256 + size stored at upload) — never buffer the blob to describe it.
      const [labels, vectors] = await Promise.all([env.ATLAS_BLOBS.head("labels-t01.json"), env.ATLAS_BLOBS.head("vectors-e02.bin")]);
      if (!labels || !vectors) return json({ error: "dataset_unavailable" }, 503);
      return json({
        datasetVersion: labels.customMetadata?.datasetVersion ?? "unknown",
        taxonomyVersion: "t01",
        embeddingModel: "e02",
        dims: 384,
        count: Number(labels.customMetadata?.count ?? 0),
        quantization: "int8-symmetric-x127",
        labels: { url: `${origin}/labels-t01.json`, sha256: labels.customMetadata?.sha256 ?? "", bytes: labels.size },
        vectors: { url: `${origin}/vectors-e02.bin`, sha256: vectors.customMetadata?.sha256 ?? "", bytes: vectors.size },
      });
    }

    if (path === "/labels-t01.json" || path === "/vectors-e02.bin") {
      const obj = await env.ATLAS_BLOBS.get(path.slice(1));
      if (!obj) return json({ error: "not_found" }, 404);
      const contentType = path.endsWith(".json") ? "application/json" : "application/octet-stream";
      return new Response(obj.body, { headers: { "content-type": contentType, etag: obj.httpEtag } });
    }

    return json({ error: "not_found" }, 404);
  },
};
