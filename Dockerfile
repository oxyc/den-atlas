# den-atlas — the Rust serving layer. Multi-stage: a static musl binary built on Alpine, copied into
# `scratch` → a ~few-MB, dependency-free image. The server speaks plain HTTP, and its outbound HTTPS
# (JustWatch) uses rustls with bundled roots, so scratch needs no CA bundle. CI builds this with an empty
# data/ (blobs gitignored) → the published image is the SERVER ONLY; mount the dataset at runtime:  docker run -p 8080:8080 -v /path/to/data:/app/data ghcr.io/oxyc/den-atlas
FROM rust:1.99.0-alpine@sha256:0cce0a5e0e8ba67b455257a3a02a1d99005f382748789d6464460028810f1627 AS chef
RUN apk add --no-cache musl-dev \
    && cargo install cargo-chef --version 0.1.78 --locked
WORKDIR /app

# Keep third-party crates in a layer keyed only by the manifests and lockfile. The planner reruns after a source
# edit, but emits the same recipe, so ordinary PRs no longer rebuild Atlas's whole dependency graph.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json -p den-atlas
# Keep the application compile keyed only by inputs Cargo or compile-time fixtures read. In particular,
# VERSION and data/ belong to later image layers and must not rebuild the Rust binary.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY crates ./crates
COPY embed/Cargo.toml ./embed/Cargo.toml
COPY embed/src ./embed/src
COPY judged/rail.json judged/rail-cross.json judged/franchise.json ./judged/
# rust:alpine's default host target is x86_64-unknown-linux-musl → a fully static binary.
RUN cargo build --release --locked -p den-atlas

# The release version reaches only the final image configuration (and this marker), never the Rust build above,
# so a release commit that changes only VERSION reuses the cached compile. A missing or malformed value fails
# here, before an image can be pushed.
FROM rust:1.99.0-alpine@sha256:0cce0a5e0e8ba67b455257a3a02a1d99005f382748789d6464460028810f1627 AS release-metadata
ARG DEN_VERSION
RUN printf '%s\n' "$DEN_VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' \
    || { echo "DEN_VERSION must be X.Y.Z, got '$DEN_VERSION'" >&2; exit 1; }; \
    printf '%s' "$DEN_VERSION" > /den-version

# Optional migration targets. Neither is the Dockerfile's final/default target, so the published Atlas image
# remains the same static-musl server. Select `--target den-atlas-embedded` or `--target den-embed` explicitly.
# ORT's archive needs Debian trixie's newer libstdc++; this cannot reuse the normal Alpine/musl builder.
FROM rust:1.99.0-trixie@sha256:81015672d6939b0265f09adf96d5de125482bb1330106076956b0518e74edc90 AS embedded-chef
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/* \
    && cargo install cargo-chef --version 0.1.78 --locked

FROM embedded-chef AS embedded-planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Keep the corpus inference runtime independent of the Rust ort wrapper. rc.15's automatic
# download advances ORT from 1.28.0 to 1.31.0 and fails the committed byte-exact canary.
# This is the same CPU archive (and hash) shipped by ort-sys rc.13, not a rebuilt approximation.
FROM embedded-chef AS embedded-ort
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates xz-utils \
    && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL https://cdn.pyke.io/0/pyke:ort-rs/ms@1.28.0/x86_64-unknown-linux-gnu.tar.lzma2 -o /tmp/ort.tar.lzma2 \
    && echo 'e454f710f8a49f53aa5b4ff51e3454ae1835777e431c6c35c5255ce6f205fd68  /tmp/ort.tar.lzma2' | sha256sum -c - \
    && mkdir /opt/ort \
    && xz --decompress --stdout --format=raw --lzma2=dict=64MiB /tmp/ort.tar.lzma2 > /tmp/ort.tar \
    && tar -xf /tmp/ort.tar -C /opt/ort \
    && rm /tmp/ort.tar.lzma2 /tmp/ort.tar

FROM embedded-chef AS embedded-build
COPY --from=embedded-ort /opt/ort /opt/ort
# Both cargo-chef and the application must link the pinned archive; no automatic runtime download.
ENV ORT_LIB_PATH=/opt/ort ORT_SKIP_DOWNLOAD=1
COPY --from=embedded-planner /app/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json \
      -p den-atlas -p den-embed --features den-atlas/embedded-model
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY crates ./crates
COPY embed/Cargo.toml ./embed/Cargo.toml
COPY embed/src ./embed/src
COPY judged/rail.json judged/rail-cross.json judged/franchise.json ./judged/
RUN cargo build --release --locked -p den-atlas -p den-embed --features den-atlas/embedded-model

FROM debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f AS embed-model
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

FROM debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f AS embedded-base
RUN apt-get update && apt-get upgrade -y && apt-get install -y --no-install-recommends libgomp1 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 65532 --user-group --no-create-home nonroot
COPY --from=embed-model /models /models

FROM embedded-base AS den-embed
ARG DEN_VERSION
ARG GIT_REVISION
ENV PORT=8080 MODEL_DIR=/models MAX_CHARS=8000 MAX_TOKENS=1024 MALLOC_ARENA_MAX=2 DEN_VERSION=$DEN_VERSION
LABEL org.opencontainers.image.version=$DEN_VERSION org.opencontainers.image.revision=$GIT_REVISION \
      org.opencontainers.image.source="https://github.com/oxyc/den-atlas"
COPY --from=release-metadata /den-version /den-version
COPY --from=embedded-build /app/target/release/den-embed /den-embed
EXPOSE 8080
USER 65532:65532
ENTRYPOINT ["/den-embed"]

FROM embedded-base AS den-atlas-embedded
ARG DEN_VERSION
ARG GIT_REVISION
ENV PORT=8080 DATA_DIR=/app/data EMBED_LOCAL=1 EMBED_MODEL_DIR=/models \
    EMBED_MAX_TOKENS=1024 EMBED_IDLE_UNLOAD_SECS=600 MALLOC_ARENA_MAX=2 DEN_VERSION=$DEN_VERSION
LABEL org.opencontainers.image.version=$DEN_VERSION org.opencontainers.image.revision=$GIT_REVISION \
      org.opencontainers.image.source="https://github.com/oxyc/den-atlas"
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
      org.opencontainers.image.revision=$GIT_REVISION \
      org.opencontainers.image.source="https://github.com/oxyc/den-atlas"
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
