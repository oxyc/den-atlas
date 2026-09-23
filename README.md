# den-atlas

A self-hosted **dataset addon** for [Den](https://github.com/oxyc/den). It holds the shared **feature
store** — derived labels (genre / subgenre / mood) + quantized semantic vectors for the whole catalog —
and **answers queries against it**: similar titles, category and facet rows, search, the billboard.

The store stays here. The app used to download it (`labels-*.json`, `vectors-*.bin`, a metadata sidecar,
a premise index and a facet blob), verify each blob's sha256 and run the nearest-neighbour search
on-device; since #113 it asks instead, and atlas serves **no files at all** — only JSON answers.

```
Den (Apple TV) ──GET /manifest.json──►  atlas   { resources: ["dataset"] }
               ──GET /dataset.json───►          { datasetVersion, taxonomyVersion, embeddingModel,
                                                  dims, count, quantization, embed, queries }
               ──GET /index/similar/…─►         { ids }        ← ranked here, off the mmap'd store
               ──GET /index/query.json?q=─►     { parse, hits }
```

It implements the Den **`dataset`** resource (the Stremio superset — see the Den repo's
`tickets/EPIC-feature-provider-addon.md`, FP-1). A plain Stremio client ignores the unknown resource, so
installing Atlas there is harmless; only Den acts on it.

## Facts, not tokens
Atlas answers from **derived data** — labels, vectors, and the titles / poster paths / years the store
carries so a client draws a card without a per-result TMDB call. No overviews and no raw text, and
**nothing personal**. There is no per-user state and no token; `/configure` only picks the catalog region
and services, carried in plaintext in the install URL. Personalisation (your taste vector) never leaves
your device.

No file leaves atlas, so nothing it serves is a verbatim copy of an upstream artifact: every TMDB-derived
value it emits is assembled by a route and audited for prohibited prose at the last HTTP boundary
(`src/tos.rs`).

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

`datasetVersion`/`embeddingModel`/`taxonomyVersion` still say when the dataset moved — a client caching
answers keys on them, and a change of `embeddingModel` invalidates every query vector with it.

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
A small **Rust** (axum + tokio) server — a ~0.8 MB static musl binary, **~2–4 MB RSS** whatever the dataset
size. The store is **mmap'd**, never read into RAM; its header hash is verified once at load, so the row
count served as `count` is the store's own rather than a number the manifest claimed. (The original
TypeScript server is preserved at the `legacy-ts` git tag.)

## Caching
Every response is cache-friendly (`src/http.rs`): a strong `ETag` (the body's 64-bit FNV-1a plus its
length) honoring `If-None-Match` (→ `304`), plus `HEAD` and `Range` (`Accept-Ranges` / `206` / `416`). A
JSON answer ignores `Range` and is always sent whole, since the prose guard can only judge a whole body.
`dataset.json` carries no `Last-Modified`: its body moves with the embed/index flags under an unchanged
dataset date, so only its ETag can say it changed. Nothing varies on a request header. Sit a CDN in front
and it caches everything by URL with correct revalidation.

| Response | `Cache-Control` |
|---|---|
| `manifest.json` | `max-age=3600, stale-while-revalidate=600, stale-if-error=86400` |
| `dataset.json` | `max-age=300, stale-while-revalidate=3600, stale-if-error=86400` |
| `/index/…` GET | `max-age=3600, stale-while-revalidate=86400` (answers move only with the dataset); `max-age=300` when a search should have been ranked through den-embed and wasn't |
| title search | `max-age=3600, stale-while-revalidate=3600` |
| a JustWatch row | `max-age=3600, stale-while-revalidate=86400, stale-if-error=86400`; `max-age=60` for a stale or empty fallback |

A search query's vector is remembered in memory (1,000 texts, least recently used first, 24 h), keyed by the
dataset's embedding model and width, so a repeated search does not call den-embed again.

