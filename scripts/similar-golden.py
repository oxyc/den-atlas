#!/usr/bin/env python3
"""Recapture src/similar-golden.json from a running atlas.

  scripts/similar-golden.py http://127.0.0.1:8080 "<why it was recaptured>"

Asks the atlas for each anchor's whole More Like This row of its own type — the playground's route with
mix_types=0, so the atlas needs PLAYGROUND on (GET /playground/similar/<type>/<id>.json?limit=200&mix_types=0)
— and its dataset's version (GET /dataset.json), and rewrites the golden with them. The golden is of one
type; the test holds a mixed row's own-type titles to it. The anchors are the ones already in the file. Run it
against a binary built from the commit you mean to pin, serving the store the golden is for, and say in the
second argument what changed.
"""
import json
import pathlib
import sys
import urllib.request

base, why = sys.argv[1].rstrip("/"), sys.argv[2]
path = pathlib.Path(__file__).resolve().parent.parent / "src" / "similar-golden.json"


def get(route):
    with urllib.request.urlopen(base + route) as resp:
        return json.load(resp)


golden = json.loads(path.read_text())
anchors = []
for anchor in golden["anchors"]:
    row = get(f"/playground/similar/{anchor['type']}/{anchor['id']}.json?limit=200&mix_types=0")
    ids = [t["id"] for t in row["titles"]]
    if not ids:
        sys.exit(f"{anchor['name']}: the atlas answered an empty row; is it serving the right store?")
    anchors.append({"name": anchor["name"], "type": anchor["type"], "id": anchor["id"], "ids": ids})

out = {
    "about": "More Like This rows of one type for these anchors, GET /playground/similar/<type>/<id>.json"
    f"?limit=200&mix_types=0 on the store named by datasetVersion, captured by scripts/similar-golden.py. {why}",
    "datasetVersion": get("/dataset.json")["datasetVersion"],
    "anchors": anchors,
}
path.write_text(json.dumps(out, ensure_ascii=False, separators=(",", ":")))
print(f"wrote {path}: {len(anchors)} anchors, {sum(len(a['ids']) for a in anchors)} ids")
