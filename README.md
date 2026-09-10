# den-atlas

A self-hosted **dataset addon** for [Den](https://github.com/oxyc/den). It serves the shared **feature
store** — derived labels (genre / subgenre / mood) + quantized semantic vectors for the whole catalog —
that the Den app downloads once and refreshes, then uses on-device for **similar-titles, category rows,
primary-genre, and the billboard**. No ad-hoc queries: the app pulls one versioned, checksummed payload
and does the nearest-neighbour + ranking locally.

```
Den (Apple TV) ──GET /manifest.json──►  atlas   { resources: ["dataset"] }
               ──GET /dataset.json───►          { version, sha256, dims, labels{url}, vectors{url} }
               ──GET /labels-t02.json─►          derived labels
               ──GET /vectors-bge-m3.bin─►       int8 vectors
Den (on-device) ── sha256-gated cache, stale-while-revalidate ──► ANN + categories + billboard
```

It implements the Den **`dataset`** resource (the Stremio superset — see the Den repo's
`tickets/EPIC-feature-provider-addon.md`, FP-1). A plain Stremio client ignores the unknown resource, so
installing Atlas there is harmless; only Den acts on it.

## Facts, not tokens
Atlas ships **derived data only** — no raw TMDB overviews/posters/text (ToS-clean, exactly what the Den
backfill asserts) and **nothing personal**. There is no per-user state and no token; `/configure` only
picks the catalog region and services, carried in plaintext in the install URL. Personalisation (your
taste vector) never leaves your device. Every blob is **sha256-pinned** in the descriptor, so the app
verifies what it downloads and a mismatch keeps the prior cache.

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

The app re-syncs only when `datasetVersion`/`embeddingModel`/`taxonomyVersion` moves; unchanged blobs are
served from the on-device cache with no re-download. A change of `embeddingModel` forces a clean re-sync
into the new vector space.

## Catalogs (JustWatch)
Alongside the dataset, Atlas serves Stremio **catalog** rows of "most popular" titles per streaming
service — `Popular on Netflix`, `Max`, `Prime Video`, `Disney+`, `Apple TV+` — plus a headline
**Trending Everywhere** row that unions the services and re-ranks by inverse-rank-sum (a title trending
on several services floats up). Data is the unofficial **JustWatch** GraphQL API (public, tokenless),
fetched server-side and cached in-process (~6h, serve-stale-on-error). Each meta carries the **IMDb id**
(a plain Stremio client + Cinemeta resolve the detail page from it) plus JustWatch's **TMDB id** as
`moviedb_id` — the key the Den app maps rows through (it bridges everything via TMDB). No TMDB API calls,
no `meta` resource. The module is fully **isolated**: if JustWatch breaks, catalog rows go empty and the
dataset resource is unaffected; with no outbound internet the rows are simply empty. Tunables:
`JW_COUNTRY`, `JW_PROVIDERS`, `JW_CACHE_TTL_SECS`. Catalog data from JustWatch.

## Implementation
A small **Rust** (axum + tokio) server — a ~0.8 MB static musl binary, **~2–4 MB RSS** whatever the dataset size.
Blob bodies are **streamed from disk** (never loaded into RAM), gzip is precomputed to a file, and sha256 is
read from the `dataset.meta.json` sidecar (no startup hashing). (The original TypeScript server is preserved
at the `legacy-ts` git tag.)

## Caching
Every response is cache-friendly (`src/http.rs`): a strong `ETag` (the blob's sha256, distinct `-gzip`
variant) + `Last-Modified`, honoring `If-None-Match` and `If-Modified-Since` (→ `304`), plus `HEAD`. Blob
URLs in the descriptor are version-stamped (`?v=<datasetVersion>`), so a matching hit is served `immutable`
for a year while a bare path revalidates. Every blob is **range-resumable** (`Accept-Ranges` / `206`); the
labels JSON (and the metadata sidecar, when the release publishes a `.gz`) is **gzipped** transparently — the
ETag/checksum is over the raw bytes, so the Den app (which validates the decompressed payload) is
unaffected. Sit a CDN in front and it caches everything by URL with correct revalidation.

`dataset.json` builds its absolute blob URLs from `X-Forwarded-Proto` + `X-Forwarded-Host`/`Host` (and
names them in `Vary`), so a proxy in front that forwards those gets URLs on its own origin. To serve the
blobs from a CDN instead, set `PUBLIC_BASE_URL=https://cdn.example.com/atlas` and point the CDN at this
origin.

## Routes
| Route | Returns |
|---|---|
| `GET /`, `GET /configure` | landing/configure page: pick region + services, get the install URL |
| `GET /health` | always `200`: `{"status":"ok"}`, or `{"status":"degraded","reason":…,"detail":…}` with reason `dataset_unavailable`, `stale_catalog` (last JustWatch refresh failed) or `catalog_schema_suspect` (a served chart came back mostly empty) |
| `GET /manifest.json` | the `dataset` + `catalog` manifest (also under a `/<region>_<codes>/` install prefix) |
| `GET /dataset.json` | the descriptor (absolute blob URLs from the request origin); `503` when the dataset did not load |
| `GET /labels-<tax>.json` | the derived labels blob |
| `GET /vectors-<embed>.bin` | the quantized int8 vectors blob |
| `GET /<blob>` | the optional blobs the descriptor names: metadata sidecar, premise labels + vectors, facets |
| `GET /catalog/<type>/<id>[/<extra>].json` | a "most popular" row of `{id,type,name,poster}` metas |
| `POST /embed` | a search query (`{"text":…}`) embedded by den-embed; `503` when `EMBED_URL` is unset |
| `GET /metrics` | Prometheus text for `Authorization: Bearer $METRICS_TOKEN`; `404` when the token is unset or wrong |

Every response carries `Access-Control-Allow-Origin: *` and `OPTIONS` answers the CORS preflight. An
unknown path — and a refused `/metrics` — is a `404` `{"error":"not_found"}` with `cache-control: no-store`.

`POST /embed` forwards the query to the internal [den-embed](https://github.com/oxyc/den-embed) service and
returns its int8 vector (`{"vector":…,"dims":1024,"model":"bge-m3"}`), so a query embeds through the SAME
bge-m3 + quantizer that built the corpus and the two are comparable. den-embed stays internal — only Atlas
is exposed.

`/metrics` publishes only what the addon already knows: `atlas_build_info{version}`,
`atlas_dataset_loaded`, `atlas_dataset_info{dataset_version,taxonomy,embedding_model}`,
`atlas_dataset_titles`, and the two catalog signals behind `/health` — `atlas_catalog_fresh` and
`atlas_catalog_schema_suspect`.

## Configuration
Every variable is optional; the binary reads the process environment only (no `.env` loader).
[`.env.example`](.env.example) carries the same list.

| Variable | Default | Purpose |
|---|---|---|
| `PORT` | `8080` | the port to listen on |
| `DATA_DIR` | `data` (the image sets `/app/data`) | directory holding `dataset.meta.json` and the blobs it declares |
| `PUBLIC_BASE_URL` | derived from the request | origin for the descriptor's blob URLs — set it to serve blobs from a CDN |
| `JW_COUNTRY` | `US` | catalog country when an `auto` install forwards none |
| `JW_PROVIDERS` | all | provider subset for an install with no `<region>_<codes>` segment |
| `JW_CACHE_TTL_SECS` | `21600` | in-process freshness of the catalog rows |
| `EMBED_URL` | unset | den-embed base URL for `POST /embed`; unset ⇒ `/embed` answers `503` |
| `METRICS_TOKEN` | unset | bearer token for `GET /metrics`; unset or empty ⇒ `404` |

## Run
```sh
scripts/fetch-dataset.sh   # prep ./data from the den-dataset `data-latest` release (labels + vectors + gzip + meta)
cargo run                  # http://localhost:8080  (add /manifest.json in Den → Plugins)
cargo test                 # the caching layer (ETag / Range / gzip / 304), routes, catalog, shutdown
```
`fetch-dataset.sh` is anonymous (needs curl, python3 and shasum). It downloads every blob
`dataset.meta.json` declares — the labels (and their `.gz`), the vectors, the poster sidecar, the premise
index and `facets.bin` — verifies each against the meta's sha256, and only then moves them into `./data`.
The server reads all of that at startup; it never hashes or compresses. To pick up a new release, re-run it
and restart the server.

The dataset is produced by [den-dataset](https://github.com/oxyc/den-dataset) (`taxonomy-backfill finalize`
→ `publish-dataset.sh`) and published as a GitHub Release — the single source of truth this server and the
Den app both fetch. den-atlas no longer reads the Den repo.

The published image (built by `.github/workflows/docker-publish.yml` on a `v*` tag, after `ci.yml` passes
and only if the tag equals the `Cargo.toml` version) is the **server only**, so mount the data:
```sh
docker run -d --name den-atlas -p 8080:8080 --read-only \
  -v "$PWD/data:/app/data:ro" ghcr.io/oxyc/den-atlas:latest
```
`docker build -t den-atlas .` bakes whatever is in `./data` into the image instead.

## Deploy
The live deployment is the Den homelab box: a Podman Quadlet unit (`deploy/quadlet/den-atlas.container`)
from the den repo's `deploy/`, whose `deploy/README.md` covers provisioning, the health-gated updater and
rollback.

- `ghcr.io/oxyc/den-atlas`, host port **8081** → 8080, 256 MiB memory cap, read-only root filesystem,
  every capability dropped, uid 65532; environment from `/etc/den/env/den-atlas.env`.
- The dataset is bind-mounted **read-only** from `/var/lib/den/atlas-data` at `/app/data`, kept current
  by the `atlas-dataset-sync` timer, which restarts atlas only when the release actually changed.
- Image updates go through `den-update`: a new `:latest` is proved against `/health` and `/manifest.json`
  in a throwaway container before its digest is pinned, and a failed proof rolls back.
- `EMBED_URL=http://den-embed:8080` over the shared Podman network enables `POST /embed`.
- `PUBLIC_BASE_URL` is only needed to point blob downloads at a CDN in front.

Smoke test:
```sh
curl -s localhost:8081/health                        # {"status":"ok"}, or "degraded" with a reason
curl -s localhost:8081/manifest.json | jq .resources # ["dataset","catalog"]
curl -s -H 'x-forwarded-proto: https' -H 'host: atlas.example.com' \
     localhost:8081/dataset.json | jq '.count, .vectors.url'
curl -s localhost:8081/catalog/movie/jw-nfx.json | jq '.metas | length'  # live JustWatch (needs egress)
curl -s -H "Authorization: Bearer $METRICS_TOKEN" localhost:8081/metrics  # only with METRICS_TOKEN set
```

To install: Den → Settings → Plugins → add `http://<den-ip>:8081/manifest.json` (the app needs https or a
LAN/private-range host over http). Den then syncs the dataset instead of using its bundled copy; removing
the addon falls back to the bundled artifact, so discovery never goes blank.
