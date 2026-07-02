# Plan — den-atlas → Rust, and pulling the dataset producer out of the TV app

Two coupled goals:
1. **Rewrite the always-on serving layer in Rust** for minimal memory + optimal CPU.
2. **Move the whole dataset "ranking/taxonomy" producer out of the Den TV app into den-atlas** (it has no
   business in a tvOS app repo). The producer runs rarely, so it *could* be a slower language — but making it
   Rust too lets it share the artifact-format types with the server (one source of truth). Go is a viable
   alternative for the producer alone.

**Constraint from the owner:** the existing `t01`/`e02` labels+vectors are NOT being rebuilt — den-atlas keeps
serving today's bytes. The producer is being *relocated* (and modernised), and only **minimally tested** end
to end (a handful of titles), not run at 60k scale. A `.env` with `TMDB_KEY` is available; the classify step
additionally needs an `ANTHROPIC_API_KEY`.

---

## Architecture: one Cargo workspace, two binaries + a shared crate

```
den-atlas/
  crates/
    atlas-format/   # shared: IndexRecord/LabelsArtifact, the vector-blob codec, DatasetDescriptor, meta
    atlas-serve/    # the always-on HTTP addon (axum) — replaces src/*.ts
    atlas-build/    # the rare batch producer (TMDB → classify → embed → quantize → publish)
  data/             # labels-t01.json, vectors-e02.bin, labels-t01.json.gz, dataset.meta.json (gitignored)
  Dockerfile        # multi-stage: musl static build → FROM scratch (~10 MB image)
  .github/workflows/publish.yml   # build the Rust image → GHCR
  docs/rust-migration-plan.md
```

`atlas-serve` and `atlas-build` both depend on `atlas-format`, so the **labels JSON schema + the
`[int32 count][int32 dim]` int8 vector layout are defined exactly once** and can't drift between producer and
server. This shared-types win is the main reason to prefer Rust over Go for the producer.

---

## Part A — `atlas-serve` (Rust): the memory/CPU rewrite

Today (TypeScript/Node): loads both blobs (33 MB) + the gzip (0.6 MB) into the heap → **~34 MB RSS**, 196 MB
Alpine image. The Rust rewrite keeps every current behaviour (see the vitest suite) and drives that down hard.

**Stack:** `axum` (tokio + hyper) + `tower-http`. Small, mature, ideal for static + custom handlers.

**Memory wins (the point):**
- **Never load a blob into RAM.** Serve it by **streaming from disk** (`tokio::fs::File` + `ReaderStream`, 64
  KiB chunks) — or `sendfile`/`mmap` (`memmap2`) for the hot path. RSS stays flat regardless of blob size or
  concurrency. Range slices stream the requested byte window only.
- **Precompute gzip to a file** (`labels-t01.json.gz`, written by the producer/import) and stream *that* for
  gzip clients — zero runtime compression, zero in-RAM gzip copy (Node held it in the heap).
- **No startup hashing.** Read sha256 + size + `builtAt` from the `dataset.meta.json` sidecar (the producer
  writes them), so boot is a tiny file read (~1 ms) instead of hashing 33 MB.
- **Target: < 10 MB RSS** serving the same 33 MB, boot < 5 ms.

**CPU wins:** static serving becomes syscall-bound (stream/`sendfile`); sha computed **once, offline** (in the
producer) not per-boot; gzip precomputed; tokio handles many concurrent Range/blob requests with constant
memory.

**Caching:** port `src/http.ts` verbatim (it's ~200 lines and fully spec'd + tested). Keep: strong `ETag`
(sha for blobs / a cheap hash for JSON) + `If-None-Match`, `Last-Modified` + `If-Modified-Since` → 304, `HEAD`,
`Range`/206/416 + `Accept-Ranges`, gzip negotiation with `Vary` only when a gzip variant exists, the distinct
`"<sha>-gzip"` ETag, and version-stamped (`?v=`) immutable URLs. Port the vitest cases to `#[tokio::test]`.

**Endpoints:** unchanged — `/`, `/health`, `/manifest.json`, `/dataset.json` (absolute, `?v`-stamped blob
URLs from `X-Forwarded-Proto`/`Host`), `/labels-t01.json`, `/vectors-e02.bin`. Byte-identical descriptor
(same sha as today) so the Den app can't tell the difference.

**Image:** multi-stage — `rust:1-alpine` with the `x86_64-unknown-linux-musl` target builds a **static
binary**, copied into `FROM scratch` (or `distroless/static`). **~5–15 MB** vs 196 MB. Data is mounted (as
now). Update `publish.yml` to build the Rust image.

