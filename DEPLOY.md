# Deploying den-atlas

Self-hosted, like `den-scout` and `den-trailer-service`: a small Node process behind a reverse proxy that
terminates TLS. The Den app needs an **https** URL (or a LAN/private-range host over http).

## 1. Fetch the dataset
The blobs are gitignored — fetch the published artifact from the [den-dataset](https://github.com/oxyc/den-dataset)
`data-latest` release into `./data` (anonymous; needs curl, python3 and shasum):
```sh
scripts/fetch-dataset.sh
```
This downloads every blob `dataset.meta.json` declares — currently `labels-t02.json`, `vectors-bge-m3.bin`,
`labels-t02.json.gz`, the poster sidecar `metadata-<datasetVersion>.json`, the premise index and
`facets.bin` — plus the manifest itself (per-blob
sha256/size + `datasetVersion` + HTTP-date). The Rust server reads all of that — it never hashes or compresses
at boot. **den-dataset's `finalize` + `publish-dataset.sh` is the source of truth** (not the Den repo);
re-run this to refresh after a new dataset release.

## 2. Run (Docker)
Two options:

**A — pull the published image (GHCR).** The image built by CI is the **server only** (CI has no dataset
blobs), so mount the data you imported in step 1:
```sh
docker run -d --name den-atlas -p 8080:8080 --restart unless-stopped \
  -v "$PWD/data:/app/data:ro" ghcr.io/oxyc/den-atlas:latest
```
Published by `.github/workflows/publish.yml` on a `v*` tag (`git tag v0.1.0 && git push origin v0.1.0`) or a
manual run. First publish: set the package to Public + link it to the repo in GitHub → Packages if you want
it pullable anonymously. (Needs repo Settings → Actions → Workflow permissions = "Read and write".)

**B — build a data-included image locally** (self-contained, no mount). `./data` is baked in, so step 1 must
run first:
```sh
docker build -t den-atlas .
docker run -d --name den-atlas -p 8080:8080 --restart unless-stopped den-atlas
```

Smoke-test either way:
```sh
curl -s localhost:8080/health                        # {"status":"ok"}
curl -s localhost:8080/manifest.json | jq .resources # ["dataset","catalog"]
curl -s -H 'x-forwarded-proto: https' -H 'host: atlas.example.com' \
     localhost:8080/dataset.json | jq '.count, .vectors.url'
curl -s localhost:8080/catalog/movie/jw-nfx.json | jq '.metas | length'  # live JustWatch (needs egress)
```

**Catalogs (JustWatch).** The catalog rows make an outbound call to `apis.justwatch.com` (unauthenticated,
no secret). Tune with `JW_COUNTRY` / `JW_PROVIDERS` / `JW_CACHE_TTL_SECS` (see `.env.example`). Results are
cached in-process ~6h and serve-stale-on-error, so a JustWatch outage degrades to empty rows without
affecting the dataset resource. If the container has no outbound internet, the catalog rows are simply empty.

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

## Search embeds (optional)
Set `DEN_EMBED_URL=http://den-embed:8080` to enable `POST /embed` — a proxy that forwards a search query
(`{"text":"…"}`) to the internal [den-embed](https://github.com/oxyc/den-embed) service and returns its int8
vector (`{"vector":…,"dims":1024,"model":"bge-m3"}`). This is what powers semantic search: the app embeds the
query through Atlas → den-embed, i.e. the SAME bge-m3 + quantizer that built the corpus (the alignment rule),
so query and corpus vectors are comparable. den-embed stays internal — only Atlas is exposed. Unset ⇒ `/embed`
returns 503 and dataset serving is unaffected.

## Refreshing / new versions
Re-run `scripts/fetch-dataset.sh` after a new den-dataset release (its `datasetVersion` bumps iff the bytes changed) → rebuild → redeploy. The app
re-syncs only when `datasetVersion`/`embeddingModel`/`taxonomyVersion` moves; unchanged blobs are served
from the on-device cache with no re-download. A future semantic re-embed just changes `embeddingModel`
(e.g. `e02` → `bge-m3`), which forces a clean re-sync into the new vector space.

## Edge (optional)
`src/worker.ts` is a reference Cloudflare Worker that streams the blobs from an R2 bucket bound as
`ATLAS_BLOBS` (the 33 MB blobs don't fit a Worker bundle). It's excluded from the Node build/CI; the
homelab path is `server.ts` + Docker above.

**Caveat — the edge entry does NOT share the node path's caching layer** (`src/http.ts`). It serves R2's
own `ETag` (not the descriptor's pinned sha256), unversioned blob URLs (no `?v=` → no `immutable`), and no
`Range` or gzip. So the README's "range-resumable / immutable / gzipped" guarantees apply to the **node +
Docker** deployment only. If you deploy the Worker, either put a CDN in front for range/immutable behavior
or wire `serveBytes` into `worker.ts` (it's runtime-agnostic — the blobs would need to be read into memory
or streamed from R2 with manual range handling).