## Routes
| Route | Returns |
|---|---|
| `GET /`, `GET /configure` | landing/configure page: pick region + services, get the install URL |
| `GET /health` | always `200`: `{"status":"ok"}`, or `{"status":"degraded","reason":…,"detail":…}` with reason `dataset_unavailable`, `stale_catalog` (last JustWatch refresh failed), `catalog_schema_suspect` (a served chart came back mostly empty) or `facts_unusable` (the dataset declares a facts file the last index load couldn't read; `/recommend` and search run without facts) |
| `GET /manifest.json` | the `dataset` + `catalog` manifest (also under a `/<region>_<codes>/` install prefix) |
| `GET /dataset.json` | the descriptor — what the dataset IS (`datasetVersion`, `taxonomyVersion`, `embeddingModel`, `dims`, `count`, `quantization`, `signature`, the `embed`/`queries` capability flags). It names no files to download; `503` when the dataset did not load |
| `GET /catalog/<type>/<id>[/<extra>].json` | a "most popular" row of `{id,type,name,poster}` metas |
| `GET /catalog/<movie\|series>/den-titles/search=<q>.json` | with `TITLE_SEARCH` on: fuzzy, typo-tolerant title search, `{id:"tmdb:<id>",type,name,moviedb_id}` metas, best 30 |
| `GET /index/taxonomy.json` | with `INDEX_QUERIES` on: `{schema,taxonomyVersion,subgenres,moods}`, each list most-populated first. Label names only, kept for the TV app's browse rows; `schema` points at `/index/schema.json`, which describes the dataset |
| `GET /index/schema.json` | with `INDEX_QUERIES` on: the self-describing query fields, types, units and formats, value counts and per-field coverage; every count names both its known-field and full-corpus denominators (a series-only field, `broadcaster`, is out of the series). `semantics` says what each kind of count is out of and how a search applies a constraint (`filter`, `discount`, `boost`, `require`); `routes` lists every public route with its parameters, the field each reads, defaults, limits and a working example, so a client can use the API without reading the code |
| `GET /index/rows/<movie\|series>/<subgenre\|mood>/<label>.json?skip=&limit=` | with `INDEX_QUERIES` on: `{ids,total,coverage}` carrying the label, most confident first (≥ 0.55), 24 a page, at most 100; `coverage` names the full corpus, selected-type denominator and known-field population |
| `GET /index/similar/<movie\|series>/<tmdbId>.json` | with `INDEX_QUERIES` on: `{ids,total}` for More Like This — premise neighbours gated by animation, genre and plot agreement, else plot neighbours — and beside them `{mixed:[{type,id}],mixedTotal}`, the same row with films and series together (`skip`/`limit` page both) |
| `GET /index/neighbours/<movie\|series>/<tmdbId>.json?k=` | with `INDEX_QUERIES` on: `{ids}`, the plain plot neighbours (12 by default, at most 50) |
| `GET /index/search.json?q=&type=` | with `INDEX_QUERIES` on: semantic search in one request — the query embedded by den-embed, then `{titles:[{type,id}]}`, the 24 nearest; `503` without den-embed |
| `GET /index/facets.json?q=` | with `INDEX_QUERIES` on: the facet lane — `{facet,titles}`, titles matching the query's country/decade/type most-voted first, a leftover theme ranked to the front (best 50) |
| `GET /index/query.json?q=&type=&skip=&limit=` | with `INDEX_QUERIES` on: search in one request — `{parse,people:[{qid,id,name,credits}],hits:[{type,id,score,title,posterPath,year,genreIds,originalLanguage?,f}],total,semantics,coverage}`. `total` is the retrieved pool, not a corpus count, and `semantics` says so; `coverage` names every constraint the query applied, its value, how it was applied and how many titles have that field on record out of the titles of the type asked for (the corpus when none was). The query is read for a country, decade, type, genre, label, plot facet or person (the facts' credited people, by name or alias; `id` is the TMDB person id), and every candidate (fuzzy title under any of its names, facet, label, plot facet, a named person's titles, plot and premise vectors on the leftover) is scored `Φ·[2.0·title + w·max(plotSemantic,premiseSemantic) + 0.25·label + 0.10·plotFacet + 0.8·person + 0.15·popularity]`, so an exact title always outranks a theme match. Only candidates with a positive internal score are returned; wire scores are rounded to four decimals. 40 a page, at most 100 |
| `GET /index/row/<movie\|series>.json?<axis>=<value>…&mood=&subgenre=&skip=&limit=` (also `/index/plot/…`) | with `INDEX_QUERIES` on: a browse row from the store's facet axes (ending, era, chronology, pacing, tone, …) and the labels' moods and subgenres (≥ 0.55), alone or combined. A few thin values also answer as merged display rows — `ending=unresolved` (open, ambiguous, cyclical), `ending=unhappy` (tragic, bittersweet), `chronology=out-of-order` (nonlinear, framed, parallel-strands), listed in `/index/schema.json` under `fields.<axis>.merged`; similarity keeps the raw values — `{titles:[{type,id,title,posterPath,year,genreIds,originalLanguage?}],total,coverage}`, the titles carrying every constraint, most confident then most voted, 24 a page, at most 100. `coverage.fields` reports every filtered field against the selected movie/series population and also names the full corpus; a missing facet is unknown, never false |
| …and the same route's **taste tilt**: `&tilt.liked=m550,t1396&tilt.disliked=m176&tilt.era=<center>,<spread>&tilt.w.embedding=&tilt.w.dislike=&tilt.w.era=&tilt.w.square=` | the WHOLE row reordered for one household before the page is cut, so a title the plain order puts on page 3 can lead page 1. Ids are `m`/`t` + TMDB id, at most 500 a list; `tilt.era` is den-core's fitted curve and its presence is what turns the era term on (omit it for a row already fixed to an era). The four `tilt.w.*` levers are den-core's `tilt::Weights`, each independently overridable and defaulting to its shipped value. **Reorder only** — same `total`, same titles, so paging state stays valid; the answer adds `taste`, the fingerprint of the order the page is a slice of. A taste atlas cannot place returns the plain row, never an error. Memoised per (row × taste × weights); a request carrying `tilt.*` is `private`-cached rather than `public`, since its URL names the household's titles |
| `GET /index/filter/<movie\|series\|all>/counts.json?sel=…` | with `INDEX_QUERIES` on: stackable filters' counts (see **Filters** below) — `{total, kinds: {<kind>: {mode, complete, values: {<id>: n}, labels?, selected?, excluded?}}, coverage, ignored, kindsUnavailable?}`: for every value of every listed kind, the titles of the type (of both, under `all`) carrying the whole selection AND that value; zero counts left out, except a selected or excluded id, which always appears. Den Web hides an option at 0 in a `complete` kind |
| `GET /index/filter/<movie\|series\|all>/titles.json?sel=…&skip=&limit=` | with `INDEX_QUERIES` on: the titles carrying the selection, most voted first (under `all`, films and series merged by rank within type, each card naming its `type`; in similarity order when a `like` is selected), as the cards `/index/row` returns — `{titles, total, order, coverage, ignored, kindsUnavailable?}`; 24 a page, at most 100, `skip` a multiple of `limit`. `order` fingerprints the order the page is a slice of (it moves with the rating provider's votes, or is `like:<id>`) |
| `GET /index/filter/<movie\|series\|all>/values/<kind>.json?sel=…&q=&limit=` | with `INDEX_QUERIES` on: one kind's values under the selection, labelled, most titles first — `{kind, mode, values: [{id, name, count, tmdbId?}], complete, ignored, kindsUnavailable?}`, 10 at most; with `q` (2 characters or more), only those with a word starting it: the typeahead for people, studios, subjects and places, over names and aliases. `character` is search-only: `q` of 3 or more, a name starting it, 5 at most |
| `GET /index/filter/<movie\|series\|all>/people.json?sel=…&traits=…&skip=&limit=` | with `INDEX_QUERIES` on: the people credited on the titles carrying `sel`, holding every person trait in `traits` (see **People** below), most matching titles first, then most titles in the corpus, then Q-id — `{people: [{id, name, tmdbId?, credits, roles, gender?, born?, died?, citizenship?, occupation?}], total, labels, coverage, ignored, ignoredTraits?, unknownTraits?, …}`; paged as `titles.json`. `labels` names every trait id on the page |
| `GET /index/filter/<movie\|series\|all>/people/counts.json?sel=…&traits=…` | with `INDEX_QUERIES` on: for every value of every person trait, the people credited under `sel` and the other traits holding it — `{total, traits: {<trait>: {mode, complete, values, labels?, selected?, excluded?}}, traitCoverage, …}`, shaped as `counts.json`'s kinds |
| `POST /index/labels.json` | with `INDEX_QUERIES` on: `{titles:[{type,id}]}` → `{labels}`, each title's labels or null |
| `POST /index/score.json` | with `INDEX_QUERIES` on: `{space?,liked,disliked,candidates}` → `{space,scores:[{taste,dislike}]}`, cosine to each centroid, clamped at 0 |
| `POST /index/suggest.json` | with `INDEX_QUERIES` on: `{seeds (≤8),exclude?,limit?}` → `{perSeed:[{seed,ids,mixed}],pooled,pooledMixed}`, More Like This per seed and pooled in seed order; `mixed` and `pooledMixed` are the same with films and series together, `{type,id}` |
| `POST /recommend` | with `INDEX_QUERIES` on: `{surface?,service?,now?,services?,library,owned,hide?,candidates?,limit?}` → `{slides:[{type,id,imdbId?,why}],unjudged:[{type,id}],unjudgedCount,libraryUnjudged,facts,scorer,datasetVersion}`, the titles a featured surface leads with (40 by default); `no-store` |
| `GET /playground` | with `PLAYGROUND` and `INDEX_QUERIES` on: the tuning page. `/playground/params.json` lists every More Like This knob (`den_index::SimilarParams`) with its range and production value; `/playground/similar/<movie\|series>/<tmdbId>.json?<knob>=…&limit=` ranks with those overrides and returns each title's score, production rank and per-signal points. The ranges bound one request's cost (`pool_k ≤ 1000`, `max_row ≤ 400`, `limit ≤ 200`). Besides the knobs every playground route reads `watched=movie:…,series:…` (dropped from each row, and each row ordered by den-core's tilt with them as liked) and `filter.<axis>=<value>` (only candidates carrying it, as `/index/plot` reads constraints). `/playground/rows.json?seeds=movie:5723,series:1438&suggest=…&<knob>=…&limit=` answers the same for up to 12 seeds, at most 50 titles each, plus You Might Also Like (`/index/suggest`'s pooling over tuned rows) for up to 6 `suggest` seeds, in one request (no `seeds`/`suggest`: the page's defaults; empty: none), and with `judged=1` also `judged`, the `/playground/judged.json` answer for the same knobs — what the page asks on each change. `/playground/judged.json?<knob>=…` scores those knobs against `judged/rail.json` (nDCG@10 and companions, `den_index::eval`), dev and test halves apart, beside production — one tuned row per judged seed. `/playground/export.json?<state>&full=&judged=` writes the page's state and a read-only snapshot as a versioned file (the shape is documented at `playground::FILE_FORMAT`); `POST /playground/import.json` reads one back. `/playground/titles.json?q=` is the page's title autocomplete: the `TITLE_SEARCH` fuzzy index over films and series in one answer, kept to titles the store has (`{titles:[{key,title,year}]}`, at most 8); a `404` without the title index, when the page searches with `/index/query.json` instead. An unknown knob or out-of-range value is a `400` naming it; `no-store`; off ⇒ `404` |
| `POST /embed` | a search query (`{"text":…}`) embedded by den-embed; `503` when `EMBED_URL` is unset |
| `GET /metrics` | Prometheus text for `Authorization: Bearer $METRICS_TOKEN`; `404` when the token is unset or wrong |

Every response carries `Access-Control-Allow-Origin: *` and `OPTIONS` answers the CORS preflight. An
unknown path — and a refused `/metrics` — is a `404` `{"error":"not_found"}` with `cache-control: no-store`.

Catalog rows, `/embed` and the `/index/…` answers carry `Server-Timing`: `justwatch;dur=<ms>` when the row
was fetched upstream or `cache;desc=hit` when it came from the cache (plus `cache;desc=stale` when the
last-good copy was served), `embed;dur=<ms>` for the den-embed call, and `total;dur=<ms>`. An answer that
is stale or a fallback carries `X-Den-Degraded: <reason>` with /health's reason slug — a catalog row
served after a failed JustWatch refresh says `stale_catalog`. Normal answers carry no `X-Den-Degraded`.

`POST /embed` forwards the query to the internal [den-embed](https://github.com/oxyc/den-embed) service and
returns its int8 vector (`{"vector":…,"dims":1024,"model":"bge-m3"}`), so a query embeds through the SAME
bge-m3 + quantizer that built the corpus and the two are comparable. den-embed stays internal — only Atlas
is exposed.

Title search (`TITLE_SEARCH`) builds an in-memory index of the 100k most popular movies and series in
TMDB's daily ID exports — downloaded once a day, gunzipped straight into the scanner, nothing written to
disk — and ranks by the share of the query's trigrams a title contains, blended with popularity, as the Den
TV app's on-device index does. The index lives in the `den-titlesearch` crate (`crates/`), which has no
async runtime, IO or global state, so it also compiles for Wasm and tvOS. Until the first build lands a
search answers empty with `X-Den-Degraded: title_index_building`. The request log shows the query as
`<query>`.

Index queries (`INDEX_QUERIES`) answer from the dataset's plot and premise indexes through the
`den-index` crate (`crates/`, portable like `den-titlesearch`), the same way the Den TV app's on-device
index does. They return TMDB ids only; clients hydrate titles themselves. The indexes load on the first
query — the answer's `Server-Timing` carries `load;dur=<ms>` then — and are released after 10 idle minutes,
so an unused atlas holds none of their ~80 MB. The descriptor carries `"queries":true` when they're on.
Semantic search and the facet lane's theme ranking embed the query through den-embed (`EMBED_URL`). Unified
query search scans both plot and premise vectors when the latter exist, normalises each scan against its own
distribution, and lets the stronger one spend the single semantic weight; a missing premise index preserves
plot-only behaviour. The facet lane reads the store's country, language, year and vote columns — it used to
read a `facets.bin` sidecar, which covered 9,086 fewer titles. Taste weights stay with the client:
`score` returns the raw boosts.

Vote counts — what every browse row is ORDERED by — are TMDB's `vote_count`, with its `vote_average` the score
`/recommend` rates a title with where no upstream list scored it, and TMDB's credits give the list of titles
that share a character. None of it is in the published dataset: atlas asks den-edge's TMDB proxy for it
(`TMDB_PROXY`) and keeps it in `CACHE_DIR` on the box — vote counts in bulk from `/discover`, sliced by date,
about once a month; credits a share of the corpus a day, and at once for a title TMDB's changes feed names.
Every kept value is dropped six months after TMDB gave it. `src/tmdb.rs` opens with the rules on what these
numbers may be used for: sort keys, filters, floors, merit and popularity terms and character-link evidence at
runtime — never an embedding, a model or a file that leaves the box. A first boot reads a seed made from
den-dataset's own TMDB cache (`scripts/tmdb-seed.py`). The store's own `votes` column, where an older store
carries one, is the fallback for a row nothing is kept for — stated in the load line (`row order: …`) rather
than left to be inferred. When NEITHER source has a count, `/health` reports `votes_unusable`: rows still come
back full, in tmdb-id order, which is the one degradation here that looks like a working addon.

This product uses the TMDB API but is not endorsed or certified by TMDB.

### Filters

`/index/filter/<type>/…` is Den Web's stackable Search: a selection of values, AND-ed, and three questions
about it (counts, titles, one kind's values). `/index/schema.json`'s `filter` object lists every kind with its
`mode`, id format, listing and floors, the axis aliases and the canonical rules; `tests/fixtures/facets-canonical.json`
pins url → canonical url pairs a client can test against.

- **`sel`** is `[-]<kind>:<id>` items joined by `,`. Kinds: `genre` (TMDB id; a series also under 10759/10765/10768),
  `language`, `country` (every one a title lists), `region` (one pick: any of its countries; the table is
  `crates/den-index/src/regions.rs`, published as the schema's `regions`), `decade` (first year; a series' first air date), `mood`,
  `subgenre`, `primary` (the labels' primary genre), `animated` (`yes`/`no`), `runtime` (`under-90`, `90-120`,
  `120-150`, `over-150`; a series per episode), `source` (adapted from), `rating` (the rating provider's average ≥ 6,
  7 or 8 out of 10, on 10+ votes), `technique` (≥ 0.4), `audience` (≥ 0.5), `critique` (≥ 0.6), `warning` (depicts, ≥ 0.5), the
  plot axes known for enough of the titles people browse — `era` and `ensemble` everywhere, `setting` and
  `chronology` for films, `continuity` for series (`offeredFor` in the schema; the other seven are rows only,
  measured in oxyc/den-atlas#35; `structure` resolves by value; the merged rows count their members' union), the entity kinds
  `person` (anyone credited), `made`, `cast`, `company`, `network` (series), `subject`, `place` and `format`
  (Wikidata Q-ids; top 30 listed, studios, subjects, places and formats from 5 titles, formats from a curated
  list), `character` (role names played in 2+ titles, from the character provider; search-only) and `like:<tmdbId>` (the set
  `/index/similar` answers).
- **`all`** asks every question of films and series together: counts and values over both; titles are the two
  types' own orders merged by rank within type (position in its type's order ÷ that type's size, ascending,
  films first on a tie), so series run at their share of the corpus, evenly spread, and the most popular series
  sit beside the most popular films. A selection merges the filtered lists by the same keys. Every kind answers there; one only a type has (`network`, a series-only genre such as 10762 kids, a
  composite) matches only that type's titles, and a film genre id matches the series filed under it too
  (`genre:28` is action films and Action & Adventure series). `like` names its title's type —
  `like:movie-550`, `like:series-1396` — and answers `/index/similar`'s `mixed` row; a bare id is a `400` there.
- **Mode.** `and` kinds hold several values a title (two genres are both genres) and count under the whole
  selection. `single` kinds hold one (decade, runtime, rating, primary, animated, the plot axes, like) and count
  each value under the selection WITHOUT the kind's own pick, so the other decades read as alternatives.
- **Exclude** with `-kind:id`: the titles known for the kind (something on record for it) and not carrying the
  value. `coverage` in every answer says how many titles of the type each applied kind is known for.
- **Canonical form**: items normalised per kind, sorted by kind, then positive before excluded, then id, each
  once, ids encoded as `encodeURIComponent` does, `:` `,` `-` literal; then `skip`/`limit` (titles) or `q`/`limit`
  (values), each only when not its default. The canonical URL is public for an hour and revalidates by ETag. Any
  other spelling is ANSWERED, not redirected (den-edge's relay drops `Location`): the same body, `private,
  max-age=60`, with `Content-Location` naming the canonical URL. At most 16 values and a 2,048-byte query;
  a malformed item, an id its kind cannot read, a `skip` off a page boundary or a short prefix is a `400`.
- **Unknown, unavailable, not offered.** A kind atlas does not know, or cannot answer now, is left out of the
  result and named in `ignored`; a value its kind does not hold (a typo, a label in the wrong case) matches
  nothing and is named in `unknownValues`. `kindsUnavailable` lists the kinds this atlas should answer and
  cannot through a failure at runtime — the facts or facet rows did not load, the rating provider has no votes
  yet — and such an answer carries `X-Den-Degraded: filter_kinds_unavailable` and is kept five minutes. A kind
  the loaded dataset has no data for — a section the store does not carry, a score table no title reaches the
  floor of — is simply not offered: absent from `kinds`, not unavailable, cached as usual. On 5b1c3213b6a1
  `warning` is not offered (its `depicts` scores are title-only, the highest 0.28); a store with article-based
  scores offers it on load. `character` is offered once its provider has built the names.
- **Providers.** `rating` and the vote order read whichever provider fills atlas's ratings holder, and
  `character` its character links: TMDB's, kept on the box (`src/tmdb.rs`), as a filter and a sort only.
- **Cost** on that store: 509 values in bitsets plus posting lists for the entity kinds, 11 MB, built at load in
  ~0.1 s; the name index for `values/…?q=` is built on the first search, 7.7 MB. The empty selection's counts
  are kept (0.1 ms); a three-kind selection answers in under 1 ms, the broadest single genre in ~6 ms, a titles
  page in ~0.1 ms, a name search in ~7 ms, and a page of people or cast without a prefix in 2–7 ms (only the
  values that can reach the page are named).
- **People** (`people.json`, `people/counts.json`; `src/filter/people.rs`) answer who is credited on the titles
  `sel` matches. `traits` is a second list in `sel`'s grammar and canonical order, placed after it, because a
  trait is about a person and means nothing to the title routes. Kinds: `gender`, `citizenship`, `occupation`
  (Wikidata Q-ids of the items the store names: P21 with every value it holds, P27, P106), `born` (decade of
  P569; a century-precision birth has none) and `role` (`cast`, `director`, `writer`, `creator`: the credit on
  a matching title; two roles mean both on one title, `-role:cast` credited without it). `gender` and `born` are
  one pick and count without their own pick. Unknown is never a match: a person with no gender on record matches
  neither `gender:` nor `-gender:`, and `traitCoverage` says how many credited people each applied trait is on
  record for. A store without the trait sections answers credits and roles and names the rest in `ignoredTraits`.
  Nothing is built at load: each answer walks the matching titles' credit lists in place. On b2c60751c955 (130,116
  people credited), male cast of 2020 films answers in ~0.8 ms warm (~39 ms the first time, mapping the pages
  in), every person unfiltered in ~4.4 ms, and the unfiltered trait counts in ~11 ms.

`total` on search is the number of retrieved candidates, not a corpus aggregate. Clients that present corpus
counts must use the field coverage and denominators from `/index/schema.json`; browse rows and search include
the relevant coverage inline. Group-by is advertised as unavailable until it can preserve that contract. Every
response that is or may be JSON (any `+json` type, or none) is checked at the final HTTP boundary for
expressive prose fields, and since nothing is served verbatim any more, that boundary is the whole guard. Only
`/manifest.json` and `/dataset.json` are exempt, by exact path: both carry atlas's own `description`. A future
MCP/TMDB hydration route therefore fails closed wherever it is mounted, instead of re-serving an overview,
synopsis, description, tagline, or equivalent prose.

`POST /recommend` ranks what a featured surface leads with — the Den web app's billboard — so no client
ranks. It is the web app's `billboard.ts` ported: what is new in the world and new to this library, with
attention (a place in Trending Everywhere, the household's "new on" lists, and the client's own lists) and
quality, multiplied by the library's taste and discounted where the library's own More Like This already
reaches. It describes each title from what atlas holds, all of it out of the one store: the labels, the
country/language/year/vote columns, and the Wikidata facts; a candidate's `hint` (release
date, genres, countries, popularity, rating and votes) fills only what those leave unknown. Rating hints affect only
that response: Atlas never writes them to catalogs, its dataset or replay fixtures. Existing upstream scores,
including JustWatch's IMDb scores, remain in Atlas's own catalog output. `unjudged` names the 20
titles atlas knows nothing about that are most worth describing; a client that can describe them asks again
with their hints, since an undescribed title is dropped once enough are judged. `library` is
`[{type,id,weight,at,hint?}]`,
`owned` every title the library holds, which never appears; `hide` is the household's rules
(`minYear`, `genres`, `languages`, `anime`). Each slide's `why` gives its terms. Its `reason` is the strongest
meaningful contribution on the scorer's common scale: `similar`, `profile`, `people`, `franchise`, `arrived`,
`recent`, `upcoming`, `timely`, `quality` or `buzz` (or `null` when none is strong enough). Clients can turn that
stable code into short copy. The raw `score`, `fit`, `similar`, `profile`, `people`, `confidence`, `fresh`, `arrived`,
`quality` and `buzz` terms remain alongside it for diagnostics and richer future explanations; they are deliberately
not directly comparable with one another. `Server-Timing` carries `lists;dur=` and `rank;dur=`. Bodies over 512 KiB
are refused.

`surface` is `home` (both types, the default), `movies` or `series`. `services` is the household's
`[{id,country?}]`: `id` a provider id from the catalogs' `denProviderIds`, `country` ISO 3166-1 alpha-2, else
the install's country; none picked, every service the install carries. A service channel (one service's page)
adds `service: {id,country?}` in the same shape, and its slides are then only titles atlas can show are on
that service in that country. Atlas holds no per-title availability, so that means titles one of its lists
for that service named — "new on" and "popular on" (JustWatch, filtered by that country's package), the
service's Movie of the Night Top 10 and additions, and Trending Everywhere read over that one service, i.e.
its own trending chart — plus the client's `candidates`, which a channel sends from its own rows and which
are trusted as the service's catalogue. The personal pool's whole-catalogue titles, other services' lists
and Netflix's US Top 10 don't reach a channel. `surface` still filters the types, and taste, `owned`, `hide`
and scoring are unchanged. A `service` whose id the install doesn't carry reads no lists and ranks the
client's `candidates` alone. The response is the same shape.

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
| `DATA_DIR` | `data` (the image sets `/app/data`) | directory holding `dataset.meta.json` and the store it declares |
| `JW_COUNTRY` | `US` | catalog country when an `auto` install forwards none |
| `JW_PROVIDERS` | all | provider subset for an install with no `<region>_<codes>` segment |
| `JW_CACHE_TTL_SECS` | `21600` | in-process freshness of the catalog rows |
| `CACHE_DIR` | unset | a writable directory the catalog rows are also kept in, so a restart serves them instead of asking JustWatch again; written only when a row is refreshed. TMDB's kept numbers and credits live here too (`tmdb-votes.tsv`, `tmdb-credits.tsv`, `tmdb-sweep.tsv`), and nowhere else. Unset ⇒ memory only |
| `EMBED_URL` | unset | den-embed base URL for `POST /embed`; unset ⇒ `/embed` answers `503` |
| `INDEX_QUERIES` | off | `1` turns on the `/index/…` routes (taxonomy, label rows, More Like This, neighbours, semantic and facet search, filters, labels, taste scores, suggestions); the indexes load on first use and are released after 10 idle minutes |
| `TMDB_PROXY` | unset | den-edge's TMDB proxy base (e.g. `http://den-edge:8080/tmdb`), which atlas asks for TMDB's vote counts, scores and credits (see above); atlas holds no TMDB key. Only alongside `INDEX_QUERIES`. Unset ⇒ atlas serves what `CACHE_DIR` keeps and asks for nothing |
| `TMDB_DAILY_MAX` | `1500` | questions atlas asks the proxy in a UTC day, two seconds apart. A full vote sweep is ~4,800, so it spreads over a few days; den-edge's own ceiling (`TMDB_DAILY_MAX` there) is shared with the web app's guests and must leave room for this |
| `MOTN_KEY` | unset | a Movie of the Night (Streaming Availability API) key. Each service's own daily Top 10 and what was added to it, per country, then lead the "Popular on" and "New on" rows and count as attention on `/recommend`; Netflix's US Top 10 reaches every billboard. Each such service also gets "Leaving <service> Soon" and "Coming to <service>" rows (the next 30 days, read every 3 days). Fetched in the background at most once a day for the markets requests ask for, 30 requests a day at most (the free plan allows 1,000 a month), and kept in `CACHE_DIR`. A country the API lacks is read as a neighbour (Uruguay as Argentina). Unset ⇒ JustWatch alone |
| `RECOMMEND_FIXTURES` | unset | a writable directory each `POST /recommend` is kept in as `<surface>.json` (a service channel's as `<surface>-service-<id>[-<country>].json`): the body as sent (the household's library included), atlas's lists for it and the moment it was ranked. `den-atlas replay <file>` ranks one again against `DATA_DIR` with that binary's scoring and prints every slide with why. Unset ⇒ nothing kept |
| `TITLE_SEARCH` | off | `1` builds the daily title-search index and declares the `den-titles` search catalog. Off by default: the Den TV app fuses every addon search catalog into its text search |
| `METRICS_TOKEN` | unset | bearer token for `GET /metrics`; unset or empty ⇒ `404` |
| `PLAYGROUND` | off | `1` turns on the `/playground` tuning routes (only alongside `INDEX_QUERIES`); computed per request, so enable it only where anyone reaching atlas may spend that. `/index/similar` never reads an override |
| `LOG_REQUESTS` | off | `1` writes one stderr line per request, `<METHOD> <path> <status> <ms>ms`, with a config segment shown as `<config>` and the query dropped |

## Run
```sh
scripts/fetch-dataset.sh   # prep ./data from the den-dataset `data-latest` release (labels + vectors + gzip + meta)
cargo run                  # http://localhost:8080  (add /manifest.json in Den → Plugins)
cargo test                 # the caching layer (ETag / Range / gzip / 304), routes, catalog, shutdown
cargo fmt --all --check    # CI gates on this — see below if the command is missing
scripts/release.sh 0.37.0  # checks, bumps, tags, and WAITS for the image
```
**If `cargo fmt` reports "no such command"**, the toolchain has no rustfmt component and there is no rustup
to add one. Run the formatter through nix instead — it reads this repo's `rustfmt.toml` and matches CI
exactly:
```sh
nix run nixpkgs#rustfmt -- --edition 2021 --check $(git ls-files '*.rs')
```
Worth doing before every push. Two releases in a row built nothing because a wrapped expression was left in
a shape rustfmt disagreed with: `docker-publish` never produced an image, and `den-update` then correctly
reported the box was already at the previous digest — a failure that reads as "nothing to deploy" rather
than as a broken build. Checking line widths by hand does not substitute: rustfmt also JOINS short wrapped
lines and SPLITS long array literals, neither of which a width check can predict.
`fetch-dataset.sh` is anonymous (needs curl, python3 and shasum). It downloads every file
`dataset.meta.json` declares and verifies each against the meta's sha256 before moving them into `./data`.
`storeFile` is the one it REFUSES a release without: the store is the only artifact the server reads, and
a manifest that still names the retired sidecars simply fetches files nothing opens. To pick up a new
release, re-run it and restart the server.

Refreshes stage on the destination filesystem and replace files by rename. A load that overlaps one is
refused rather than binding the new files to a descriptor read before them: `Dataset::load` stats
`dataset.meta.json` on both sides of its own read and compares size, modification time and Unix file
identity. The deployment sync stops Atlas after all downloads verify, replaces the data, then restarts
it. An interrupted replacement stays stopped with a recovery marker and no live descriptor until the next
successful refresh.

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

**Release images.** `docker-publish` builds on a `v*` tag, and again every Monday: the weekly run rebuilds
the newest `v*` tag (never `main`) with the base images re-pulled and no build cache, and publishes it as
`:X.Y.Z-patch.<date>.<run>` and `:latest`, so a toolchain or musl fix reaches the box between releases
through `den-update`'s probe and rollback like any release. Trivy scans each image before `:latest` moves:
a CRITICAL with a fix available fails the run (on the weekly rebuild only in OS packages, the part a
rebuild can fix), and fixable HIGH and CRITICAL findings go to code scanning. A finding that does not
apply goes in `.trivyignore` with a reason. Every image carries SLSA provenance and an SBOM and is signed
keylessly with cosign; verify a digest with:
```sh
cosign verify \
  --certificate-identity-regexp '^https://github\.com/oxyc/den-atlas/\.github/workflows/docker-publish\.yml@refs/(heads/main|tags/v[0-9]+\.[0-9]+\.[0-9]+)$' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  ghcr.io/oxyc/den-atlas@sha256:<digest>
```

Smoke test:
```sh
curl -s localhost:8081/health                        # {"status":"ok"}, or "degraded" with a reason
curl -s localhost:8081/manifest.json | jq .resources # ["dataset","catalog"]
curl -s localhost:8081/dataset.json | jq '.count, .embeddingModel, .dims'
curl -s localhost:8081/catalog/movie/jw-nfx.json | jq '.metas | length'  # live JustWatch (needs egress)
curl -s -H "Authorization: Bearer $METRICS_TOKEN" localhost:8081/metrics  # only with METRICS_TOKEN set
```

To install: Den → Settings → Plugins → add `http://<den-ip>:8081/manifest.json` (the app needs https or a
LAN/private-range host over http). Den's discovery then answers from here.