**Cutover:** land `atlas-serve` at parity (probes below), tag the current tree `legacy-ts`, then delete
`src/*.ts` + the node toolchain. Same repo, same GHCR package name.

---

## Part B — `atlas-build` (Rust): the producer, moved out of the TV app

Everything the Den repo currently carries only to *build* the dataset moves here. It's ~1,300 LOC of Swift
today; a direct port. What moves and what it becomes:

| Swift (Den repo, producer-only) | Rust (`atlas-build`) | Notes |
|---|---|---|
| `tools/taxonomy-backfill/main.swift` (762) | the CLI + resumable phases | worklist → enrich → classify → embed → assemble → finalize, with checkpoints for a 24 h run |
| `Backfill/TaxonomyClassifier.swift` (211) | `classifier.rs` | port EXACTLY: n=3 self-consistency, fused primary genre (majority + IDF rarity tie-break), per-family thresholds **0.70/0.55/0.60**, grounding bonus, off-vocab reject, top-3 |
| `Taxonomy/Taxonomy.swift` + `TaxonomyScorer.swift` | `taxonomy.rs` | the controlled vocab (`t01`) + IDF weights — data port |
| `Backfill/Embedder.swift` (FNV + Quantizer) | `embed.rs` | FNV-1a signed feature hashing → 384-d, L2-norm; int8 ×127. Trivial, port **bit-exact** (golden test) |
| `Backfill/BackfillPipeline.swift` | the orchestration in the CLI | |
| producer half of `BackfillModels.swift` (WorklistEntry, EnrichedTitle, RunReport, Checkpoint, Worklist) | `model.rs` | the *format* half (IndexRecord/LabelsArtifact) stays in the app + lives in `atlas-format` |
| `TMDBClient` (the bits the tool uses) | `tmdb.rs` (reqwest) | `/discover`, daily-export parse, `classificationRecord` (detail + `append_to_response=keywords`), vote-floor + anime filter |

**The LLM step changes for the better.** Today classification is **agent-driven**: `main.swift` ships a
`NoLLM` stub and an external "Opus loop spawns Haiku subagents" writes `out/votes/batch-N-passK.json`, which
`classify(rawVotes:)` then aggregates. The port replaces that with a **direct Anthropic Messages API client**
(`llm.rs`, Haiku, n=3, temp 0, the existing prompt) — fully reproducible from an `ANTHROPIC_API_KEY`, no
Claude Code harness dependency. The calibrated aggregation is unchanged.

**CLI:** `atlas-build run` (resumable) or per-phase subcommands, reading `.env` (`TMDB_KEY`,
`ANTHROPIC_API_KEY`). Output lands in `data/` → the server serves it + the producer also writes
`labels-t01.json.gz` and `dataset.meta.json` (sha/size/builtAt) so the server needs zero startup work.

**ToS invariant preserved:** the finalize step keeps the "no raw TMDB overview/text in the published
artifact" assertion (derived labels + int8 vectors only).

**Language note:** Rust for the DRY win (shares `atlas-format` with the server). **Go is acceptable** for
`atlas-build` alone (reqwest→net/http, serde→encoding/json) — the cost is defining the artifact format a second
time and losing the shared codec. Recommendation: **Rust**, unless you'd rather iterate the producer faster in
Go and accept the duplicated format.

---

## Part C — Den app cleanup (what leaves, what stays)

