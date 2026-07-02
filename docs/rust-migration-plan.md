# Plan (v2, post-audit) — Rust serving + extracting the dataset producer from the TV app

> **v2 changes** (an audit verified v1 against both repos and found several "clean move" claims false):
> - **Server → Rust** (confirmed sound; memory claims hold).
> - **Producer → standalone Swift package, NO DenKit import** — coupled to the app only by the published
>   artifact format, exactly like the server. Keeps the calibrated code (moved, not ported) + byte-parity;
>   owns small self-contained copies of the static genre tables + a thin TMDB client + its own format structs.
> - Corrected the real phase/checkpoint/vote contract, the e01-vs-e02 gap, the `import-dataset.mjs` work, and
>   the (over-stated) TMDB decoupling. Effort re-baselined to ~1.5–2 weeks.

Two goals: (1) rewrite the always-on serving layer in Rust for minimal memory + CPU; (2) get the dataset
"ranking/taxonomy" producer out of the Den tvOS app repo. These are independent — do the server first.

**Constraints from the owner:**
- The existing `t01`/`e02` labels+vectors are NOT rebuilt — den-atlas keeps serving today's bytes. The
  producer is *relocated*, only **minimally tested** (a handful of titles), never run at 60k scale.
- **Classification stays agent-driven** — labeling runs on the owner's **Claude Code** credit (Opus
  orchestrates Haiku subagents that write vote JSON files); the scripts need only `TMDB_KEY`, no server-side
  Anthropic key.
- **Don't hard-couple TMDB** — a future taxonomy may enrich from Wikipedia/Wikidata (see the honest scope in
  Part B; it's more than a trait swap).

---

## Part A — `atlas-serve` (Rust): the memory/CPU rewrite

Today (Node): loads labels (11.19 MB) + vectors (22.22 MB) into the heap, plus a boot-time `gzipSync` and a
boot-time `sha256` → **~33 MB RSS**, 196 MB image. Rust target: **< 10 MB RSS, ~5 ms boot, ~10 MB image**,
identical behaviour. (Audit confirmed the memory math and that these targets are plausible.)

**Stack:** `axum` (tokio/hyper). Serve blob bodies via **`tokio::fs` streaming** (`ReaderStream`, 64 KiB
chunks; Range = stream just the window). **Avoid `mmap` for the hot path** — a republish that overwrites a
file in place under a live mmap causes SIGBUS/torn reads (the audit flagged this; Node is immune only because
it loads into the heap at boot). Streaming from a freshly-`open`ed fd per request sidesteps it; if mmap is
ever wanted for speed, pair it with atomic-rename-and-reopen.

**Where the wins come from:**
- **Never hold a blob in RAM** — stream from disk. RSS stays flat under load.
- **Precomputed gzip file** (`labels-t01.json.gz`, written by the producer) served for gzip clients — no
  runtime compression, no in-RAM gzip.
- **Sha from the sidecar** — the producer writes per-blob sha256 into `dataset.meta.json` (NEW: today's meta
  has no sha; the Node server hashes at boot, `dataset.ts:52`). Server just reads it → instant boot.

