/**
 * Node/Bun entry (Hono). A thin adapter: it loads the dataset once at boot and hands every request to the
 * runtime-agnostic `handleAtlas` core. The homelab runs this behind Caddy (https + a stable domain);
 * `handleAtlas` reads `X-Forwarded-Proto`/`Host` to build correct absolute blob URLs. `PUBLIC_BASE_URL`
 * overrides the origin (e.g. a CDN in front of the blobs).
 */
import { Hono } from "hono";
import { serve } from "@hono/node-server";
import { handleAtlas } from "./handler.js";
import { loadDataset } from "./dataset.js";
const dataDir = process.env.ATLAS_DATA_DIR ?? "data";
const dataset = await loadDataset(dataDir);
const publicBaseUrl = process.env.PUBLIC_BASE_URL;
const app = new Hono();
app.all("*", (c) => handleAtlas(c.req.raw, { dataset, publicBaseUrl }));
const port = Number(process.env.PORT ?? 8080);
serve({ fetch: app.fetch, port });
// eslint-disable-next-line no-console
console.log(`den-atlas listening on :${port} — ${dataset.meta.count} titles ` +
    `(${dataset.meta.embeddingModel}/${dataset.meta.taxonomyVersion})`);
export { app };
