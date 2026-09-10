# Deploying den-atlas

The live deployment is the Den homelab box, from the den repo's `deploy/` directory. Provisioning,
the health-gated updater and rollback are described in that repo's `deploy/README.md`; this file covers
what is specific to atlas. The Den app needs an **https** URL or a LAN/private-range host over http —
the homelab serves atlas over LAN http.

## On the homelab
The unit is `deploy/quadlet/den-atlas.container`, run by rootful Podman under systemd:

- `ghcr.io/oxyc/den-atlas`, host port **8081** → 8080 in the container.
- Read-only root filesystem, every capability dropped, no new privileges; the image runs as uid 65532.
- Environment from `/etc/den/env/den-atlas.env` (the variables in [`.env.example`](.env.example)).
- The dataset is bind-mounted **read-only** from `/var/lib/den/atlas-data` at `/app/data`. The image is
  server-only, so the `atlas-dataset-sync` timer keeps that directory current from the den-dataset
  `data-latest` release and restarts atlas only when the release actually changed.
- Updates go through `den-update`: a new `:latest` is proved in a throwaway container against `/health`
  and `/manifest.json` before its digest is pinned, and a failed proof rolls back.

## Running it yourself

### 1. Fetch the dataset
The blobs are gitignored — fetch the published artifact from the [den-dataset](https://github.com/oxyc/den-dataset)
`data-latest` release into `./data` (anonymous; needs curl, python3 and shasum):
```sh
scripts/fetch-dataset.sh
```
This downloads every blob `dataset.meta.json` declares — the labels (and their `.gz`), the vectors, the
poster sidecar `metadata-<datasetVersion>.json`, the premise index and `facets.bin` — plus the meta itself
(per-blob sha256/size + `datasetVersion` + HTTP-date). The server reads all of that at startup; it never
hashes or compresses. **den-dataset's `finalize` + `publish-dataset.sh` is the source of truth.**

### 2. Run
The published image (built by `.github/workflows/docker-publish.yml` on a `v*` tag, after `ci.yml` passes
and only if the tag equals the `Cargo.toml` version) is the **server only**, so mount the data:
```sh
docker run -d --name den-atlas -p 8080:8080 --read-only \
  -v "$PWD/data:/app/data:ro" ghcr.io/oxyc/den-atlas:latest
```
`docker build -t den-atlas .` bakes whatever is in `./data` into the image instead.

Smoke test:
```sh
curl -s localhost:8080/health                        # {"status":"ok"}, or "degraded" with a reason
curl -s localhost:8080/manifest.json | jq .resources # ["dataset","catalog"]
curl -s -H 'x-forwarded-proto: https' -H 'host: atlas.example.com' \
     localhost:8080/dataset.json | jq '.count, .vectors.url'
curl -s localhost:8080/catalog/movie/jw-nfx.json | jq '.metas | length'  # live JustWatch (needs egress)
curl -s -H "Authorization: Bearer $METRICS_TOKEN" localhost:8080/metrics  # only with METRICS_TOKEN set
```

**Catalogs (JustWatch).** The catalog rows make an outbound call to `apis.justwatch.com` (unauthenticated,
no secret). Results are cached in-process ~6h and serve-stale-on-error, so a JustWatch outage degrades to
empty rows without affecting the dataset resource. With no outbound internet the rows are simply empty.

## Blob URLs, proxies and CDNs
`dataset.json` builds its absolute blob URLs from `X-Forwarded-Proto` + `X-Forwarded-Host`/`Host`, so
a proxy in front that forwards those gets URLs on its own origin. To serve the blobs from a CDN instead,
set `PUBLIC_BASE_URL=https://cdn.example.com/atlas` and point the CDN at this origin.

## Install in Den
Den → Settings → Plugins → add `http://<den-ip>:8081/manifest.json` (or your https origin). On next launch
Den syncs the dataset (sha256-gated, stale-while-revalidate) and the on-device feature store comes from
Atlas instead of the bundled copy. Removing the addon falls back to the bundled artifact — discovery never
goes blank.

## Search embeds (optional)
Set `EMBED_URL` (on the homelab, `http://den-embed:8080` over the shared Podman network) to enable
`POST /embed` — a proxy that forwards a search query (`{"text":"…"}`) to the internal
[den-embed](https://github.com/oxyc/den-embed) service and returns its int8 vector
(`{"vector":…,"dims":1024,"model":"bge-m3"}`). The app embeds its query through Atlas → den-embed, i.e. the
SAME bge-m3 + quantizer that built the corpus, so query and corpus vectors are comparable. den-embed stays
internal — only Atlas is exposed. Unset ⇒ `/embed` returns 503 and dataset serving is unaffected.

## Refreshing / new versions
On the homelab the sync timer does this. By hand: re-run `scripts/fetch-dataset.sh` and restart the
server, which reads the meta at startup. The app re-syncs only when `datasetVersion`/`embeddingModel`/
`taxonomyVersion` moves; unchanged blobs are served from the on-device cache with no re-download. A
change of `embeddingModel` forces a clean re-sync into the new vector space.
