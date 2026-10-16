# Repository map and embedding guardrails

Atlas owns discovery and query serving. Its normal image remains a small server that calls the internal
`den-embed` HTTP service. The `embed/` package is the canonical embedding implementation and produces two
optional release targets: the API-compatible `ghcr.io/oxyc/den-atlas-embed` service and the experimental
integrated Atlas image. Read `docs/EMBED-MIGRATION.md` before changing either target or `EMBED_LOCAL`.

## Responsibility boundaries

- This repository owns the embedding implementation, model files, image builds, and release signatures.
- `den-dataset` owns the corpus, embedding-space canary, exact `DEN_EMBED_IMAGE` digest, corpus production,
  and publication. Its `AGENTS.md` and `docs/OPERATE.md` are authoritative for compatibility decisions.
- The `den` repository owns the homelab unit and health-gated updater. The service/API/network name remains
  `den-embed` even though Atlas builds its image.

## Embedding-space safety

Never conclude that an image is compatible from matching `model`, `dims`, `vector_epoch`, and `max_tokens`.
Those four fields are necessary identity checks, but only den-dataset's committed byte-exact canary proves
that the returned vectors belong to the published corpus space. `runtime` is human-readable provenance, not
part of the comparable identity; it may legitimately name the older build that created unchanged rows.

- Test an immutable `ghcr.io/...@sha256:...` reference with den-dataset's `embed image parity` workflow.
- The host CPU is part of the inference environment. Never use an amd64 image under Apple Silicon Docker
  emulation as parity evidence, whether it passes or fails. A native homelab result is useful additional
  evidence but does not replace the dataset workflow on its allowed native x86_64 runner.
- Never regenerate or weaken the canary to make a candidate pass.
- On a failed or invalid comparison, keep the current digest and investigate. Do not infer that the corpus
  needs rebuilding, bump the vector epoch, change production variables, or deploy anything.
- An intentional model, tokenizer, inference-runtime behavior, pooling, quantization, token-cap, or
  vector-space change that changes the vectors must be coordinated in den-dataset as a new space with a
  complete corpus rebuild and publication. A release/version string changing does not imply such a move.

Do not enable `EMBED_LOCAL` while moving image ownership or the compatibility service. Local inference is a
separate rollout: the live dataset must expose the full identity it validates, memory/cold-start behavior must
fit the Atlas unit, and the external service remains the rollback path.