**Delete from the Den repo** (0 `App/` references — verified): `tools/taxonomy-backfill/`,
`Backfill/{BackfillPipeline,TaxonomyClassifier,Embedder}.swift`, `Taxonomy/{Taxonomy,TaxonomyScorer}.swift`,
their tests, and the producer-only types in `BackfillModels.swift`. (`LLMClient` STAYS — `SubtitleTranslator`
+ `AIRecoProvider` use it; only the classifier's use goes.)

**Keep in DenKit (runtime feature store):** `SubgenreIndex`, `SubgenreIndexStore`, `DatasetProvider`,
`DatasetDescriptor`, `IndexConsumers`, the *format* types (`IndexRecord`/`LabelsArtifact`/`LabelConfidence`/
`LabelSource`), `RecipeCatalog`, `DiscoverQuery`, `GenreCatalog`, `GenreRarity`.

**Flip the source of truth.** Today the Den repo's `Resources/` is the master and den-atlas imports from it.
After: **`atlas-build` output is the master**; the app bundles a *snapshot* copied from atlas (a
`make sync-dataset` that pulls atlas's `data/`, or a published release asset). Today's `t01`/`e02` bytes are
just relocated — not rebuilt.

**Open decision — the tvOS bundled fallback:** keep a bundled snapshot (app works offline / out-of-box, ~30
MB) or drop it (slimmer app, degrades to recipes+TMDB until the first atlas sync). Recommend **keep**, sourced
from atlas.

**Gate:** `make verify` stays green after the deletion (the app never used the deleted code).

---

## The format contract (the one invariant that must not drift)

The producer, the server, and the Den app's `SubgenreIndex` parser must agree byte-for-byte on:
- `labels-tNN.json`: `{ taxonomyVersion, count, records:[{tmdbId, mediaType, primaryGenre, subgenres:[{label,
  confidence}], moods:[…], source, animated}] }`
- `vectors-eNN.bin`: little-endian `[int32 count][int32 dim]` then `count*dim` int8, row-major, aligned 1:1
  with `records`; int8 = L2-normalized float ×127.

Guard it with a **cross-language conformance fixture**: a tiny labels+vectors sample the Rust `atlas-format`
codec *produces* and the Swift `SubgenreIndex` *parses* (checked into both repos), so a change on either side
fails a test.

---

## Phases

1. **RUST-1 — serve at parity.** Workspace + `atlas-format` + `atlas-serve` (axum, streaming, full caching
   port). Serve the existing `data/`. Port the vitest suite. New musl/scratch Dockerfile + GHCR workflow.
   *Exit:* byte-identical descriptor, all caching probes pass, **RSS + image size measured** (prove the win),
   TS retired.
2. **RUST-2 — producer port.** `atlas-build`: tmdb, taxonomy, classifier, Anthropic LLM, embed+quantize,
   assemble/finalize, resumable CLI. Golden parity tests (embedder/quantizer bit-exact; a few classifier
   cases). *Exit:* minimal end-to-end run on 3–5 titles from `.env` produces a valid chunk the conformance
   fixture accepts.
3. **DEN-CLEANUP.** Delete the moved Swift + tests, trim `BackfillModels` to format-only, flip the
   source-of-truth, decide the bundled fallback. *Exit:* `make verify` green; Den repo carries no producer.
4. **(later) FP-2 hook.** `atlas-build`'s `Embedder` trait leaves room for a real semantic model (bge-m3 via
   Workers AI or `candle`) — the quality upgrade then lives naturally in the producer, not the app.

---

## Testing (minimal, per the constraint)

- **Serve:** boot `atlas-serve` against the real `data/`; re-run the exact probes already used on the TS
  server (health, manifest, `dataset.json` behind proxy headers, `Range` 206, gzip round-trip whose gunzip
  sha matches the descriptor, `If-None-Match` 304, HEAD). Assert the descriptor is byte-identical to today.
  Measure RSS (`/proc` or `ps`) and image size to confirm the targets.
- **Producer:** with `TMDB_KEY` (+ `ANTHROPIC_API_KEY` for classify), run `atlas-build` on **3–5 known
  titles** end to end → assert the emitted labels+vectors pass the conformance fixture and the Swift parser.
  Golden unit tests: the FNV embedder + int8 quantizer match the Swift output bit-for-bit on fixed inputs; a
  couple of `classify(rawVotes:)` cases match the Swift calibrated result. **No 60k rebuild.**

---

## Risks & watch-items

- **Classifier fidelity** — the thresholds + IDF tie-break must port exactly; golden-test them. (Low urgency:
  nothing is being rebuilt now, so this only bites a future run.)
- **Format drift** — mitigated by the shared `atlas-format` crate + the cross-language conformance fixture.
- **Toolchain** — adds cargo + musl cross-compile to the homelab (one-time); the image gets *smaller* and
  dependency-free.
- **`ANTHROPIC_API_KEY`** — needed for the classify step; `TMDB_KEY` alone covers enrich/embed/format, so the
  minimal test can exercise everything except classify (or classify a couple of titles with a key).
- **Bundled-fallback decision** — product call (keep vs drop), independent of the rest.

## Rough effort
RUST-1 ~2–3 days (serving is small + well-spec'd). RUST-2 ~3–5 days (the classifier + LLM + TMDB port is the
bulk). DEN-CLEANUP ~0.5 day. Total ~1–1.5 weeks, landable in the phase order above with the app untouched
until Phase 3.
