# den-atlas — the Rust serving layer. Multi-stage: a static musl binary built on Alpine, copied into
# `scratch` → a ~few-MB, dependency-free image. The server speaks plain HTTP, and its outbound HTTPS
# (JustWatch) uses rustls with bundled roots, so scratch needs no CA bundle. CI builds this with an empty
# data/ (blobs gitignored) → the published image is the SERVER ONLY; mount the dataset at runtime:  docker run -p 8080:8080 -v /path/to/data:/app/data ghcr.io/oxyc/den-atlas
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY crates ./crates
# The package is optional for the normal Atlas binary, but it is a workspace member and Cargo must be able
# to read its manifest while resolving the locked workspace. It is not linked into this default build.
COPY embed ./embed
# Compiled in (`raileval.rs`): the playground scores its knobs against the judged set, both files.
COPY judged/rail.json judged/rail-cross.json judged/franchise.json ./judged/
# rust:alpine's default host target is x86_64-unknown-linux-musl → a fully static binary.
RUN cargo build --release --locked

# The release version reaches only the final image configuration (and this marker), never the Rust build above,
# so a release commit that changes only VERSION reuses the cached compile. A missing or malformed value fails
# here, before an image can be pushed.
FROM rust:1-alpine AS release-metadata
ARG DEN_VERSION
RUN printf '%s\n' "$DEN_VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' \
    || { echo "DEN_VERSION must be X.Y.Z, got '$DEN_VERSION'" >&2; exit 1; }; \
    printf '%s' "$DEN_VERSION" > /den-version

# Optional migration targets. Neither is the Dockerfile's final/default target, so the published Atlas image
# remains the same static-musl server. Select `--target den-atlas-embedded` or `--target den-embed` explicitly.
# ORT's archive needs Debian trixie's newer libstdc++; this cannot reuse the normal Alpine/musl builder.
FROM rust:1-trixie AS embedded-build
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY crates ./crates
COPY embed ./embed
COPY judged/rail.json judged/rail-cross.json judged/franchise.json ./judged/
RUN cargo build --release --locked -p den-atlas --features embedded-model \
    && cargo build --release --locked -p den-embed

FROM debian:trixie-slim AS embed-model
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /models
ARG HF_REV=4de13258303883538bd53b696b452bf8099f0858
ARG HF=https://huggingface.co/Xenova/bge-m3/resolve/${HF_REV}
ARG MODEL_SHA256=a206e10e995aa2a833924bcd725ba5dd6c3425cd34bac3cf2b5677cd2a1c51d6
ARG TOKENIZER_SHA256=6710678b12670bc442b99edc952c4d996ae309a7020c1fa0096dd245c2faf790
RUN curl -fsSL "$HF/onnx/model_int8.onnx" -o model_int8.onnx \
    && curl -fsSL "$HF/tokenizer.json" -o tokenizer.json \
    && echo "${MODEL_SHA256}  model_int8.onnx" | sha256sum -c - \
    && echo "${TOKENIZER_SHA256}  tokenizer.json" | sha256sum -c -

FROM debian:trixie-slim AS embedded-base
RUN apt-get update && apt-get upgrade -y && apt-get install -y --no-install-recommends libgomp1 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 65532 --user-group --no-create-home nonroot
COPY --from=embed-model /models /models

FROM embedded-base AS den-embed
ARG DEN_VERSION
ARG GIT_REVISION
ENV PORT=8080 MODEL_DIR=/models MAX_CHARS=8000 MALLOC_ARENA_MAX=2 DEN_VERSION=$DEN_VERSION
LABEL org.opencontainers.image.version=$DEN_VERSION org.opencontainers.image.revision=$GIT_REVISION
COPY --from=release-metadata /den-version /den-version
COPY --from=embedded-build /app/target/release/den-embed /den-embed
EXPOSE 8080
USER 65532:65532
ENTRYPOINT ["/den-embed"]

FROM embedded-base AS den-atlas-embedded
ARG DEN_VERSION
ARG GIT_REVISION
ENV PORT=8080 DATA_DIR=/app/data EMBED_LOCAL=1 EMBED_MODEL_DIR=/models \
    EMBED_IDLE_UNLOAD_SECS=600 MALLOC_ARENA_MAX=2 DEN_VERSION=$DEN_VERSION
LABEL org.opencontainers.image.version=$DEN_VERSION org.opencontainers.image.revision=$GIT_REVISION
COPY --from=release-metadata /den-version /den-version
COPY --from=embedded-build /app/target/release/den-atlas /den-atlas
COPY data /app/data
EXPOSE 8080
USER 65532:65532
ENTRYPOINT ["/den-atlas"]

FROM scratch AS runtime
ARG DEN_VERSION
ARG GIT_REVISION
ENV DATA_DIR=/app/data \
    PORT=8080 \
    DEN_VERSION=$DEN_VERSION
LABEL org.opencontainers.image.version=$DEN_VERSION \
      org.opencontainers.image.revision=$GIT_REVISION
COPY --from=release-metadata /den-version /den-version
COPY --from=build /app/target/release/den-atlas /den-atlas
# The dataset blobs (gitignored; fetched via scripts/fetch-dataset.sh). Empty in CI → mount at runtime.
COPY data /app/data
EXPOSE 8080
# scratch has no /etc/passwd, so the uid is numeric: 65532, the one every den addon image runs as
# (distroless's `nonroot`). No HEALTHCHECK, deliberately: a periodic probe keeps an idle box awake, and
# health is checked by the deploy (den/deploy/den-update.sh) against /health and /manifest.json.
USER 65532:65532
ENTRYPOINT ["/den-atlas"]