**`tower-http` `ServeFile`/`ServeDir` is NOT enough** — it won't reproduce the sha `ETag`, the distinct
`"<sha>-gzip"` ETag, `Vary`-only-when-gzip, `?v` immutable-vs-revalidate, or multi-range/garbage→`200`. So
**hand-roll `serve_bytes` over `tokio::fs`**. Only the *validator/negotiation* logic ports from `http.ts` (137
lines): this is a **body-layer rewrite** (today it's all in-memory `Uint8Array.subarray`), not a "verbatim
port".

**Port checklist — every behaviour, incl. the ones v1 missed** (verified against `handler.ts`/`http.ts`/
`util.ts`/`descriptor.ts`):
- `ETag` (blob sha / JSON fnv) + `If-None-Match`; `Last-Modified` + `If-Modified-Since` → 304 (INM precedence).
- `HEAD`; **`405` for non-GET/HEAD**; `/configure` + `/configure/` alias to the landing page.
- `Range` → 206 `Content-Range` / 416; `Accept-Ranges`; **multi-range or garbage Range → full 200**.
- gzip negotiation (`Accept-Encoding`, incl. `*` and `q=0`); **`Vary` only when a gzip variant exists**; the
  **distinct `"<sha>-gzip"` ETag** for the gzip representation.
- Origin from **`X-Forwarded-Proto` + `X-Forwarded-Host`/`Host`** (v1 omitted `x-forwarded-host`,
  `util.ts:37-38`); `PUBLIC_BASE_URL` override.
- `?v=<datasetVersion>` → `immutable, max-age=1y`; bare path → revalidate. Distinct TTLs: `manifest.json`
  `max-age=3600`; `dataset.json` `max-age=300` + ETag + Last-Modified (`handler.ts:44-48`).
- The Cloudflare `worker.ts` reference entry is dropped by a native binary (acceptable — it's not the CI path).

**Image / CI:** multi-stage `rust:alpine` musl → `FROM scratch`/`distroless-static`. Rework `Dockerfile`,
`publish.yml` (currently node/npm), and drop `package.json`/`vitest`. Data still mounted. Port the vitest
cases to `#[tokio::test]`. Cutover: tag `legacy-ts`, then delete `src/*.ts`.

---

## Part B — the producer: **standalone Swift, NO DenKit import**

Keep it Swift (the calibrated classifier + FNV + int8 quantizer are the same code — re-deriving them in
another language is the real cost/risk, for a batch job where speed is irrelevant). But — per the owner —
`den-dataset` must **not import DenKit**. Depending on the app's *internal library* was the wrong boundary;
the correct one is the one the **server already uses**: den-atlas agrees with the app on a **published
artifact format**, not shared code. The producer becomes a **self-contained SwiftPM package**, symmetric with
the server, coupled to the app by exactly one thing — the format.

**Moves in as-is (relocated, deleted from the app — these are producer-only in DenKit today):**
`tools/taxonomy-backfill/`, `TaxonomyClassifier`, `Taxonomy` + `TaxonomyScorer`, `Embedder` (FNV + Quantizer),
`BackfillPipeline`, the producer `BackfillModels` types (`WorklistEntry`/`EnrichedTitle`/`RunReport`/
`Checkpoint`/`Worklist`), and the `finalize`/serialization code. This ~1,300 LOC just changes folder — same
Swift → **byte-parity stays free** (same `JSONEncoder(.sortedKeys)`/FNV/quantizer), no re-derivation.

**Owns small self-contained copies** instead of importing DenKit — because these are *reference data / a wire
schema*, not business logic (the owner's point: `GenreCatalog` is just TMDB's list):
- `GenreCatalog` (TMDB's static id↔name — a ~20-line copy) and `GenreRarity` (static IDF priors) — copy.
- the **grounding keyword-id map** — extract it into a producer-owned taxonomy table instead of deriving it
  from the app's `RecipeCatalog`. This *also fixes the one real smell*: the classifier stops reaching into a
  UI-discovery construct. The app's `RecipeCatalog` is untouched (it keeps its own keyword ids for queries).
- a **thin TMDB client** (`/discover`, `/{movie,tv}/{id}?append_to_response=keywords`, daily-export parse) —
  the producer shouldn't borrow the app's client.
- its **own copy of the format structs** (`IndexRecord`/`LabelsArtifact`/…). The app keeps its copy in DenKit;
  the two agree via the **format spec + a conformance fixture** (below), exactly like the server does today.

Net: `den-dataset` is fully standalone. **den-atlas (server + producer) and the Den app share one thing — the
published artifact format — and no code.** The only duplication is static reference data + a small wire schema,
which is the *correct* price for a defined-contract boundary; the calibrated logic is *moved*, not copied.

**The real tool** (corrected from v1 — verified against `main.swift`). Commands are
`worklist | enrich | enrich-ids | escalation | assemble | finalize | score` — not v1's 5:
- **Embedding + int8 quantization happen INSIDE `assemble`** (`main.swift:283`), not a separate `embed` phase.
- **`escalation`** is the adaptive self-consistency pass: after pass 1 it emits only the subset needing n=3
  (primary ∈ {Drama,Comedy,Thriller}, or top-subgenre confidence < 0.65; `main.swift:219-247`). This *is* the
  n=3 mechanism — v1 described n=3 as a classifier property but omitted the phase that selects who gets it.
- **Two forward-compatible checkpoints** (`enrich-checkpoint.json`, `classify-checkpoint.json`) using
  `decodeIfPresent` so a new field doesn't silently trigger a full re-run (`main.swift:597-605`).
- **Append-only index store** (`index/labels.jsonl`+`vectors.jsonl` via `LineAppender`); a `--force` re-pass
  writes superseding records; **`finalize` de-dupes by `(mediaType,tmdbId)` keeping the LAST**, vectors kept
  aligned (`main.swift:271-312`).
- **Lenient `HaikuVote` decoding**: a label may be a bare string or an object; missing confidence defaults to
  **0.7** (`main.swift:558-576`) so one off-schema item can't spuriously escalate a title. Test fixtures must
  include a malformed vote, not just clean ones.

**Two things v1 hand-waved that must be built (regardless of language):**
- **e01 vs e02.** `finalize` hard-codes `vectors-e01.bin` (`main.swift:321`) but the shipped/served artifact
  is `vectors-e02.bin`. Today's e02 was NOT produced by the committed `finalize`. **Reconcile how e02 was
  actually made** before claiming "relocate today's bytes" — likely a small `finalize`/embedder-version fix.
- **`datasetVersion` + meta.** `import-dataset.mjs` content-addresses `datasetVersion =
  sha256(labelsSha:vectorsSha)[..12]` and validates header/count alignment (`import-dataset.mjs:42-56`). That
  derivation + writing per-blob sha256 into `dataset.meta.json` + emitting `labels-t01.json.gz` must move into
  the producer's `finalize` (the server now reads sha from meta and `?v` depends on `datasetVersion`). This
  replaces `import-dataset.mjs`.

**TMDB decoupling — honest scope (downgraded from v1).** The `EnrichmentSource` trait cleanly abstracts the
*text* (title/overview/year), but the *signals* are TMDB-bound and live in the classifier + pipeline, not just
enrichment:
- **Grounding** matches TMDB **keyword integer ids** inside the classifier (`TaxonomyClassifier.swift:185-201`)
  — a Wikipedia source has none, so the grounding bonus silently no-ops.
- **`animated`** = `genreIDs.contains(16)`; **anime filter** = keyword `210024`/genre 16 + `ja`; **vote-floor**
  = `voteCount` (`main.swift:154,284,455`).
So: build the `EnrichmentSource` seam now for the *text*, and be explicit that a real Wikipedia source is a
**future project** needing (a) a Wikidata→tmdbId resolver (identity stays tmdbId) and (b) grounding/animated
fallbacks — not a drop-in. Don't over-invest in the abstraction now; just don't bake TMDB assumptions into new
code paths.

---

## Part C — Den app cleanup (surgical, not "delete files")

v1 said "delete producer-only files, 0 App references." Two are entangled into runtime files that STAY:
- `EnrichedTitle` + `Classification` + the `classificationRecord` helper live in `Sources/DenKit/TMDB/
  TMDBClient.swift:87` + `TMDBWire.swift:309-318` (`toEnrichedTitle`). Those *files* stay (runtime browsing),
  but the classification-only members are producer-only — so cleanup **surgically deletes those members** from
  DenKit (first verify no `App/` caller). The standalone producer re-derives them in its own thin TMDB client;
  nothing is "moved to shared."

**Re-homed in `den-dataset`** (deleted from `den/`): `tools/taxonomy-backfill/`, `Backfill/{TaxonomyClassifier,
Embedder,BackfillPipeline}.swift`, `Taxonomy/{Taxonomy,TaxonomyScorer}.swift`, producer-only `BackfillModels`
types, their tests, plus the classification members carved out of `TMDBClient`/`TMDBWire`. The producer also
gets its own copies of `GenreCatalog`/`GenreRarity`/the grounding map + its own format structs (Part B) — no
DenKit import.
**Stays in DenKit:** `SubgenreIndex`, `SubgenreIndexStore`, `DatasetProvider`, `DatasetDescriptor`,
`IndexConsumers`, format types (`IndexRecord`/`LabelsArtifact`/`LabelConfidence`/`LabelSource`),
`RecipeCatalog`, `DiscoverQuery`, `GenreCatalog`, `GenreRarity`, `TMDBClient`/`TMDBWire`, `LLMClient` (used by
`SubtitleTranslator`/`AIRecoProvider` — only the classifier's use leaves).

**Source-of-truth flip + fallback (kept):** `den-dataset`'s `finalize` output becomes the master; den-atlas
serves it; the app bundles a **snapshot** from atlas (a `make sync-dataset`, replacing `import-dataset.mjs`'s
role). The tvOS bundled fallback is **kept** (offline/out-of-box). Today's `t01`/`e02` bytes are relocated,
not rebuilt. **Gate:** `make verify` green after the extraction.

---

## The format contract — den-atlas's one public interface

Since nothing imports anything across the boundary, the **artifact format IS the contract** — the only thing
the producer, the server, and the app share. It's already how the serving side works (the app hits
`manifest.json`/`dataset.json`/blobs; neither imports the other). Formalise it as den-atlas's published spec:
- `labels-tNN.json`: `{taxonomyVersion, count, records:[{tmdbId, mediaType, primaryGenre,
  subgenres:[{label,confidence}], moods, source, animated}]}`
- `vectors-eNN.bin`: LE `[int32 count][int32 dim]` + row-major int8, 1:1 with records, L2-norm ×127.
- `dataset.json`: the descriptor (per-blob sha256 + size + `datasetVersion` + `embeddingModel`/`dims`).

**Three independent implementations, guarded by one conformance fixture:** the producer *writes* it (Swift),
the server *serves* it (Rust), the app *reads* it (Swift). A tiny checked-in labels+vectors sample that a
round-trip test parses on every side catches schema drift. Note (audit M12) the fixture guards the **schema**,
not the **sha** — different encoders can produce logically-identical-but-different bytes; the app's re-sync
gate keys on `datasetVersion` + sha, so that's expected, not a bug. (Producer and app being both Swift with
identical structs means their bytes *do* match today — a free bonus, not a requirement.)

---

## Phases

1. **RUST-1 — `atlas-serve` at parity** (~3–5 days). Rust project + hand-rolled `serve_bytes` over `tokio::fs`
   + the full port checklist above. New Dockerfile + `publish.yml`; port the vitest suite. *Exit:* every probe
   passes, descriptor byte-identical, **RSS + image measured**, TS retired.
2. **DATASET-1 — extract the standalone Swift producer** (~2–4 days). New `den-dataset` SwiftPM package,
   **no DenKit import**; relocate the calibrated code + tests; add self-contained `GenreCatalog`/`GenreRarity`
   + the extracted grounding map + a thin TMDB client + its own format structs; fold `import-dataset.mjs`'s
   `datasetVersion`/meta/gzip into `finalize`; reconcile e01/e02; add the `EnrichmentSource` seam (text only).
   *Exit:* runs the deterministic phases on 3–5 titles with fixture votes → emits `t01`-shaped output; the
   conformance fixture + `swift test` green.
3. **DEN-CLEANUP** (~1 day). Surgically remove the moved code (incl. the `TMDBClient`/`TMDBWire` edits), flip
   the source-of-truth, wire the app's bundled-snapshot sync. *Exit:* `make verify` green; Den repo carries no
   producer.
4. **(later) FP-2 / Wikipedia** — semantic embedder + a real `EnrichmentSource` (Wikidata resolver + grounding
   fallback) as separate projects.

---

## Testing (minimal, per the constraint)
- **Serve:** boot against the real `data/`; re-run the exact probes used on the TS server (health, manifest,
  `dataset.json` behind proxy headers, Range 206, gzip round-trip whose gunzip sha matches the descriptor,
  If-None-Match 304, HEAD); assert byte-identical descriptor; measure RSS + image.
- **Producer (no Anthropic key):** `TMDB_KEY` only; run the phases on 3–5 titles with **fixture vote JSON**
  (incl. one malformed vote), assert `t01`-shaped output passes the conformance fixture + the app parser.
  Since it's the same Swift code, `classify(rawVotes:)` + FNV + quantizer parity is inherited, not re-proven.
  Real Haiku labeling happens when the owner runs it via Claude Code. **No 60k rebuild.**

## Risks
- **e01/e02 reconciliation** (must resolve before "relocate today's bytes" is true).
- **`import-dataset.mjs` → `finalize`**: the content-addressed `datasetVersion` + per-blob-sha meta is new
  producer work the server now depends on.
- **`TMDBClient`/`TMDBWire` surgery** in the app (not a clean delete).
- **mmap SIGBUS** on in-place republish — use `tokio::fs` streaming (or atomic rename+reopen).
- **Wikipedia decoupling is future work**, not a trait swap (grounding/animated/anime/vote-floor are TMDB
  signals).

## Effort
RUST-1 ~3–5 days · DATASET-1 ~2–4 days (Swift move, not a port) · DEN-CLEANUP ~1 day → **~1.5–2 weeks**, app
untouched until DEN-CLEANUP. (A Rust producer port would be ~2–3 weeks *more* for no runtime benefit — the
audit's recommendation, and mine, is to keep it Swift.)
