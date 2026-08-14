"""
02_video_stats.py — Graph topology statistics.

Orphans, dead ends, and fully isolated articles, computed from the integer
edge list with numpy bincounts.

These numbers are only meaningful because the parser drops red links. When
link targets that name no article were kept as nodes, every red link counted
as a "dead end", so that statistic measured nothing but the parser's own
sloppiness.
"""

import argparse
import os
import sys
import time

import numpy as np
import pandas as pd
import pyarrow.parquet as pq

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import Paths, load_config  # noqa: E402


def main():
    ap = argparse.ArgumentParser(description="Graph topology statistics")
    ap.add_argument("--data-dir", default=None, help="override pipeline.data_dir")
    args = ap.parse_args()

    t0 = time.time()
    cfg = load_config()
    paths = Paths(args.data_dir or cfg["pipeline"]["data_dir"])

    if not os.path.exists(paths.nodes):
        raise SystemExit(
            f"{paths.nodes} not found. Run: python python/01_graph_compute.py"
        )

    nodes = pd.read_parquet(paths.nodes)
    n = len(nodes)
    print(f"Total articles: {n:,}\n")

    tbl = pq.read_table(paths.edges, columns=["src", "dst"])
    src = tbl.column("src").to_numpy()
    dst = tbl.column("dst").to_numpy()
    del tbl

    outdeg = np.bincount(src, minlength=n)
    indeg = np.bincount(dst, minlength=n)

    orphans = indeg == 0
    dead_ends = outdeg == 0
    isolated = orphans & dead_ends

    def line(label, mask):
        c = int(mask.sum())
        print(f"  {label:<36} {c:>10,}  ({100 * c / n:5.2f}%)")

    print(f"  {'Edges':<36} {len(src):>10,}")
    print(f"  {'Average out-degree':<36} {len(src) / n:>10.1f}")
    line("Orphans (no inbound links)", orphans)
    line("Dead ends (no outbound links)", dead_ends)
    line("Fully isolated (neither)", isolated)

    # Reciprocity: how often A->B is matched by B->A.
    key = (src.astype(np.int64) << 32) | dst.astype(np.int64)
    rev = (dst.astype(np.int64) << 32) | src.astype(np.int64)
    mutual = np.isin(rev, key, assume_unique=False).sum()
    print(f"  {'Reciprocated links':<36} {mutual:>10,}  ({100 * mutual / len(src):5.2f}%)")

    # Orphans cannot be ranked by PageRank: with no inbound links their score
    # is exactly the teleport constant (1-alpha)/n for every one of them, so
    # "top orphans by PageRank" is an arbitrary tie-break over identical
    # values. Out-degree is the meaningful ranking — these are articles that
    # reference the encyclopedia heavily while nothing references them back.
    nodes = nodes.assign(out_degree=outdeg)
    print("\nTop orphans by out-degree (link out heavily, nobody links back):")
    orphan_nodes = nodes[orphans].nlargest(10, "out_degree")
    for _, r in orphan_nodes.iterrows():
        print(f"   {r['vertex'][:52]:<52} out={int(r['out_degree']):,}")

    print("\nTop dead ends by PageRank (link to nobody, yet important):")
    dead_nodes = nodes[dead_ends].nlargest(10, "pagerank")
    for _, r in dead_nodes.iterrows():
        print(f"   {r['vertex'][:52]:<52} PR={r['pagerank']:.2e}")

    print(f"\nCompleted in {time.time() - t0:.1f}s")


if __name__ == "__main__":
    main()
