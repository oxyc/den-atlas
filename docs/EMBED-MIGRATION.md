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

# Atlas plus the same engine and pinned model; this target is published but not deployed
docker build --target den-atlas-embedded \
  --build-arg DEN_VERSION=$(cat VERSION) -t den-atlas:embedded .
```

Local-engine settings use the compatibility names with `EMBED_` prepended, for example
`EMBED_MODEL_DIR`, `EMBED_MAX_TOKENS`, `EMBED_CACHE_MAX_ENTRIES`, and
`EMBED_IDLE_UNLOAD_SECS`. `EMBED_PORT` is ignored because no second listener is opened.

den-dataset should keep calling the batch HTTP API while migrating its image reference. After the first
release containing this pipeline, its exact compatibility image is
`ghcr.io/oxyc/den-atlas-embed:<Atlas VERSION>` (prefer the release's immutable digest in CI). That image
guarantees corpus and Atlas queries execute the same Rust code, 1024-token cap, vector epoch and pinned model.
Do not archive the standalone repository until a corpus built with this image passes its parity gate.

## Rollout blockers

Before setting `EMBED_LOCAL=1` in production:

1. Both optional Debian targets are built, probed, scanned, signed and published by the Atlas release. The
   model/ORT build needs glibc and cannot use Atlas's current Alpine/scratch image.
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
7. The Rust and Debian bases are digest-pinned and Dependabot owns their refresh. In-process compatibility
   responses have a 10 MiB collection bound, large enough for the engine's maximum batch shape.

Keeping these as rollout gates avoids adding a second abstraction or configuration service. The public
Atlas routes, response bodies, error codes, headers, concurrency bound, timeout, and default deployment stay
as they are while the ownership moves.
