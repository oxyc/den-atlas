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
# EVERY "<name>File" the meta declares, newline-delimited. A positional list had to be extended by
# hand for each new artifact and silently skipped anything missed: metadataGzFile was added to the
# server and never fetched here, so a dev's ./data had no gz variant and the server logged one
# missing. deploy/atlas-dataset-sync.sh has been generic over these keys for exactly this reason.
FILES="$(python3 - <<'PY'
import json
m = json.load(open("data/dataset.meta.json"))
for k, v in m.items():
    if k.endswith("File") and isinstance(v, str) and v:
        print(v)
PY
)"
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

# The two the server cannot start without. deploy/atlas-dataset-sync.sh has always required these;
# "$FILES is non-empty" only says SOMETHING was declared, so `"labelsFile": ""`, or a meta declaring
# just labelsGzFile, fetched happily and reported success with a ./data den-atlas cannot load.
for required in labelsFile vectorsFile; do
  python3 - "$required" <<'PY' || { echo "release meta declares no $required — refusing" >&2; exit 1; }
import json, sys
m = json.load(open("data/dataset.meta.json"))
sys.exit(0 if m.get(sys.argv[1]) else 1)
PY
done

# Newline-delimited via a redirect, so a name is never word-split or glob-expanded and `set -e` can
# still abort the script (a pipe would put this loop in a subshell).
[ -n "$FILES" ] || { echo "release meta declares no blobs — refusing" >&2; exit 1; }
MISSING=""
while IFS= read -r f; do
  [ -n "$f" ] || continue
  safe_name "$f" || { echo "refusing unsafe blob name: $f" >&2; exit 1; }
  echo "fetching $f …"
  # A declared name that is not a release asset is NOT fatal. Reading every "<name>File" key means
  # picking up any future key of that shape, blob or not, and aborting mid-loop left ./data holding
  # the meta and whichever files happened to come first. The two required ones are checked above, so
  # anything else missing is a note, not a failure.
  curl -fsSL "$BASE/$f" -o "data/$f" || { rm -f "data/$f"; MISSING="$MISSING $f"; }
done <<EOF
$FILES
EOF
[ -z "$MISSING" ] || echo "not in the release, skipped:$MISSING" >&2
echo "fetched → ./data:"
# `|| true`: an unmatched glob makes `ls` exit 2, and under `set -o pipefail` that failed the whole
# script AFTER a completely successful fetch — a release with no .gz was enough.
ls -la data/*.json data/*.bin data/*.gz 2>/dev/null | awk '{print $5, $NF}' || true
