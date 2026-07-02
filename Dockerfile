# den-atlas — the Rust serving layer. Multi-stage: a static musl binary built on Alpine, copied into
# `scratch` → a ~few-MB, dependency-free image. No TLS / no outbound (it sits behind Caddy), so scratch is
# enough. CI builds this with an empty data/ (blobs gitignored) → the published image is the SERVER ONLY;
# mount the dataset at runtime:  docker run -p 8080:8080 -v /path/to/data:/app/data ghcr.io/oxyc/den-atlas
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
# scratch has no /etc/passwd; run as the numeric `nobody`. (No Docker HEALTHCHECK — scratch has no shell;
# health is a plain `GET /health`, checked by the reverse proxy / compose.)
USER 65534:65534
ENTRYPOINT ["/den-atlas"]
