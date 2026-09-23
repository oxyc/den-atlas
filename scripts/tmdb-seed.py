#!/usr/bin/env python3
"""Seed atlas's kept TMDB numbers (`src/tmdb.rs`) from den-dataset's own TMDB detail cache.

  scripts/tmdb-seed.py <den-dataset .cache/tmdb> <out dir>

Writes `tmdb-votes.tsv` and `tmdb-credits.tsv` in the layout `tmdb.rs` reads, for copying into the box's
CACHE_DIR by hand. Each entry's `fetched` is the cache file's mtime — when TMDB gave that body — so TMDB's six
months are counted from TMDB's answer, not from the copy, and a body already past them is left out.

NEVER publish what this writes, and never commit it: it is TMDB's data, and it may live on the box's own disk
only (the rules are at the top of `src/tmdb.rs`). The out dir is for a one-off copy onto the box.

Every body in the cache is seeded, not only the corpus's: atlas ignores a title its store does not hold, and
this needs no store to run. The cached details were asked with `append_to_response=keywords,credits`, so a
series' credits here are its `credits` (the latest season's regular cast); atlas's own refresh asks
`aggregate_credits` for a series, which replaces them over the following months.
"""
import json
import os
import sys
import time

MAX_AGE = 180 * 86400
#: `characters::BILLED`: the only billing positions atlas reads.
BILLED = int(os.environ.get("TMDB_SEED_BILLED", "10"))
VOTES_HEADER = "# den-atlas tmdb-votes v1\tmedia\tid\tvote_average\tvote_count\tfetched"
CREDITS_HEADER = "# den-atlas tmdb-credits v1\tmedia\tid\tfetched\torder\tperson\tcharacter"


def clean(text):
    return " ".join(text.replace("\t", " ").replace("\r", " ").replace("\n", " ").split())


def main(cache, out):
    now = time.time()
    votes, credits = [], []
    stats = {"bodies": 0, "unreadable": 0, "expired": 0, "votes": 0, "credits": 0, "roles": 0}
    for shard in sorted(os.listdir(cache)):
        folder = os.path.join(cache, shard)
        if not os.path.isdir(folder):
            continue
        for name in sorted(os.listdir(folder)):
            if not name.endswith(".json"):
                continue
            path = os.path.join(folder, name)
            try:
                fetched = int(os.path.getmtime(path))
                with open(path, "rb") as fh:
                    body = json.load(fh)
            except (OSError, ValueError):
                stats["unreadable"] += 1
                continue
            stats["bodies"] += 1
            if fetched + MAX_AGE <= now:
                stats["expired"] += 1
                continue
            if not isinstance(body, dict) or not isinstance(body.get("id"), int):
                stats["unreadable"] += 1
                continue
            media = "tv" if "first_air_date" in body or "number_of_seasons" in body else "movie"
            key = f"{media}\t{body['id']}"
            count, average = body.get("vote_count"), body.get("vote_average")
            if isinstance(count, int) and count > 0 and isinstance(average, (int, float)):
                votes.append(f"{key}\t{average}\t{count}\t{fetched}\n")
                stats["votes"] += 1
            cast = (body.get("credits") or {}).get("cast")
            if isinstance(cast, list):
                roles = []
                for member in cast:
                    if not isinstance(member, dict):
                        continue
                    order, person = member.get("order"), member.get("id")
                    character = clean(member.get("character") or "")
                    if isinstance(order, int) and isinstance(person, int) and order < BILLED and character:
                        roles.append(f"{key}\t{fetched}\t{order}\t{person}\t{character}\n")
                # A title with no named role is kept as one, so atlas does not ask for it again at once.
                credits.extend(roles or [f"{key}\t{fetched}\t\t\t\n"])
                stats["credits"] += 1
                stats["roles"] += len(roles)
    os.makedirs(out, exist_ok=True)
    for name, header, lines in (("tmdb-votes.tsv", VOTES_HEADER, votes),
                                ("tmdb-credits.tsv", CREDITS_HEADER, credits)):
        with open(os.path.join(out, name), "w", encoding="utf-8") as fh:
            fh.write(header + "\n")
            fh.writelines(lines)
    print(json.dumps(stats))


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
