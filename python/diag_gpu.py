"""
diag_gpu.py — bisect a cuGraph ForceAtlas2 segfault.

Run:  python python/diag_gpu.py

Each step prints BEFORE it runs and flushes immediately, because a segfault
kills the process outright — the last line printed is the step that crashed.

The steps escalate from "plain cuGraph on a toy graph" to the exact
configuration `_layout_gpu` uses, so whichever line is last tells us whether
the fault is in the environment or in our setup.
"""

import sys


def step(n, what):
    print(f"\n[{n}] {what}", flush=True)


def ok(msg=""):
    print(f"    OK {msg}", flush=True)


step(0, "versions")
import numpy as np  # noqa: E402

print(f"    numpy   {np.__version__}", flush=True)
for mod in ("cudf", "cugraph", "rmm", "cupy", "numba", "cuda", "scipy"):
    try:
        m = __import__(mod)
        print(f"    {mod:<7} {getattr(m, '__version__', '?')}", flush=True)
    except Exception as e:  # noqa: BLE001
        print(f"    {mod:<7} MISSING ({type(e).__name__})", flush=True)

import cudf  # noqa: E402
import cugraph  # noqa: E402
import rmm  # noqa: E402

import os  # noqa: E402
import subprocess  # noqa: E402

try:
    out = subprocess.check_output(
        ["nvidia-smi", "--query-gpu=name,driver_version,memory.total",
         "--format=csv,noheader"],
        stderr=subprocess.DEVNULL,
    ).decode().strip()
    print(f"    gpu     {out}", flush=True)
except Exception:  # noqa: BLE001
    print("    gpu     nvidia-smi unavailable", flush=True)

try:
    from numba import cuda as nbcuda

    print(f"    numba sees {len(nbcuda.gpus)} GPU(s)", flush=True)
    # cuGraph's FA2 wrapper reaches back into Python and through ctypes into
    # cuCtxGetDevice — that is numba's driver binding. cudf and cuGraph create
    # their own C++ CUDA context, so numba's may never have been initialized.
    # NUMBA_INIT=1 forces one first, to test whether that is the fault.
    if os.environ.get("NUMBA_INIT"):
        ctx = nbcuda.current_context()
        print(f"    numba context forced: device {ctx.device.id}", flush=True)
    else:
        print("    numba context NOT initialized (set NUMBA_INIT=1 to force)", flush=True)
except Exception as e:  # noqa: BLE001
    print(f"    numba.cuda unavailable: {type(e).__name__}: {e}", flush=True)

# A tiny ring graph: 5 vertices, enough for FA2 to do real work.
SRC = [0, 1, 2, 3, 4, 0]
DST = [1, 2, 3, 4, 0, 2]
N = 5


def edgelist_graph():
    df = cudf.DataFrame(
        {"src": cudf.Series(SRC, dtype="int32"), "dst": cudf.Series(DST, dtype="int32")}
    )
    g = cugraph.Graph(directed=False)
    g.from_cudf_edgelist(df, source="src", destination="dst", renumber=False)
    return g


step(1, "cudf on the GPU at all")
print(f"    sum = {cudf.Series([1, 2, 3]).sum()}", flush=True)
ok()

step(2, "cugraph.pagerank on a toy graph (does any cuGraph algo run?)")
g = cugraph.Graph(directed=True)
g.from_cudf_edgelist(
    cudf.DataFrame(
        {"src": cudf.Series(SRC, dtype="int32"), "dst": cudf.Series(DST, dtype="int32")}
    ),
    source="src",
    destination="dst",
    renumber=False,
)
print(f"    pagerank rows = {len(cugraph.pagerank(g))}", flush=True)
ok()

step(3, "force_atlas2 on a toy graph, plain edgelist, NO rmm reinit")
pos = cugraph.force_atlas2(edgelist_graph(), max_iter=10)
print(f"    positions = {len(pos)}", flush=True)
ok("<-- if we get here, FA2 itself works in this environment")

step(4, "force_atlas2 with barnes_hut_optimize=True (what we actually pass)")
pos = cugraph.force_atlas2(
    edgelist_graph(),
    max_iter=10,
    barnes_hut_optimize=True,
    outbound_attraction_distribution=True,
    lin_log_mode=False,
    verbose=False,
)
print(f"    positions = {len(pos)}", flush=True)
ok()

step(5, "rmm.reinitialize(managed_memory=True), then force_atlas2 again")
rmm.reinitialize(managed_memory=True)
print("    rmm reinitialized", flush=True)
pos = cugraph.force_atlas2(edgelist_graph(), max_iter=10, barnes_hut_optimize=True)
print(f"    positions = {len(pos)}", flush=True)
ok("<-- if this crashes, managed memory / context teardown is the culprit")

step(6, "CSR adjlist + directed flip + patched nodes() — the exact _layout_gpu setup")
import scipy.sparse as sp  # noqa: E402

coo = sp.coo_matrix((np.ones(len(SRC), dtype=np.int8), (SRC, DST)), shape=(N, N))
csr = coo.tocsr()
offsets = cudf.Series(csr.indptr.astype(np.int32))
indices = cudf.Series(csr.indices.astype(np.int32))
G = cugraph.Graph(directed=True)
G.from_cudf_adjlist(offsets, indices, None)
G.graph_properties.directed = False
assert not G.is_directed()
nodes = cudf.Series(np.arange(N, dtype=np.int32), name="vertex")
if hasattr(G, "_nodes"):
    G._nodes = nodes
G.nodes = lambda: nodes
print("    graph built from CSR, flagged undirected, nodes() patched", flush=True)
pos = cugraph.force_atlas2(G, max_iter=10, barnes_hut_optimize=True)
print(f"    positions = {len(pos)}", flush=True)
ok("<-- full _layout_gpu configuration works on a toy graph")

print("\nAll steps passed. FA2 works here; the fault is scale-dependent.", flush=True)
sys.exit(0)
