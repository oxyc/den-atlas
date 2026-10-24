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

den-dataset keeps calling the batch HTTP API. Release 0.96.11's exact compatibility image passed
den-dataset's committed byte-exact canary on both the native homelab Intel CPU and an allowed native GitHub
Actions AMD EPYC 7763 runner. The dataset workflow now pins that immutable digest. This proves it belongs to
the existing corpus space; it does not require a corpus rebuild or vector-epoch change.

The optional images pin native ONNX Runtime 1.28.0 by archive SHA independently of the Rust `ort`
wrapper. Upgrading the wrapper to rc.15 otherwise downloads native ORT 1.31.0; release 0.96.17 failed
all seven corpus canaries on an allowed native EPYC 7763 runner despite unchanged model/tokenizer hashes.
Keep native-runtime updates separate from wrapper safety fixes and validate each resulting image.
The wrapper selects its supported `api-28` interface; no version checks are bypassed.

For a compatibility change, manually dispatch Atlas `ci` on the proposed branch. Its optional candidate
artifact names the immutable image to pass to den-dataset's `embed image parity` workflow before merging.
After release, validate the exact released digest again before changing the dataset pin or live service.

The compatibility service deployment is a separate operation in the den repository. Keep its unattended
update hold, verify the Atlas release signature and exact digest, then explicitly update and run the same
canary against the live service. Do not archive the standalone repository until that switch is live and
verified. None of this enables `EMBED_LOCAL`.

## Rollout blockers

Before setting `EMBED_LOCAL=1` in production:

1. Both optional Debian targets are built, probed, scanned, signed and published by the Atlas release. The
   model/ORT build needs glibc and cannot use Atlas's current Alpine/scratch image.
2. Pass den-dataset's `embed image parity` workflow for the exact immutable digest. It rejects known
   incompatible CPUs and runs the corpus's committed byte-exact canary on native x86_64. Never use amd64
   Docker emulation on Apple Silicon as parity evidence, and never regenerate the canary to make a build
   pass. Query and corpus paths must use the same `MAX_TOKENS` (currently 1024); changing it requires a new
   vector epoch and complete corpus rebuild.
3. Raise and measure Atlas's memory limit. The current 256 MiB Atlas limit cannot also hold the model; prior
   den-embed measurements are roughly 1.2 GiB at 1024 tokens before Atlas indexes and caches.
4. Observe cold-load latency, RSS after idle unload, and shutdown during inference. Atlas now runs a real
   inference canary before advertising the local engine and reports failures through `/health` and `/ready`,
   but the release pipeline still probes only the default image.
5. Publish a corpus whose manifest carries `embedderVectorEpoch` and `embedderMaxTokens`. Local mode compares
   those with `embeddingModel` and `dims` against the engine and remains unavailable if any field is absent or
   different; existing manifests intentionally fail closed.
6. Keep den-dataset pinned to the exact validated Atlas-owned compatibility digest. Retire the standalone
   den-embed release workflow/repository only after the compatibility service is deployed and its live
   canary passes. A fresh dataset publication carrying the complete comparable identity (model, dimensions,
   vector epoch, and token cap) is still required before enabling local mode, but byte-identical image
   ownership migration does not require rebuilding the existing corpus. `embedderRuntime` is provenance for
   the build that created the rows and is deliberately not compared.
7. The Rust and Debian bases are digest-pinned and Dependabot owns their refresh. In-process compatibility
   responses have a 10 MiB collection bound, large enough for the engine's maximum batch shape.

Keeping these as rollout gates avoids adding a second abstraction or configuration service. The public
Atlas routes, response bodies, error codes, headers, concurrency bound, timeout, and default deployment stay
as they are while the ownership moves.
