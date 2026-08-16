"""Export the high-PageRank core as Gephi CSVs, to test the LinLog hypothesis.

Four graph sizes have now been laid out with graph-tool SFDP — 36k, 72k,
722k and 7.2M nodes — and all four came out as featureless discs. The one
thing every published Wikipedia map does that we have never done is
ForceAtlas2 with **LinLog mode**, which changes the attraction term
specifically to separate clusters. graph-tool has no equivalent, and
sweeping C and p did not substitute for it.

36k nodes is small enough to lay out interactively in Gephi, so this is a
one-afternoon test of whether the algorithm is the difference.

Writes Gephi's expected column names (Id/Label/Source/Target) so both files
import with no column mapping.
"""
import numpy as np
import pandas as pd

DATA = "data"
OUT = "data/gephi"
FRAC = 0.005
# Gephi slows badly past a few million edges. Keeping only the strongest
# reciprocal structure would change what is being tested, so instead cap by
# taking the densest core and, if needed, thinning by endpoint popularity.
MAX_EDGES = 3_000_000

import os
os.makedirs(OUT, exist_ok=True)

pr = pd.read_parquet(f"{DATA}/cache_metrics.parquet")["pagerank"].to_numpy()
n = len(pr)
k = int(n * FRAC)
cut = np.partition(pr, -k)[-k]
core = pr >= cut
ids = np.flatnonzero(core)
print(f"core: {len(ids):,} of {n:,} articles (top {100*FRAC:.1f}% by PageRank)")

e = pd.read_parquet(f"{DATA}/edges.parquet")
src = e.src.to_numpy(np.int32)
dst = e.dst.to_numpy(np.int32)
del e
keep = core[src] & core[dst]
u, v = src[keep], dst[keep]
del src, dst
print(f"induced edges: {len(u):,}")

if len(u) > MAX_EDGES:
    # Thin deterministically by a hash of the pair rather than at random, so
    # the export is reproducible, and never drop an article entirely.
    h = ((u.astype(np.uint64) * np.uint64(0x9E3779B97F4A7C15))
         ^ (v.astype(np.uint64) * np.uint64(0xBF58476D1CE4E5B9)))
    thresh = np.uint64(float(MAX_EDGES) / len(u) * float(np.iinfo(np.uint64).max))
    m = h < thresh
    u, v = u[m], v[m]
    print(f"thinned to {len(u):,} edges for Gephi (cap {MAX_EDGES:,})")

titles = pd.read_parquet(f"{DATA}/titles.parquet")["title"].to_numpy()
nodes_src = f"{DATA}/fulloutput/nodes.parquet"
if os.path.exists(nodes_src):
    com = pd.read_parquet(nodes_src, columns=["community"])["community"].to_numpy()
else:
    com = np.zeros(n, np.int32)

indeg = np.bincount(np.concatenate([v]), minlength=n)

pd.DataFrame({
    "Id": ids,
    "Label": titles[ids],
    "pagerank": pr[ids],
    "community": com[ids],
    "indegree_in_core": indeg[ids],
}).to_csv(f"{OUT}/nodes.csv", index=False)

pd.DataFrame({"Source": u, "Target": v, "Type": "Directed"}).to_csv(
    f"{OUT}/edges.csv", index=False)

print(f"\nwrote {OUT}/nodes.csv  ({len(ids):,} rows)")
print(f"wrote {OUT}/edges.csv  ({len(u):,} rows)")
print("\ntop 10 of the core by PageRank:")
order = ids[np.argsort(-pr[ids])][:10]
for i in order:
    print(f"   {titles[i]}")
