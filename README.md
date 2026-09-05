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
| `GET /manifest.json` | the `dataset` + `catalog` manifest |
| `GET /dataset.json` | the descriptor (absolute blob URLs from the request origin) |
| `GET /labels-<tax>.json` | the derived labels blob |
| `GET /vectors-<embed>.bin` | the quantized int8 vectors blob |
| `GET /catalog/<type>/<id>.json` | a "most popular" row of `{id,type,name,poster}` metas |

## The dataset

**Verified live 2026-09-05** by fetching `/dataset.json` from the deployed addon:

| field | value |
|---|---|
| `datasetVersion` | `ebe9ab936444` (a **content hash**, not a semver — it moves when content moves, and that is what triggers client re-sync) |
| `taxonomyVersion` | `t02` |
| `embeddingModel` / `dims` | `bge-m3` / **1024** |
| `count` | 37,533 |
| `quantization` | `int8-symmetric-x127` |

This matches `den-dataset/out-t02` exactly, so that directory is what is being served.

Three things about the data that keep being got wrong — see `den-dataset/README.md` for the measurements:

- **37,533 is not the catalogue.** den-dataset enriches 57,715 titles; the 20,182 without a Wikipedia plot
  are dropped by a ToS rule, and they have **no labels and no vectors** — the two cover the identical ids.
  Consumers must degrade for 35% of the catalogue, not treat it as an edge case.
- **There are two vector spaces.** The plot index (`vectors-bge-m3.bin`) and the premise index
  (`vectors-premise.bin`, Sonnet-generated premise tags embedded with the same model) are complements, not
  duplicates — measured mean |cos| 0.43. The app's "More Like This" leads with premise.
- **A bare TMDB id is ambiguous**: movie and TV namespaces overlap (tv 95 is Buffy, movie 95 is
  Armageddon). Key every per-title map on `(mediaType, id)`.

*(This section previously described `labels-t01.json` + 384-dim `vectors-e02.bin` and called bge-m3 a
future upgrade. That was two taxonomy generations out of date.)*

## Catalogs (JustWatch)
Alongside the dataset, Atlas serves Stremio **catalog** rows of "most popular" titles per streaming
service — `Popular on Netflix`, `Max`, `Prime Video`, `Disney+`, `Apple TV+` — plus a headline
**Trending Everywhere** row that unions the services and re-ranks by inverse-rank-sum (a title trending
on several services floats up). Data is the unofficial **JustWatch** GraphQL API (public, tokenless),
fetched server-side and cached in-process (~6h, serve-stale-on-error). Each meta carries the **IMDb id**
(a plain Stremio client + Cinemeta resolve the detail page from it) plus JustWatch's **TMDB id** as
`moviedb_id` — the key the Den app maps rows through (it bridges everything via TMDB). No TMDB API calls,
no `meta` resource. The module is fully **isolated**: if JustWatch breaks, catalog rows go empty and the
dataset resource is unaffected. Tunables: `JW_COUNTRY`, `JW_PROVIDERS`, `JW_CACHE_TTL_SECS`. Catalog data from
JustWatch.

## Implementation
A small **Rust** (axum + tokio) server — a ~0.8 MB static musl binary, **~2–4 MB RSS** serving 33 MB of data.
Blob bodies are **streamed from disk** (never loaded into RAM), gzip is precomputed to a file, and sha256 is
read from the `dataset.meta.json` sidecar (no startup hashing). (The original TypeScript server is preserved
at the `legacy-ts` git tag.)

## Caching
Every response is cache-friendly (`src/http.rs`): a strong `ETag` (the blob's sha256, distinct `-gzip`
variant) + `Last-Modified`, honoring `If-None-Match` and `If-Modified-Since` (→ `304`), plus `HEAD`. Blob
URLs in the descriptor are version-stamped (`?v=<datasetVersion>`), so a matching hit is served `immutable`
for a year while a bare path revalidates. The 22 MB vectors blob is **range-resumable** (`Accept-Ranges` /
`206`); the 11 MB labels JSON is **gzipped** (~18×, to ~0.5 MB) transparently — the ETag/checksum is over the
raw bytes, so the Den app (which validates the decompressed payload) is unaffected. Sit a CDN in front and it
caches everything by URL with correct revalidation.

## Develop
```sh
scripts/fetch-dataset.sh   # prep ./data from the den-dataset `data-latest` release (labels + vectors + gzip + meta)
cargo run                  # http://localhost:8080  (add /manifest.json in Den → Plugins)
cargo test                 # the caching layer (ETag / Range / gzip / 304)
```
The dataset is produced by [den-dataset](https://github.com/oxyc/den-dataset) (`taxonomy-backfill finalize`
→ `publish-dataset.sh`) and published as a GitHub Release — the single source of truth this server and the
Den app both fetch. den-atlas no longer reads the Den repo.

## Deploy
Self-hosted, Docker, behind a reverse proxy (Caddy) that terminates TLS and forwards
`X-Forwarded-Proto`/`Host` — same shape as `den-scout` / `den-trailer-service`. See [DEPLOY.md](DEPLOY.md).
