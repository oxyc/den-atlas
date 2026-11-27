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

## What deserves fixing

Den is a household app: one owner, a few family members, one Apple TV, a homelab box. There is no
adversary inside the household, and diagnostics go to our own already-redacted log collector. Judge
every finding against that, not against a bank.

**Worth fixing (and filing, if you can't fix it now):**
- A person can hit it in normal use and sees something wrong, missing or stuck: wrong audio
  language, skip-credits at the wrong time, no next episode, an empty row, a frozen overlay.
- Data people care about is lost or wrong: watch progress, seen marks, watchlist, library.
- A security hole reachable from the public internet (den-edge public names, public media listener).
- Slowness a person can feel, with a measurement or a trace showing it.
- A failing or flaky gate (`make verify`, Playwright, the dataset daily run).

**Not worth an issue, a PR or a comment** — don't file these, don't fix them in passing:
- Hypothetical failures of local storage: Keychain/SQLite/UserDefaults/filesystem refusing a write,
  "verify by readback", "fail closed when unreadable", rollback-of-a-failed-rollback, quarantine.
- Numeric edge cases nobody hits: overflow, saturation, non-finite values, exhausted clocks/epochs.
- Races and "fence to the exact profile/authority" work without a reproduced user-visible symptom.
- Removing identifiers from our own diagnostics.
- Parity for its own sake. Platforms may differ internally; only a user-visible wrong result counts.
- Localizing rare error or recovery copy, "qualify on hardware" chores, audit/tracker issues.
- Performance "opportunities" without a measurement of a felt problem.
- Bundle-size bookkeeping. Don't write size records into this file or into PRs.

**How to work:**
- Do the task you were given, then stop. Don't audit neighbouring code looking for more.
- Note anything else you noticed in one line in your final report. File an issue only if it meets
  the bar above; at most three per session, after searching for an existing one. Never file batches.
- Smallest fix for the reported symptom, one PR per issue. A PR over ~400 changed lines, or one
  touching more than one client, needs the owner's go-ahead first.
- Don't leave draft PRs behind: finish it to mergeable or close it.
- Fix the same bug on another client only when that client shows the same user-visible symptom.
- Tests prove the symptom is fixed (E2E/integration preferred). Don't add tests for the cases in the
  "not worth it" list.
