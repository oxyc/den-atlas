# den-atlas

A self-hosted **dataset addon** for [Den](https://github.com/oxyc/den). It serves the shared **feature
store** — derived labels (genre / subgenre / mood) + quantized semantic vectors for the whole catalog —
that the Den app downloads once and refreshes, then uses on-device for **similar-titles, category rows,
primary-genre, and the billboard**. No ad-hoc queries: the app pulls one versioned, checksummed payload
and does the nearest-neighbour + ranking locally.

```
Den (Apple TV) ──GET /manifest.json──►  atlas   { resources: ["dataset"] }
               ──GET /dataset.json───►          { version, sha256, dims, labels{url}, vectors{url} }
               ──GET /labels-t01.json─►          derived labels  (≈11 MB)
               ──GET /vectors-e02.bin─►          int8 vectors    (≈21 MB)
Den (on-device) ── sha256-gated cache, stale-while-revalidate ──► ANN + categories + billboard
```

It implements the Den **`dataset`** resource (the Stremio superset — see the Den repo's
`tickets/EPIC-feature-provider-addon.md`, FP-1). A plain Stremio client ignores the unknown resource, so
installing Atlas there is harmless; only Den acts on it.

## Facts, not tokens
Atlas ships **derived data only** — no raw TMDB overviews/posters/text (ToS-clean, exactly what the Den
backfill asserts) and **nothing personal**. There is no per-user state, no token, and no `/configure`.
Personalisation (your taste vector) never leaves your device. Every blob is **sha256-pinned** in the
descriptor, so the app verifies what it downloads and a mismatch keeps the prior cache.

## Routes
| Route | Returns |
|---|---|
| `GET /` | landing page with the install URL |
| `GET /health` | `{ "status": "ok" }` |
| `GET /manifest.json` | the `dataset`-resource manifest |
| `GET /dataset.json` | the descriptor (absolute blob URLs from the request origin) |
| `GET /labels-<tax>.json` | the derived labels blob |
| `GET /vectors-<embed>.bin` | the quantized int8 vectors blob |

## The dataset
The blobs are the **same artifacts the Den app currently bundles** — `labels-t01.json` +
`vectors-e02.bin` (57,872 titles × 384-dim int8, L2-normalized ×127). Atlas is the migration off bundling
30 MB in the app: same bytes, now downloaded + refreshed instead of shipped. The descriptor declares
`embeddingModel` opaquely, so a future semantic re-embed (bge-m3) is a drop-in — publish new blobs, bump
the version, and the app re-syncs. See `docs`/the Den EPIC for the FP-2 embedding upgrade.

## Caching
Every response is cache-friendly (`src/http.ts`): a strong `ETag` (the blob's sha256) + `Last-Modified`,
honoring `If-None-Match` and `If-Modified-Since` (→ `304`), plus `HEAD`. Blob URLs in the descriptor are
version-stamped (`?v=<datasetVersion>`), so a matching hit is served `immutable` for a year while a bare
path revalidates. The 22 MB vectors blob is **range-resumable** (`Accept-Ranges` / `206`); the 11 MB labels
JSON is **gzipped** (~18×, to ~0.6 MB) transparently — the ETag/checksum is over the raw bytes, so the Den
app (which validates the decompressed payload) is unaffected. Sit a CDN in front and it caches everything
by URL with correct revalidation.

## Develop
```sh
npm install
DEN_REPO=../den npm run import   # copy the blobs from the Den repo into ./data (gitignored)
npm run dev                      # http://localhost:8080  (add /manifest.json in Den → Plugins)
npm test                         # vitest (handler + descriptor contract)
```

## Deploy
Self-hosted, Docker, behind a reverse proxy (Caddy) that terminates TLS and forwards
`X-Forwarded-Proto`/`Host` — same shape as `den-scout` / `den-trailer-service`. See [DEPLOY.md](DEPLOY.md).
