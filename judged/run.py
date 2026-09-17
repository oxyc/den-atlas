#!/usr/bin/env python3
"""Run the judged query set against a live atlas and print pass/fail per assertion.

`search.rs` says the scoring weights are "starting values, to be tuned against a judged query set". This is
that set. It asserts a handful of things per query rather than a full ranked ideal: a ranked ideal is
expensive to author, goes stale on every corpus rebuild, and mostly tests agreement about the middle of the
list, where nobody looks.

  judged/run.py --base <atlas config base url> [--split dev|test|all] [--only <substring>] [-v]

Exit 1 if any case fails, so it can gate a weight change. DEFAULT SPLIT IS DEV: tuning against `test` is how
a weight set learns its own answers, and `build_bakeoff.py` already holds that line for the recommender.

A case whose `observed` is FAIL is a bug this set is meant to hold us to — it is expected to be red until
someone fixes the thing it describes, and going green is the signal that they did.
"""
import argparse
import json
import os
import sys
import urllib.parse
import urllib.request


def fetch(base, q, limit=40):
    url = f"{base.rstrip('/')}/index/query.json?" + urllib.parse.urlencode({"q": q, "limit": limit})
    req = urllib.request.Request(url, headers={"User-Agent": "den-judged-queries"})
    with urllib.request.urlopen(req, timeout=60) as fh:
        return json.load(fh).get("hits", [])


def key(hit):
    """`series:1438` — the type name atlas emits, not the one the dataset uses."""
    return f"{hit.get('type')}:{hit.get('id')}"


def check(case, hits):
    """Every assertion in one case. Returns (failures, checked_count)."""
    a, keys, bad = case.get("assert", {}), [key(h) for h in hits], []
    for k, want in a.get("rank_at_most", {}).items():
        at = keys.index(k) + 1 if k in keys else None
        if at is None or at > want:
            bad.append(f"{k} wanted rank <= {want}, got {at or 'absent'}")
    for k, n in a.get("in_top", {}).items():
        if k not in keys[:n]:
            at = keys.index(k) + 1 if k in keys else None
            bad.append(f"{k} wanted in top {n}, got {at or 'absent'}")
    for k, n in a.get("not_in_top", {}).items():
        if k in keys[:n]:
            bad.append(f"{k} must NOT be in top {n}, is at {keys.index(k) + 1}")
    if (m := a.get("min_hits")) and len(hits) < m:
        bad.append(f"wanted >= {m} hits, got {len(hits)}")
    if m := a.get("min_scored"):
        scored = sum(1 for h in hits if (h.get("score") or 0) > 0)
        if scored < m:
            bad.append(f"wanted >= {m} SCORED hits, got {scored} of {len(hits)} (rest is popularity padding)")
    if t := a.get("all_type"):
        off = {h.get("type") for h in hits} - {t}
        if off:
            bad.append(f"every hit should be {t}; also saw {sorted(off)}")
    return bad, sum(len(a.get(f, {})) if isinstance(a.get(f), dict) else bool(a.get(f))
                    for f in ("rank_at_most", "in_top", "not_in_top", "min_hits", "min_scored", "all_type"))


ap = argparse.ArgumentParser()
ap.add_argument("--base", default=os.environ.get("ATLAS_BASE"),
                help="e.g. https://…/atlas/<config>  (or set ATLAS_BASE)")
ap.add_argument("--split", default="dev", choices=["dev", "test", "all"])
ap.add_argument("--only", help="run only cases whose query contains this")
ap.add_argument("-v", "--verbose", action="store_true", help="print the top hits for every case")
args = ap.parse_args()
if not args.base:
    sys.exit("need --base (or ATLAS_BASE)")

here = os.path.dirname(os.path.abspath(__file__))
cases = json.load(open(os.path.join(here, "queries.json"), encoding="utf-8"))["cases"]
cases = [c for c in cases if args.split in (c["split"], "all")]
if args.only:
    cases = [c for c in cases if args.only in c["q"]]

failed = unjudged = passed = 0
for c in cases:
    if not c.get("assert"):
        print(f"  ????  {c['q']!r}  — no assertions yet ({c['note'].split('.')[0]})")
        unjudged += 1
        continue
    try:
        hits = fetch(args.base, c["q"])
    except Exception as exc:
        print(f"  ERR   {c['q']!r}  {exc}")
        failed += 1
        continue
    bad, n = check(c, hits)
    mark = "FAIL" if bad else "ok  "
    known = "  (known)" if bad and c.get("observed") == "FAIL" else ""
    print(f"  {mark}  {c['q']!r}  [{c['lane']}] {n} assertions, {len(hits)} hits{known}")
    for b in bad:
        print(f"          {b}")
    if args.verbose:
        for h in hits[:5]:
            print(f"          . {key(h):14} {str(h.get('title'))[:40]:42} s={h.get('score')}")
    failed += bool(bad)
    passed += not bad

print(f"\n{passed} passed, {failed} failed, {unjudged} unjudged  (split={args.split})")
sys.exit(1 if failed else 0)
