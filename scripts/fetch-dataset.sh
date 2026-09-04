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
# The names come from a release meta we do not control, and are used as `-o "data/$f"` — so
# "../../../x" writes outside the repo. den-atlas rejects the same shapes when it loads them.
# An allowlist: "not a path" is the right question for the server, where `*` is a legal file name,
# but not for a shell, which expands it.
#
# deploy/atlas-dataset-sync.sh has the same guard over a LONGER reserved list — it also keeps
# `.needs-restart`, the pre-move location of its container-restart marker, which has no counterpart
# here. Claiming the two are "the same guard" is what let them drift apart last time, so the
# difference is stated rather than asserted away.
safe_name() {
  case "$1" in
    # dataset.meta.json is this script's own working copy in data/: a release declaring it as a blob
    # overwrites the meta mid-fetch and Dataset::load then fails outright.
    "" | . | .. | dataset.meta.json) return 1 ;;
    *[!A-Za-z0-9._-]*) return 1 ;;
    -*) return 1 ;;
    *) return 0 ;;
  esac
}

for f in "$LABELS" "$VECTORS" ${GZ:+"$GZ"} ${METADATA:+"$METADATA"} ${PLABELS:+"$PLABELS"} ${PVECTORS:+"$PVECTORS"} ${FACETS:+"$FACETS"}; do
  safe_name "$f" || { echo "refusing unsafe blob name: $f" >&2; exit 1; }
  echo "fetching $f …"
  curl -fsSL "$BASE/$f" -o "data/$f"
done
echo "fetched → ./data:"
ls -la data/*.json data/*.bin data/*.gz 2>/dev/null | awk '{print $5, $NF}'
