# Plan: JustWatch "most popular" catalog rows in den-atlas

**Status:** ready to implement. **Owner:** den-atlas agent. **Author handoff:** scoped against the
current Rust/axum den-atlas (not the stale ticket, which assumed a CF-Worker den-scout).

## Why this lives in den-atlas (not den-scout)

den-scout is the **private** addon: debrid token in the URL, `/configure`, `configurationRequired`,
stream resolution. A public "trending on Netflix" catalog has the wrong posture there.

den-atlas is already the **public, tokenless discovery addon** (`resources:["dataset"]`, no
`/configure`, no token, `configurationRequired:false`, "facts not tokens"). JustWatch trending is
public discovery metadata → same posture. Adding it here also gives the **strongest isolation the
ticket demands**: a JustWatch outage is in a different service and cannot affect the stream addon at
all. den-atlas graduates from "dataset addon" to **Den's discovery addon**: `dataset` + `catalog`
resources side by side.

## Scope corrections vs. the original ticket (ADDON-01-catalog-consumer)

- **No CF-Worker / KV / D1.** den-atlas is a Rust/axum container. The cache is an in-process
  TTL map (see §4). "dataset.json resource" in the ticket referred to den-atlas's existing dataset
  descriptor, not a catalog store.
- **No TMDB. No `meta` resource.** We emit the **IMDb `tt` id** as the catalog item id, so Stremio's
  built-in **Cinemeta** supplies the detail page and other addons resolve streams. den-atlas already
  advertises `idPrefixes:["tt"]`. Grid posters come from **metahub** by IMDb id (Cinemeta's own
  poster source — consistent, reliable, no image-URL templating). This drops a large chunk of the
  original scope.
- **Do NOT touch** `dataset.rs` / `descriptor.rs` / the blob-serving paths. The catalog is purely
  additive; the dataset resource must keep working unchanged (isolation).

## 1. Dependencies (Cargo.toml)

Add an HTTP client for the GraphQL POST. Keep the minimal-binary ethos (`opt-level="z"`, rustls, no
OpenSSL):

```toml
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls", "gzip"] }
```

No cache crate — hand-roll a tiny TTL map (§4) to avoid pulling in `moka`. `tokio` is already present.

## 2. JustWatch source module — `src/justwatch.rs` (isolated)

- `POST https://apis.justwatch.com/graphql`, operation **`GetPopularTitles`**. Mirror the query in
  **rleroi/Stremio-Streaming-Catalogs-Addon** (`src/services/justwatch.js`) — field names matter.
  Reference shape:

  ```graphql
  query GetPopularTitles($country: Country!, $first: Int!,
      $popularTitlesSortBy: PopularTitlesSorting!, $packages: [String!], $objectTypes: [ObjectType!]) {
    popularTitles(country: $country, first: $first, sortBy: $popularTitlesSortBy,
        filter: { objectTypes: $objectTypes, packages: $packages }) {
      edges { node { objectType
        content(country: $country, language: "en") {
          title originalReleaseYear externalIds { imdbId } } } }
    }
  }
  ```

  Variables: `country` (ISO, from env `JW_COUNTRY`, default `"US"`), `first: 100`,
  `popularTitlesSortBy: "TRENDING"`, `objectTypes: ["MOVIE"]` or `["SHOW"]`, `packages:
  [<provider short-code>]`.
