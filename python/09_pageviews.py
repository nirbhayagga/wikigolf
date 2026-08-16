"""
09_pageviews.py — join Wikimedia pageview counts onto the article ids.

The graph knows what Wikipedia *links to*. It has no idea what anyone
actually reads, and the gap between those two is the most interesting thing
in the dataset: "Moth" is the 21st most linked-to article on English
Wikipedia, on 81,516 inbound links, because of tens of thousands of
moth-species stubs. Nobody reads it.

Wikimedia publishes a monthly per-article view count. It is a small download
against a 27 GB dump and it is the only outside data this project uses.

    python python/09_pageviews.py --month 2026-07
    python python/09_pageviews.py --month 2026-07 --data-dir data/resrun

Writes pageviews.parquet: id (dense article id), views (u32).

Titles are matched through the parser's own normalization and redirect map,
so "USA" folds into "United States" exactly as it does in the graph — the
dump's own alias table is the authority, not a guess.
"""

import argparse
import bz2
import os
import sys
import time
import urllib.request

import numpy as np
import pandas as pd

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import Paths, load_config  # noqa: E402

# The "pageview complete" monthly dumps: one line per article, already
# aggregated, ~100 MB compressed against 27 GB for the content dump.
URL = ("https://dumps.wikimedia.org/other/pageview_complete/monthly/"
       "{year}/{year}-{month}/pageviews-{year}{month}-user.bz2")


def normalize(title):
    """The parser's rules, in the few places they matter for a lookup."""
    t = title.replace("_", " ").strip()
    if not t:
        return None
    # ucfirst, but only when the uppercase mapping is a single character —
    # Python's .upper() turns ß into SS, which would merge distinct articles.
    head = t[0]
    up = head.upper()
    if len(up) == 1:
        t = up + t[1:]
    return t


def main():
    ap = argparse.ArgumentParser(description="Join pageview counts onto article ids")
    ap.add_argument("--month", required=True, help="YYYY-MM, e.g. 2026-07")
    ap.add_argument("--data-dir", default=None, help="override pipeline.data_dir")
    ap.add_argument("--cache", default=None,
                    help="path to an already-downloaded dump (skips the fetch)")
    args = ap.parse_args()

    cfg = load_config()
    paths = Paths(args.data_dir or cfg["pipeline"]["data_dir"])
    year, month = args.month.split("-")

    t0 = time.time()
    titles = pd.read_parquet(paths.titles)
    print(f"{len(titles):,} articles")

    # id lookup by normalized title, plus every redirect alias pointing at one.
    # Without aliases a large share of real traffic lands on titles the graph
    # does not contain and is silently dropped.
    lookup = {}
    for i, t in zip(titles["id"].to_numpy(), titles["title"].to_numpy()):
        n = normalize(t)
        if n:
            lookup[n] = i
    if os.path.exists(paths.redirects):
        red = pd.read_parquet(paths.redirects)
        for a, i in zip(red["alias"].to_numpy(), red["article_id"].to_numpy()):
            n = normalize(a)
            if n and n not in lookup:
                lookup[n] = i
        print(f"  + {len(red):,} redirect aliases")

    src = args.cache
    if not src:
        url = URL.format(year=year, month=month)
        src = os.path.join(paths.data_dir, f"pageviews-{year}{month}.bz2")
        if not os.path.exists(src):
            print(f"downloading {url}")
            urllib.request.urlretrieve(url, src)
        else:
            print(f"using cached {src}")

    views = np.zeros(len(titles), dtype=np.int64)
    seen = matched = 0
    with bz2.open(src, "rt", encoding="utf-8", errors="replace") as f:
        for line in f:
            # domain title views agent_breakdown
            parts = line.split(" ")
            if len(parts) < 3 or parts[0] != "en.wikipedia":
                continue
            seen += 1
            n = normalize(parts[1])
            if n is None:
                continue
            i = lookup.get(n)
            if i is None:
                continue
            try:
                views[i] += int(parts[2])
            except ValueError:
                continue
            matched += 1
            if matched % 1_000_000 == 0:
                print(f"  matched {matched:,}…", flush=True)

    out = os.path.join(paths.data_dir, "pageviews.parquet")
    pd.DataFrame({
        "id": np.arange(len(titles), dtype=np.uint32),
        "views": np.clip(views, 0, np.iinfo(np.uint32).max).astype(np.uint32),
    }).to_parquet(out, compression="zstd", index=False)

    nz = views > 0
    print(f"\n{matched:,} of {seen:,} en.wikipedia rows matched an article "
          f"({100 * matched / max(seen, 1):.1f}%)")
    print(f"{nz.sum():,} articles have views ({100 * nz.mean():.1f}%)")
    print(f"total views: {views.sum():,}")
    print(f"→ {out}  in {time.time() - t0:.0f}s")

    # The headline the graph alone cannot produce: heavily linked, barely read.
    order = np.argsort(-views)
    names = titles["title"].to_numpy()
    print("\nmost read:")
    for i in order[:10]:
        print(f"  {views[i]:>12,}  {names[i]}")


if __name__ == "__main__":
    main()
