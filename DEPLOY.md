# Deploying den-atlas

Self-hosted, like `den-scout` and `den-trailer-service`: a small Node process behind a reverse proxy that
terminates TLS. The Den app needs an **https** URL (or a LAN/private-range host over http).

## 1. Import the dataset
The blobs are gitignored — pull them from the Den repo into `./data`:
```sh
DEN_REPO=/path/to/den npm run import
```
This copies `labels-t01.json` + `vectors-e02.bin` and writes `data/dataset.meta.json` (dims/count read
from the blobs, `datasetVersion` content-addressed). Re-run to refresh when Den publishes new artifacts.

## 2. Build + run (Docker)
```sh
docker build -t den-atlas .
docker run -d --name den-atlas -p 8080:8080 --restart unless-stopped den-atlas
```
The image bakes `./data` in, so step 1 must run first. Smoke-test:
```sh
curl -s localhost:8080/health                       # {"status":"ok"}
curl -s localhost:8080/manifest.json | jq .resources # ["dataset"]
curl -s -H 'x-forwarded-proto: https' -H 'host: atlas.example.com' \
     localhost:8080/dataset.json | jq '.count, .vectors.url'
```

## 3. Reverse proxy (Caddy)
```
atlas.example.com {
    reverse_proxy 127.0.0.1:8080
}
```
Caddy forwards `X-Forwarded-Proto` + `Host`, so `dataset.json` emits
`https://atlas.example.com/{labels,vectors}-…` — the URLs the app fetches. If you serve the blobs from a
CDN instead, set `PUBLIC_BASE_URL=https://cdn.example.com/atlas` and point the CDN at this origin.

## 4. Install in Den
Den → Settings → Plugins → add `https://atlas.example.com/manifest.json`. On next launch Den syncs the
dataset (sha256-gated, stale-while-revalidate) and the on-device feature store comes from Atlas instead of
the bundled copy. Removing the addon falls back to the bundled artifact — discovery never goes blank.

## Refreshing / new versions
Re-run `npm run import` (bumps `datasetVersion` iff the bytes changed) → rebuild → redeploy. The app
re-syncs only when `datasetVersion`/`embeddingModel`/`taxonomyVersion` moves; unchanged blobs are served
from the on-device cache with no re-download. A future semantic re-embed just changes `embeddingModel`
(e.g. `e02` → `bge-m3`), which forces a clean re-sync into the new vector space.

## Edge (optional)
`src/worker.ts` is a reference Cloudflare Worker that streams the blobs from an R2 bucket bound as
`ATLAS_BLOBS` (the 33 MB blobs don't fit a Worker bundle). It's excluded from the Node build/CI; the
homelab path is `server.ts` + Docker above.