- **Parse** each `edges[].node.content`: take `externalIds.imdbId`, **validate `^tt\d+$`** (drop
  items without a valid IMDb id — they can't be resolved by other addons), keep `title`. Preserve
  list order = trending rank (needed for aggregation §5).
- Return `Vec<TrendingItem { imdb: String, title: String, rank: usize }>`.
- **Isolation / graceful degradation:** own `reqwest::Client`, own timeout (~8s), response body size
  cap, its own error type. Every failure returns `Err` that the handler turns into an **empty
  `metas` list (HTTP 200)** — never a 5xx, never a panic. Nothing here can touch the dataset path.
- **Testability:** define a `trait TrendingSource { async fn popular(&self, provider, ObjectType) ->
  Result<Vec<TrendingItem>> }` and implement it for the real JustWatch client. Tests inject a fake
  (mirrors den-scout's `doer` seam). Keep a saved JSON fixture under `tests/fixtures/` or
  `data/justwatch-sample.json` for the parse test.

## 3. Provider table + manifest (`src/manifest.rs`)

- Static provider table (code → catalog id → row name). Default set (short codes can drift; verify
  against JustWatch's provider list per country if a row comes back empty):

  | code | catalog id | row name |
  |---|---|---|
  | `nfx` | `jw-nfx` | Popular on Netflix |
  | `max` (a.k.a `hbm`) | `jw-max` | Popular on Max |
  | `amp` (Prime) | `jw-amp` | Popular on Prime Video |
  | `dnp` | `jw-dnp` | Popular on Disney+ |
  | `atp` | `jw-atp` | Popular on Apple TV+ |

  **Per-install config (`/configure`).** Region + provider set are chosen per install, carried in a
  `<region>_<codes>` URL path segment (Stremio config-URL pattern) — no server-side state:
  - `region` = `auto` (the Den app forwards the device country as a `country` catalog extra) or a
    fixed ISO code; `codes` = provider short-codes joined by `-`. Empty codes = no catalog rows.
  - `country` is per-request through `TrendingSource::popular(provider, ObjectType, country)`; the
    manifest is `configurable:true` and declares the `country` extra only when `region=auto`.
  - A config-less `…/manifest.json` install falls back to the **operator default**: env `JW_COUNTRY`
    (default `US`) + `JW_PROVIDERS` (default all). So existing installs are unaffected.
- Manifest changes: `resources: vec!["dataset", "catalog"]`; populate `catalogs` with one entry per
  provider × type **plus** the aggregated row:

  ```json
  { "type": "movie", "id": "jw-nfx",       "name": "Popular on Netflix" }
  { "type": "series","id": "jw-nfx",       "name": "Popular on Netflix" }
  ...
  { "type": "movie", "id": "jw-trending",  "name": "Trending Everywhere" }
  { "type": "series","id": "jw-trending",  "name": "Trending Everywhere" }
  ```

  Add a real `Catalog { type, id, name }` serde struct (replace `catalogs: vec![]`). `behavior_hints`
  already has `configurable:false, configuration_required:false` — leave as-is (perfect for a public
  catalog). No `extra`/genre/search initially (optionally support `skip` for pagination later).
- Manifest body changes → its fnv ETag changes automatically; additive `catalog` resource is safe for
  the Den app (it ignores what it doesn't use) and enables the rows in any Stremio client.

## 4. Cache — `src/cache.rs` (new, tiny)

- In-process TTL map: `Mutex<HashMap<String, (Instant, Arc<str>)>>` holding the serialized `metas`
  JSON per key. Key: `jw:{country}:{catalog_id}:{type}`.
- TTL ~**6h** (trending moves in hours). On a miss/expiry, fetch + store. **Serve-stale-on-error:**
  keep the last-good value and return it if a refresh fails (don't evict on error) — this is the
  latency + resilience win the ticket asks for.
- No `Instant::now()` concerns for tests: inject a clock or gate the TTL test behind a small
  `now: fn() -> Instant` seam if you unit-test expiry (optional).

## 5. Aggregation — "Trending Everywhere"

- For the requested type, fetch the configured provider set (reuse per-provider cached lists), dedupe
  by IMDb id, and **re-rank by inverse-rank-sum**: `score(title) = Σ over providers 1/(rank+1)` (a
  title trending on 3 services outranks one #1 on a single service). Sort desc, take top ~100.
- Cache under `jw:{country}:jw-trending:{type}`. This is the headline feature — no existing addon
  does cross-service aggregation.

## 6. Handler wiring (`src/handler.rs`)

- In the `handle` fallback, add a branch for `path` matching `/catalog/{type}/{id}.json`
  (and tolerate a trailing `/catalog/{type}/{id}/{extra}.json`). Parse `type` ∈ {`movie`→MOVIE,
  `series`→SHOW}; `id` ∈ provider ids or `jw-trending`. Unknown id/type → empty `metas` 200 (or 404
  for a truly unknown path, matching the existing style).
- Response body: `{ "metas": [ { "id": "tt…", "type": "movie|series", "name": "<title>",
  "poster": "https://images.metahub.space/poster/medium/tt…/img" } ] }`.
- Serve via the existing `serve_json`-style path so it gets an fnv **ETag + conditional 304**; set
  `Cache-Control: public, max-age=3600` (HTTP-layer; the 6h freshness is the in-process cache).
- Landing page: add a line noting the catalog rows + **JustWatch attribution** ("Data from
  JustWatch"). Honor attribution wherever offers/where-to-watch would be surfaced (we only use
  trending ranking, so a footer credit suffices).

## 7. Config / docs

- `.env.example`: `JW_COUNTRY` (default US), `JW_PROVIDERS` (default set), optional `JW_CACHE_TTL_SECS`
  (default 21600). No secrets — JustWatch's endpoint is unauthenticated.
- `DEPLOY.md`: note the new outbound dependency (apis.justwatch.com) and that a JW outage degrades to
  empty rows without affecting the dataset resource.
- README: den-atlas now serves `dataset` + `catalog`; add the row list + attribution.

## 8. Tests (keep den-atlas's bar; gofmt/clippy-clean, `cargo test` green)

- JustWatch parse from the saved fixture → correct imdb/title/rank; items without a valid `tt` id
  dropped.
- Aggregation: given per-provider fixtures, "Trending Everywhere" dedupes and inverse-rank orders
  correctly (a multi-service title floats above a single-service #1).
- Manifest: `catalog` in resources; one catalog per provider×type + the two trending entries.
- Handler: `/catalog/movie/jw-nfx.json` returns metas with `tt` ids + metahub posters and a 304 on
  If-None-Match; **degradation test** — source errors ⇒ 200 with empty `metas`, and the
  `/dataset.json` + blob routes are unaffected.
- Fake `TrendingSource` for all handler/aggregation tests (no network in tests).

## Acceptance

- `cargo build --release` clean; `cargo test` + `cargo clippy` clean; static binary in the existing
  distroless image.
- `/manifest.json` lists `catalog` + the catalogs; `/catalog/movie/jw-nfx.json` and
  `/catalog/movie/jw-trending.json` return IMDb-id metas that resolve via Cinemeta/other addons.
- JustWatch fetch is cached ~6h, serves stale on error, and a forced JW failure leaves `dataset`
  fully working. JustWatch attributed. No TMDB, no `meta` resource, no changes to den-scout.

## Non-goals (defer)

- `meta` resource / self-contained metadata (Cinemeta covers it).
- `extra` filters (genre/search) and `skip` pagination — add later if wanted.
- TMDB enrichment.
- Positional file handling, debrid, anything stream-related (that's den-scout).
