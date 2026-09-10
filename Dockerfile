# den-atlas — the Rust serving layer. Multi-stage: a static musl binary built on Alpine, copied into
# `scratch` → a ~few-MB, dependency-free image. The server speaks plain HTTP, and its outbound HTTPS
# (JustWatch) uses rustls with bundled roots, so scratch needs no CA bundle. CI builds this with an empty
# data/ (blobs gitignored) → the published image is the SERVER ONLY; mount the dataset at runtime:  docker run -p 8080:8080 -v /path/to/data:/app/data ghcr.io/oxyc/den-atlas
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# rust:alpine's default host target is x86_64-unknown-linux-musl → a fully static binary.
RUN cargo build --release --locked

FROM scratch AS runtime
ENV ATLAS_DATA_DIR=/app/data \
    PORT=8080
COPY --from=build /app/target/release/den-atlas /den-atlas
# The dataset blobs (gitignored; fetched via scripts/fetch-dataset.sh). Empty in CI → mount at runtime.
COPY data /app/data
EXPOSE 8080
# scratch has no /etc/passwd, so the uid is numeric: 65532, the one every den addon image runs as
# (distroless's `nonroot`). No HEALTHCHECK, deliberately: a periodic probe keeps an idle box awake, and
# health is checked by the deploy (den/deploy/den-update.sh) against /health and /manifest.json.
USER 65532:65532
ENTRYPOINT ["/den-atlas"]
