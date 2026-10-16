# Embedding ownership and migration

Atlas now owns the embedding implementation in `embed/`. There is one engine, model contract, cache,
quantizer and set of HTTP handlers. It can be used in two forms:

- `den-embed`, a compatibility binary for today's Atlas proxy and den-dataset corpus jobs;
- the `embedded-model` Atlas feature, selected at runtime with `EMBED_LOCAL=1`.

The ordinary Atlas build has no ONNX dependency and still reads `EMBED_URL`. If both variables are set,
`EMBED_URL` wins. Nothing in this change switches production.

## Build and check

```sh
cargo test --workspace
cargo check -p den-atlas --features embedded-model

# Existing small musl/scratch Atlas image (the Dockerfile default)
docker build -t den-atlas .

# Compatibility service, now built from the Atlas workspace
docker build --target den-embed \
  --build-arg DEN_VERSION=$(cat VERSION) -t den-embed .

# Atlas plus the same engine and pinned model; this target is not published or deployed yet
docker build --target den-atlas-embedded \
  --build-arg DEN_VERSION=$(cat VERSION) -t den-atlas:embedded .
```

Local-engine settings use the compatibility names with `EMBED_` prepended, for example
`EMBED_MODEL_DIR`, `EMBED_MAX_TOKENS`, `EMBED_CACHE_MAX_ENTRIES`, and
`EMBED_IDLE_UNLOAD_SECS`. `EMBED_PORT` is ignored because no second listener is opened.

den-dataset should keep calling the batch HTTP API while migrating its image reference. Building the
compatibility target above guarantees its corpus and Atlas queries execute the same Rust code and pinned
model. Do not archive the standalone repository or change dataset's image until this target is published
and its corpus parity gate passes.

## Rollout blockers

Before setting `EMBED_LOCAL=1` in production:

1. Publish and scan both optional Debian targets. The model/ORT build needs glibc and cannot use Atlas's
   current Alpine/scratch image.
2. Run the existing golden-vector parity check on the production amd64 CPU. Query and corpus jobs must use
   the same `MAX_TOKENS` (currently 1024 for the corpus); changing it requires a new vector epoch/corpus.
3. Raise and measure Atlas's memory limit. The current 256 MiB Atlas limit cannot also hold the model; prior
   den-embed measurements are roughly 1.2 GiB at 1024 tokens before Atlas indexes and caches.
4. Observe cold-load latency, RSS after idle unload, and shutdown during inference. Atlas now runs a real
   inference canary before advertising the local engine and reports failures through `/health` and `/ready`,
   but the release pipeline still probes only the default image.
5. Publish a corpus whose manifest carries `embedderVectorEpoch` and `embedderMaxTokens`. Local mode compares
   those with `embeddingModel` and `dims` against the engine and remains unavailable if any field is absent or
   different; existing manifests intentionally fail closed.
6. Point den-dataset at the Atlas-owned compatibility image and complete a corpus build before retiring the
   standalone den-embed release workflow/repository.
7. Pin the optional image's Debian packages/base digest and choose a smaller explicit maximum for responses
   collected across the in-process HTTP boundary. Both are release-hardening follow-ups; neither changes the
   default-off engine selection or the engine-owned inference bound.

Keeping these as rollout gates avoids adding a second abstraction or configuration service. The public
Atlas routes, response bodies, error codes, headers, concurrency bound, timeout, and default deployment stay
as they are while the ownership moves.
