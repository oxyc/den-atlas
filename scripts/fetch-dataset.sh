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

# What the server cannot start without. `"$FILES" is non-empty` only says SOMETHING was declared,
# so `"labelsFile": ""`, or a meta declaring just labelsGzFile, fetched happily and reported success
# with a ./data den-atlas cannot load.
REQUIRED="labels vectors"
for r in $REQUIRED; do
  python3 - "${r}File" <<'PY' || { echo "release meta declares no ${r}File — refusing" >&2; exit 1; }
import json, sys
m = json.load(open("data/dataset.meta.json"))
sys.exit(0 if m.get(sys.argv[1]) else 1)
PY
done
[ -n "$FILES" ] || { echo "release meta declares no blobs — refusing" >&2; exit 1; }

# PRE-FLIGHT, before anything on disk is touched, the same shape deploy/atlas-dataset-sync.sh has
# always had. Skipping a 404 mid-loop was meant to tolerate a "<name>File" key that is not a release
# asset; what it actually did was delete the previously-good copy of a REQUIRED blob (the `rm -f` on
# the failure branch) and then exit 0, leaving a meta that declares a file no longer there. curl -f
# does not truncate the output file on a 404, so the delete was the whole damage.
#
# A required blob that is declared but not published is fatal and nothing is touched. Anything else
# missing is skipped, which is what the tolerance was for.
MISSING=""
FATAL=""
while IFS= read -r f; do
  [ -n "$f" ] || continue
  safe_name "$f" || { echo "refusing unsafe blob name: $f" >&2; exit 1; }
  code="$(curl -sL -m 20 -o /dev/null -w '%{http_code}' -r 0-0 "$BASE/$f" 2>/dev/null || echo 000)"
  case "$code" in
    200 | 206) continue ;;
  esac
  for r in $REQUIRED; do
    if [ "$f" = "$(python3 -c 'import json,sys; print(json.load(open("data/dataset.meta.json")).get(sys.argv[1], ""))' "${r}File")" ]; then
      FATAL="$FATAL $f($code)"
      continue 2
    fi
  done
  MISSING="$MISSING $f($code)"
done <<EOF
$FILES
EOF
[ -z "$FATAL" ] || {
  echo "release declares required blobs that are not published:$FATAL — refusing, ./data untouched" >&2
  exit 1
}
[ -z "$MISSING" ] || echo "declared but not in the release, skipping:$MISSING" >&2

# Newline-delimited via a redirect, so a name is never word-split or glob-expanded and `set -e` can
# still abort the script (a pipe would put this loop in a subshell).
while IFS= read -r f; do
  [ -n "$f" ] || continue
  case " $MISSING " in *" $f("*) continue ;; esac
  echo "fetching $f …"
  curl -fsSL "$BASE/$f" -o "data/$f"
done <<EOF
$FILES
EOF
echo "fetched → ./data:"
# `|| true`: an unmatched glob makes `ls` exit 2, and under `set -o pipefail` that failed the whole
# script AFTER a completely successful fetch — a release with no .gz was enough.
ls -la data/*.json data/*.bin data/*.gz 2>/dev/null | awk '{print $5, $NF}' || true
