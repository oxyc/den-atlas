#!/usr/bin/env bash
# Fetch the published dataset artifact from the den-dataset `data-latest` release into ./data. This is the
# source-of-truth flip: den-atlas no longer imports from the Den app repo — den-dataset's finalize output
# (published as a GitHub Release) is the master, and both den-atlas and the Den app fetch from it.
#
#   scripts/fetch-dataset.sh          # populates ./data for `cargo run` or a data-included `docker build`
#
# Anonymous (public repo) — needs only curl + python3, no `gh`/token. The server reads sha256/size/gzip from
# dataset.meta.json (no startup hashing), so all four assets are fetched.
set -euo pipefail

REPO="${DEN_DATASET_REPO:-oxyc/den-dataset}"
BASE="https://github.com/$REPO/releases/download/data-latest"
mkdir -p data

# The meta names the blobs (version-agnostic), so fetch it first, then the files it points at.
curl -fsSL "$BASE/dataset.meta.json" -o data/dataset.meta.json
read -r LABELS VECTORS GZ METADATA PLABELS PVECTORS FACETS < <(python3 - <<'PY'
import json
m = json.load(open("data/dataset.meta.json"))
print(m["labelsFile"], m["vectorsFile"], m.get("labelsGzFile", ""), m.get("metadataFile", ""),
      m.get("premiseLabelsFile", ""), m.get("premiseVectorsFile", ""), m.get("facetsFile", ""))
PY
)
for f in "$LABELS" "$VECTORS" ${GZ:+"$GZ"} ${METADATA:+"$METADATA"} ${PLABELS:+"$PLABELS"} ${PVECTORS:+"$PVECTORS"} ${FACETS:+"$FACETS"}; do
  echo "fetching $f …"
  curl -fsSL "$BASE/$f" -o "data/$f"
done
echo "fetched → ./data:"
ls -la data/*.json data/*.bin data/*.gz 2>/dev/null | awk '{print $5, $NF}'
