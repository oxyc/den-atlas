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
