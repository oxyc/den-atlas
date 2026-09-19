#!/usr/bin/env python3
"""Walk the unjudged cases, show what the index returns, and write the assertions you pick.

`queries.json` holds assertions, not graded relevance — but authoring one still means looking at real
results and deciding which reading of an ambiguous query is right. That decision is taste and belongs to a
person; this just removes the JSON editing around it.

  judged/judge.py --base <atlas config base url> [--only <substring>] [--all]

By default it visits only cases with no assertions yet. `--all` revisits judged ones too, so a case can be
re-judged when the corpus or the weights move. Nothing is written until a case is finished, and pressing
enter at every prompt leaves that case exactly as it was.
"""
import argparse
import json
import os
import sys
import urllib.parse
import urllib.request


def fetch(base, q, limit=12):
    url = f"{base.rstrip('/')}/index/query.json?" + urllib.parse.urlencode({"q": q, "limit": limit})
    req = urllib.request.Request(url, headers={"User-Agent": "den-judged-queries"})
    with urllib.request.urlopen(req, timeout=60) as fh:
        return json.load(fh).get("hits", [])


def why(hit):
    """Plain words for how a hit got here, from the per-signal breakdown atlas returns.

    Nobody can judge a result without knowing what matched. A hit that scored on nothing is the important
    case: `search.rs` says "a candidate relevant to nothing is dropped", and these are not being dropped —
    they are thin result sets padded out with popular titles, which is a different bug from a bad ranking.
    """
    f, parts = hit.get("f", {}), []
    for field, word in (("t", "title"), ("sem", "plot-vector"), ("lab", "label"),
                        ("pf", "plot-facet"), ("p", "person")):
        if f.get(field):
            parts.append(f"{word} {f[field]}")
    return ", ".join(parts) if parts else "NOTHING — popularity padding"


def link(hit):
    """A clickable row. TMDB always works (we have type + id); IMDb only when the index carries the tconst."""
    kind = "tv" if hit.get("type") == "series" else "movie"
    out = f"https://www.themoviedb.org/{kind}/{hit.get('id')}"
    if imdb := hit.get("imdbId"):
        out += f"   https://www.imdb.com/title/{imdb}/"
    return out


def ask(prompt):
    try:
        return input(prompt).strip()
    except EOFError:
        raise SystemExit("\nnothing written")


def picks(raw, hits):
    """'1,3' or '1-4' or '1-3,7' -> the keys of those hits. Out-of-range entries are ignored, not fatal."""
    want = []
    for part in raw.replace(" ", "").split(","):
        if "-" in part:
            lo, _, hi = part.partition("-")
            if lo.isdigit() and hi.isdigit():
                want.extend(range(int(lo), int(hi) + 1))
        elif part.isdigit():
            want.append(int(part))
    return [f"{hits[i - 1].get('type')}:{hits[i - 1].get('id')}" for i in want if 1 <= i <= len(hits)]


ap = argparse.ArgumentParser()
ap.add_argument("--base", default=os.environ.get("ATLAS_BASE"))
ap.add_argument("--only", help="only cases whose query contains this")
ap.add_argument("--all", action="store_true", help="revisit cases that already have assertions")
args = ap.parse_args()
if not args.base:
    sys.exit("need --base (or ATLAS_BASE)")

here = os.path.dirname(os.path.abspath(__file__))
path = os.path.join(here, "queries.json")
doc = json.load(open(path, encoding="utf-8"))
todo = [c for c in doc["cases"] if args.all or not c.get("assert")]
if args.only:
    todo = [c for c in todo if args.only in c["q"]]
if not todo:
    raise SystemExit("nothing to judge")

print(f"{len(todo)} case(s). Enter alone skips a question; ctrl-D quits without writing.\n")
changed = 0
for case in todo:
    print("=" * 78)
    print(f"  {case['q']!r}   [{case['lane']}]")
    print(f"  {case['note']}")
    try:
        hits = fetch(args.base, case["q"])
    except Exception as exc:
        print(f"  fetch failed: {exc}\n")
        continue
    if not hits:
        print("  no results at all — that is itself the finding; skipping\n")
        continue
    print()
    for i, h in enumerate(hits, 1):
        f = h.get("f", {})
        lanes = " ".join(f"{k}={v}" for k, v in f.items() if v and k != "phi")
        year = str(h.get("year") or "")
        print(f"   {i:>2}. {h.get('type'):6} {str(h.get('title'))[:38]:40} {year:>4}"
              f"  s={h.get('score')}  {lanes}")
        print(f"       {link(h)}")
    print()

    # Easy judgements first. "Which is #1" is the hardest question to answer and rarely has one answer,
    # so it is optional and last; pointing at what belongs and what does not is what a person can do quickly.
    a = {}
    print("  Answer with numbers: 1,3  or  1-4  or  1-3,7. Enter alone skips.")
    if raw := ask("  Which of these BELONG here? (they must stay in the top 10) > "):
        if got := picks(raw, hits):
            a["in_top"] = {k: 10 for k in got}
    if raw := ask("  Which are WRONG? (they must not be in the top 5)      > "):
        if got := picks(raw, hits):
            a["not_in_top"] = {k: 5 for k in got}
    if raw := ask("  Is one of them clearly THE answer? (number, or enter) > "):
        if got := picks(raw, hits):
            a["rank_at_most"] = {got[0]: 1}
            a.get("in_top", {}).pop(got[0], None)   # implied by rank 1; keeping both is noise
            if not a.get("in_top"):
                a.pop("in_top", None)
    if raw := ask("  How many results should actually SCORE? (or enter)    > "):
        if raw.isdigit():
            a["min_scored"] = int(raw)
    if raw := ask("  A note on why (enter to keep the existing one) > "):
        case["note"] = raw

    if a:
        case["assert"] = a
        case["observed"] = "unjudged"   # run.py decides pass/fail; this is no longer a guess
        changed += 1
        print(f"  -> {json.dumps(a)}\n")
    else:
        print("  -> left unjudged\n")

if not changed:
    raise SystemExit("nothing written")
with open(path, "w", encoding="utf-8") as fh:
    json.dump(doc, fh, indent=2, ensure_ascii=False)
    fh.write("\n")
print(f"wrote {changed} case(s) to {path}")
print("now run:  judged/run.py --base <base>")
