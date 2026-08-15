"""
Wikipedia Graph Compute Pipeline
=================================
Consumes the Rust parser's output directly — there is no CSV ingest and no
string→integer mapping step, because the parser already resolved article
identity and emitted dense int32 ids.

Phase 1: PageRank + in-degree on the DIRECTED graph   (GPU cuGraph / CPU scipy)
Phase 2: ForceAtlas2 layout + Leiden communities on the UNDIRECTED graph
                                                      (GPU cuGraph / CPU igraph)
Phase 3: Merge and attach article titles

Directed vs undirected is deliberate: link direction carries importance
(PageRank), but force layout and community structure are about mutual
connectivity, so those run on the symmetrized graph.

  --sample 0.01   Subsample edges for a smoke test. NOTE: this shatters the
                  graph into disconnected fragments, so it cannot validate
                  layout or community quality. To validate those, run the
                  whole pipeline on Simple English Wikipedia instead.
  --reset         Delete caches and recompute.
"""

import argparse
import gc
import os
import sys
import time
import warnings

warnings.filterwarnings("ignore", category=FutureWarning)
warnings.filterwarnings("ignore", category=UserWarning)

import numpy as np
import pandas as pd
import pyarrow.parquet as pq

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from common import (  # noqa: E402
    Paths,
    check_manifest,
    elapsed,
    free_gpu_memory,
    load_config,
    log_phase,
    reset_caches,
    run_with_timer,
    vram_status,
)


def load_edges(paths, sample_ratio=1.0):
    """Load the int32 edge list as two numpy arrays."""
    tbl = pq.read_table(paths.edges, columns=["src", "dst"])
    src = tbl.column("src").to_numpy()
    dst = tbl.column("dst").to_numpy()
    del tbl

    if sample_ratio < 1.0:
        rng = np.random.default_rng(42)
        keep = rng.random(len(src)) < sample_ratio
        src, dst = src[keep], dst[keep]
        print(f"   Sampled {sample_ratio * 100:.1f}%: {len(src):,} edges")

    return src, dst


# ---------------------------------------------------------------------------
# Phase 1 — PageRank + in-degree (directed)
# ---------------------------------------------------------------------------

def _metrics_gpu(src, dst, n, cfg):
    import cudf
    import cugraph

    print("   [GPU] Loading edges into VRAM...")
    gdf = cudf.DataFrame({"src": src, "dst": dst})
    vram_status()

    G = cugraph.Graph(directed=True)
    G.from_cudf_edgelist(
        gdf,
        source="src",
        destination="dst",
        renumber=False,
        store_transposed=cfg["gpu"]["store_transposed"],
    )
    del gdf
    gc.collect()
    vram_status()

    # .to_arrow().to_pandas() rather than .to_pandas(): the Arrow bridge
    # sidesteps Numba driver bugs on GeForce cards.
    print("   [GPU] In-degree...")
    indeg = G.in_degree().to_arrow().to_pandas()

    print("   [GPU] PageRank...")
    pr = cugraph.pagerank(G).to_arrow().to_pandas()

    del G
    gc.collect()
    free_gpu_memory()
    vram_status()

    degree = np.zeros(n, dtype=np.int32)
    col = "degree" if "degree" in indeg.columns else "in_degree"
    degree[indeg["vertex"].to_numpy()] = indeg[col].to_numpy()

    pagerank = np.zeros(n, dtype=np.float64)
    pagerank[pr["vertex"].to_numpy()] = pr["pagerank"].to_numpy()
    return pagerank, degree


def _metrics_cpu(src, dst, n, alpha=0.85, tol=1e-9, max_iter=100):
    """PageRank by sparse power iteration.

    The previous pipeline had no CPU path for this phase at all — a missing
    cuGraph meant an ImportError partway through. One CSR of the edge list is
    ~3 GB at full enwiki scale, which is well within reach without a GPU.
    """
    import scipy.sparse as sp

    print("   [CPU] Building transition matrix...")
    outdeg = np.bincount(src, minlength=n).astype(np.float64)
    degree = np.bincount(dst, minlength=n).astype(np.int32)

    # Invert per-vertex first, then gather. `np.where(outdeg[src] > 0, 1/outdeg[src], 0)`
    # is the obvious spelling but materializes three arrays the size of the
    # edge list — 5.5 GB at enwiki scale, before the matrix even exists.
    # Inverting the 7.2M-element vector costs nothing and leaves one gather.
    inv = np.zeros(n, dtype=np.float64)
    nz = outdeg > 0
    inv[nz] = 1.0 / outdeg[nz]
    w = inv[src]
    del inv, nz
    # M[i, j] = probability of stepping j -> i
    M = sp.csr_matrix((w, (dst, src)), shape=(n, n))
    del w

    dangling = outdeg == 0
    r = np.full(n, 1.0 / n)
    teleport = (1.0 - alpha) / n

    print(f"   [CPU] Power iteration (alpha={alpha}, tol={tol})...")
    with run_with_timer("PageRank"):
        for it in range(max_iter):
            r_next = alpha * (M @ r + r[dangling].sum() / n) + teleport
            err = np.abs(r_next - r).sum()
            r = r_next
            if err < tol:
                break
    print(f"   Converged after {it + 1} iterations (L1 delta {err:.2e})")
    return r, degree


def phase1_metrics(paths, cfg, n, sample_ratio):
    if os.path.exists(paths.cache_metrics):
        print("-> Cached metrics found. Skipping Phase 1.")
        return

    t0 = time.time()
    log_phase("Phase 1: PageRank + In-Degree (directed)")
    src, dst = load_edges(paths, sample_ratio)
    print(f"   Vertices: {n:,}  Edges: {len(src):,}")

    backend = cfg["layout"].get("backend", "auto")
    pagerank = degree = None
    if backend in ("auto", "gpu"):
        try:
            pagerank, degree = _metrics_gpu(src, dst, n, cfg)
        except Exception as e:
            if backend == "gpu":
                raise
            print(f"\n   GPU path unavailable ({type(e).__name__}: {e})")
            print("   Falling back to CPU.")
            free_gpu_memory()
    if pagerank is None:
        pagerank, degree = _metrics_cpu(src, dst, n)

    del src, dst
    gc.collect()

    pd.DataFrame(
        {
            "vertex": np.arange(n, dtype=np.int32),
            "pagerank": pagerank.astype(np.float64),
            "degree": degree.astype(np.int32),
        }
    ).to_parquet(paths.cache_metrics, compression="zstd", index=False)
    print(f"   Phase 1 complete in {elapsed(t0)}")


# ---------------------------------------------------------------------------
# Phase 2 — layout + communities (undirected)
# ---------------------------------------------------------------------------

def symmetrize(src, dst, n):
    """Build the undirected edge set on the CPU.

    cuGraph symmetrizes internally when given an undirected graph, and that
    peak is what exhausted 16 GB of VRAM. Doing it here costs system RAM,
    which is plentiful, and lets the GPU receive a graph it can just use.
    """
    print("   Symmetrizing on CPU...")
    t = time.time()
    a = np.concatenate([src, dst])
    b = np.concatenate([dst, src])
    # Deduplicate (u, v) pairs by packing them into one int64 key.
    key = (a.astype(np.int64) << 32) | b.astype(np.int64)
    del a, b
    key = np.unique(key)
    u = (key >> 32).astype(np.int32)
    v = (key & 0xFFFFFFFF).astype(np.int32)
    del key
    mask = u != v
    u, v = u[mask], v[mask]
    print(f"   Symmetric edges: {len(u):,} (from {len(src):,} directed) in {elapsed(t)}")
    return u, v


def _layout_gpu(u, v, n, cfg):
    """cuGraph ForceAtlas2 + Leiden on a pre-symmetrized graph.

    The graph is handed over as a CSR built on the CPU and loaded as
    `directed=True`, then flagged undirected. Both steps exist to stop cuGraph
    from re-symmetrizing (and OOMing) a graph that is already symmetric.
    """
    import cudf
    import cugraph
    import rmm
    import scipy.sparse as sp

    free_gpu_memory()
    # Managed memory lets allocations overflow to system RAM over PCIe.
    rmm.reinitialize(managed_memory=True)
    print("   [GPU] RMM managed memory enabled")
    vram_status()

    print("   [CPU] Building CSR...")
    coo = sp.coo_matrix(
        (np.ones(len(u), dtype=np.int8), (u, v)), shape=(n, n)
    )
    csr = coo.tocsr()
    del coo
    gc.collect()

    print("   [GPU] Loading CSR...")
    offsets = cudf.Series(csr.indptr.astype(np.int32))
    indices = cudf.Series(csr.indices.astype(np.int32))
    del csr
    gc.collect()

    G = cugraph.Graph(directed=True)
    G.from_cudf_adjlist(offsets, indices, None)
    del offsets, indices
    gc.collect()

    # is_directed() reads graph_properties.directed, NOT properties.directed.
    G.graph_properties.directed = False
    assert not G.is_directed(), "graph still marked directed; FA2 would re-symmetrize"
    vram_status()

    # force_atlas2 calls G.nodes(), which for a CSR-built graph reconstructs
    # and concatenates the edge list — several GB of VRAM for nothing. Inject
    # the answer instead. Fragile against cuGraph internals changing.
    nodes = cudf.Series(np.arange(n, dtype=np.int32), name="vertex")
    if hasattr(G, "_nodes"):
        G._nodes = nodes
    G.nodes = lambda: nodes

    iters = cfg["layout"].get("max_iter", 500)
    print(f"   [GPU] ForceAtlas2 ({iters} iterations, Barnes-Hut)...")
    with run_with_timer(f"FA2 ({iters} iters)"):
        pos = cugraph.force_atlas2(
            G,
            max_iter=iters,
            barnes_hut_optimize=True,
            outbound_attraction_distribution=True,
            lin_log_mode=cfg["layout"].get("lin_log", False),
            verbose=False,
        )
    pos = pos.to_arrow().to_pandas().sort_values("vertex")
    coords = pos[["x", "y"]].to_numpy().astype(np.float32)
    del pos
    gc.collect()
    vram_status()

    res = cfg["community"].get("resolution", 1.0)
    print(f"   [GPU] Leiden (resolution={res})...")
    with run_with_timer("Leiden"):
        parts, _ = cugraph.leiden(G, resolution=res)
    parts = parts.to_arrow().to_pandas().sort_values("vertex")
    memberships = parts["partition"].to_numpy().astype(np.int32)

    del G, parts
    gc.collect()
    free_gpu_memory()
    return coords, memberships


CANVAS = 1000.0
# Spread for backbone-propagated articles, as a fraction of the canvas.
JITTER = CANVAS * 0.004


def _normalize_centres(centres, connected):
    """Spread community centres over a fixed canvas, robustly.

    Only communities that actually link to another community carry positional
    information. The rest — singletons and isolated fragments, which are the
    large majority by count — get pushed to an outer ring instead of being
    allowed to dominate the bounding box and squash the real structure into a
    dot at the origin.
    """
    out = np.zeros_like(centres)
    idx = np.flatnonzero(connected)

    if len(idx) >= 2:
        pts = centres[idx]
        pts = pts - np.median(pts, axis=0)
        # Scale on a high percentile so a few stragglers cannot shrink everything.
        scale = np.percentile(np.hypot(pts[:, 0], pts[:, 1]), 95)
        pts = pts / max(scale, 1e-9) * (CANVAS * 0.5)
        out[idx] = pts
    elif len(idx) == 1:
        out[idx] = 0.0

    iso = np.flatnonzero(~connected)
    if len(iso):
        ang = np.arange(len(iso)) * 2.39996323
        r = CANVAS * 0.62
        out[iso, 0] = r * np.cos(ang)
        out[iso, 1] = r * np.sin(ang)
    return out


def _place_within_communities(mem, deg, centres, n, nc):
    """Scatter each community's articles around its centre.

    Articles are ordered by degree so hubs land near the middle, then placed
    on a golden-angle spiral with radius proportional to sqrt(rank), which
    fills the disc at uniform density. Disc area is proportional to community
    size, so the discs tile the canvas rather than nesting concentrically.
    """
    sizes = np.bincount(mem, minlength=nc)

    # Rank of each article within its own community, by descending degree.
    order = np.lexsort((-deg, mem))
    starts = np.zeros(nc + 1, dtype=np.int64)
    np.cumsum(sizes, out=starts[1:])
    rank = np.empty(n, dtype=np.int64)
    rank[order] = np.arange(n) - starts[mem[order]]

    frac = (rank + 0.5) / np.maximum(sizes[mem], 1)
    radius = 0.30 * CANVAS * np.sqrt(sizes[mem] / n) * np.sqrt(frac)
    theta = rank * 2.39996323  # golden angle

    coords = np.empty((n, 2), dtype=np.float32)
    coords[:, 0] = centres[mem, 0] + radius * np.cos(theta)
    coords[:, 1] = centres[mem, 1] + radius * np.sin(theta)
    return coords


def _layout_sfdp(u, v, n, mem, cfg, paths=None):
    """SFDP layout via graph-tool — the replacement for cuGraph's ForceAtlas2.

    cugraph.force_atlas2 is a legacy algorithm on cuGraph's old C++ API and
    segfaults against modern NVIDIA drivers (verified on a 4080 / driver 580:
    it dies inside cuCtxGetDevice on a *five vertex* graph, so it is not a
    scale or VRAM problem). SFDP is Hu's multilevel force-directed algorithm:
    actively maintained C++, coarsens like FA2 does, and needs no GPU at all,
    which removes this pipeline's last driver dependency.

    Communities are deliberately NOT fed in as layout groups — see the note
    at the sfdp_layout call. They are for colour, not geometry.
    """
    import graph_tool.all as gt

    # The layout is the expensive step by orders of magnitude — hours at
    # enwiki scale. Cache its raw output the moment it exists, so a bug in the
    # cheap post-processing below costs a rerun of the post-processing, not of
    # the layout. (Learned the hard way: a GraphView indexing mistake threw
    # away a completed 12-minute run.)
    raw_cache = os.path.join(paths.data_dir, "cache_sfdp_raw.npz") if paths else None
    if raw_cache and os.path.exists(raw_cache):
        z = np.load(raw_cache)
        if len(z["in_giant"]) == n:
            print(f"-> Reusing raw SFDP positions from {raw_cache}")
            bb = z["backbone"] if z["backbone"].size else None
            return _sfdp_place(z["raw"], z["in_giant"], z["labels"], n,
                               bb, z["u"], z["v"])
        print(f"   (ignoring {raw_cache}: built for a different graph)")

    print(f"   [CPU] Building graph-tool graph ({n:,} vertices, {len(u):,} edges)...")
    t = time.time()
    g = gt.Graph(directed=False)
    g.add_vertex(n)
    # add_edge_list takes the (E, 2) array directly; no Python-level loop.
    g.add_edge_list(np.column_stack([u, v]))
    print(f"   built in {elapsed(t)}")

    # Lay out the giant component ONLY.
    #
    # A force layout applies no attraction *between* disconnected components,
    # so it flings the small ones arbitrarily far and squashes everything real
    # into a dot: measured on simplewiki, 99% of articles landed within 55
    # units while stragglers reached 1,400, leaving the actual map occupying
    # 3.9% of the frame. This is the same failure `_normalize_centres` exists
    # to prevent, and the same fix — lay out what is connected, then place the
    # fragments deliberately.
    # Optional backbone mode: lay out only the highest-PageRank articles, then
    # place everything else at the centroid of its already-placed neighbours.
    #
    # Measured on simplewiki: a 10% backbone finished in 105s against 2,304s
    # for the full graph — 22x — and rendered *more* legibly, because the
    # backbone carries the community structure while the long tail of stubs
    # only blurs it. One propagation round placed 250k of 284k articles.
    #
    # It is an approximation, not a force layout: tail articles sit at the
    # average of their neighbours rather than finding their own equilibrium.
    frac = float(cfg["layout"].get("backbone_frac", 0.0) or 0.0)
    backbone = None
    if 0.0 < frac < 1.0 and paths is not None and os.path.exists(paths.cache_metrics):
        pr = pd.read_parquet(paths.cache_metrics)["pagerank"].to_numpy()
        k = max(2, int(n * frac))
        backbone = np.zeros(n, dtype=bool)
        backbone[np.argpartition(pr, -k)[-k:]] = True
        keep_e = backbone[u] & backbone[v]
        print(f"   [CPU] Backbone mode: top {k:,} by PageRank ({100*frac:.0f}%), "
              f"{keep_e.sum():,} induced edges ({100*keep_e.mean():.1f}% of all)")
        g.clear_edges()
        g.add_edge_list(np.column_stack([u[keep_e], v[keep_e]]))

    comp, _ = gt.label_components(g)
    labels = np.asarray(comp.a)
    counts = np.bincount(labels, weights=backbone if backbone is not None else None)
    giant = int(counts.argmax())
    in_giant = labels == giant
    if backbone is not None:
        in_giant &= backbone
    print(
        f"   [CPU] {len(counts):,} components; laying out the giant one "
        f"({in_giant.sum():,} articles, {100 * in_giant.mean():.1f}%)"
    )

    vfilt = g.new_vertex_property("bool")
    vfilt.a = in_giant
    gv = gt.GraphView(g, vfilt=vfilt)

    # No `groups=`. Feeding Leiden membership in as group attraction sounds
    # helpful and is not: graph-tool also applies inter-group *repulsion*, which
    # shatters a single connected component into isolated islands floating in
    # blank space. A map wants one landmass with regions in it, and the link
    # structure alone already produces that — communities are for colour, not
    # for geometry.
    threads = gt.openmp_get_num_threads() if gt.openmp_enabled() else 1
    print(f"   [CPU] SFDP (multilevel, OpenMP threads={threads})...")
    with run_with_timer("SFDP layout"):
        pos = gt.sfdp_layout(gv, multilevel=True, verbose=False)

    # get_2d_array on a GraphView returns ONLY the vertices the view keeps, in
    # ascending underlying-index order — not an array of size n. Map it back
    # explicitly rather than indexing with the mask.
    raw = np.asarray(pos.get_2d_array([0, 1]), dtype=np.float64).T
    del g, gv, pos
    gc.collect()

    if raw_cache:
        np.savez(raw_cache, raw=raw, in_giant=in_giant, labels=labels,
                 backbone=backbone if backbone is not None else np.zeros(0, bool),
                 u=u, v=v)
        print(f"   raw positions cached -> {raw_cache}")
    return _sfdp_place(raw, in_giant, labels, n, backbone, u, v)


def _sfdp_place(raw, in_giant, labels, n, backbone=None, u=None, v=None):
    """Normalize SFDP output onto the canvas and ring the fragments.

    Split out from the layout itself so it can be re-run from the raw cache.
    """
    giant_idx = np.flatnonzero(in_giant)
    if len(raw) != len(giant_idx):
        raise SystemExit(
            f"SFDP returned {len(raw):,} positions for {len(giant_idx):,} "
            "giant-component vertices — graph-tool's GraphView ordering changed"
        )

    coords = np.zeros((n, 2), dtype=np.float32)
    pts = raw - np.median(raw, axis=0)
    # Scale on a high percentile, not the extremes: even within one component
    # a handful of weakly-attached articles sit far out, and letting them set
    # the scale shrinks everything else.
    scale = np.percentile(np.hypot(pts[:, 0], pts[:, 1]), 99.5)
    pts = pts / max(scale, 1e-9) * (CANVAS * 0.5)

    # Scaling on a percentile sets the scale right but does not bound the tail:
    # a handful of weakly-attached articles can still sit an order of magnitude
    # further out and blow up the frame again. Pull anything past the limit
    # back along its own radius — direction is preserved, only the extreme tail
    # is compressed, and nothing escapes into the fragment ring.
    LIMIT = CANVAS * 0.55
    r = np.hypot(pts[:, 0], pts[:, 1])
    far = r > LIMIT
    if far.any():
        pts[far] *= (LIMIT / r[far])[:, None]
        print(f"   clamped {far.sum():,} outlying articles to the canvas edge")
    coords[giant_idx] = pts.astype(np.float32)

    # Fragments go on an outer ring, grouped so a component stays together.
    # Backbone mode: propagate the unplaced tail onto the laid-out backbone
    # before ringing whatever is still unreachable.
    if backbone is not None:
        import scipy.sparse as sp

        placed = in_giant.copy()
        A = sp.csr_matrix((np.ones(len(u), np.float32), (u, v)), shape=(n, n))
        for rnd in range(4):
            if placed.all():
                break
            cnt = A @ placed.astype(np.float32)
            sx = A @ (coords[:, 0] * placed)
            sy = A @ (coords[:, 1] * placed)
            newly = (~placed) & (cnt > 0)
            if not newly.any():
                break
            coords[newly, 0] = sx[newly] / cnt[newly]
            coords[newly, 1] = sy[newly] / cnt[newly]

            # Jitter, because the centroid of identical neighbours is
            # identical. Measured on simplewiki: 23.7% of articles landed on
            # top of another and the worst pile-up was 840 French communes at
            # one coordinate — each links only to its department, so they all
            # solve to the same point. In a real force layout repulsion would
            # spread them into a small cloud; this restores the area they
            # should occupy without pretending to recover their true
            # positions. Seeded, so runs stay reproducible.
            rng = np.random.default_rng(1234 + rnd)
            k = newly.sum()
            coords[newly] += rng.normal(0.0, JITTER, size=(k, 2))
            placed |= newly
            print(f"   propagation round {rnd + 1}: placed {newly.sum():,}, "
                  f"{int((~placed).sum()):,} left")
        del A
        gc.collect()
        in_giant = placed

    rest = np.flatnonzero(~in_giant)
    if len(rest):
        order = np.argsort(labels[rest], kind="stable")
        rest = rest[order]
        ang = np.arange(len(rest)) * 2.39996323  # golden angle
        r = CANVAS * 0.62 + (np.arange(len(rest)) % 7) * (CANVAS * 0.012)
        coords[rest, 0] = r * np.cos(ang)
        coords[rest, 1] = r * np.sin(ang)
    return coords


def _layout_cpu(u, v, n, cfg, paths=None):
    """CPU layout by coarsening: Leiden first, then lay out the community graph.

    Running DRL or FR directly on the full graph is not viable — on 7.3M
    symmetric edges (Simple English Wikipedia, a *small* wiki) it had not
    finished after 10 minutes, and enwiki is 50x larger. Coarsening is how
    large maps are normally built anyway: it keeps community structure legible
    and runs in seconds. Set layout.cpu_method to "drl" or "fr" to force a
    direct layout on graphs small enough to afford it.

    Note this is an approximation of ForceAtlas2, not a substitute. The GPU
    path remains the one that produces the real layout.
    """
    import igraph as ig

    obj = cfg["community"].get("objective", "modularity")
    res = cfg["community"].get("resolution", 1.0)
    # igraph accepts a numpy (E, 2) array directly; calling .tolist() first
    # would materialize millions of Python lists for no reason.
    edges = np.column_stack([u, v])

    print(f"   [CPU] Building graph ({n:,} vertices, {len(edges):,} edges)...")
    g = ig.Graph(n=n, edges=edges, directed=False)

    print(f"   [CPU] Leiden (objective={obj}, resolution={res})...")
    t = time.time()
    # resolution is the granularity dial for the map: at 1.0 modularity Leiden
    # returns ~24 macro-communities on simplewiki, which is too coarse to read
    # as regions. It was previously not passed here at all, so the config key
    # silently did nothing on the CPU path.
    part = g.community_leiden(objective_function=obj, resolution=res)
    mem = np.asarray(part.membership, dtype=np.int32)
    nc = int(mem.max()) + 1
    print(f"   {nc:,} communities in {elapsed(t)}")
    del part
    gc.collect()

    method = cfg["layout"].get("cpu_method", "sfdp")
    if method == "sfdp":
        del g
        gc.collect()
        return _layout_sfdp(u, v, n, mem, cfg, paths), mem

    if method in ("drl", "fr"):
        print(f"   [CPU] Direct {method.upper()} layout on the full graph...")
        with run_with_timer(f"{method.upper()} layout"):
            lay = g.layout_drl() if method == "drl" else g.layout_fruchterman_reingold()
        coords = np.array(lay.coords, dtype=np.float32)
        del g, lay
        gc.collect()
        return coords, mem

    del g
    gc.collect()

    # --- lay out the community meta-graph -------------------------------
    cu, cv = mem[u], mem[v]
    inter = cu != cv
    if nc > 1 and inter.any():
        key = (cu[inter].astype(np.int64) << 32) | cv[inter].astype(np.int64)
        uk, counts = np.unique(key, return_counts=True)
        meta_edges = np.column_stack(
            [(uk >> 32).astype(np.int32), (uk & 0xFFFFFFFF).astype(np.int32)]
        )
        connected = np.zeros(nc, dtype=bool)
        connected[meta_edges.ravel()] = True
        print(
            f"   [CPU] Community graph: {nc:,} nodes "
            f"({connected.sum():,} connected), {len(meta_edges):,} edges"
        )
        meta = ig.Graph(n=nc, edges=meta_edges, directed=False)
        meta.es["weight"] = counts.astype(float)
        with run_with_timer("community layout"):
            raw = np.array(meta.layout_drl(weights="weight").coords, dtype=np.float32)
        centres = _normalize_centres(raw, connected)
        del meta, meta_edges, uk, counts, raw
    else:
        print("   [CPU] Single community — nothing to lay out")
        centres = np.zeros((nc, 2), dtype=np.float32)

    del cu, cv, inter
    gc.collect()

    deg = np.bincount(u, minlength=n).astype(np.float64)
    coords = _place_within_communities(mem, deg, centres, n, nc)
    return coords, mem


def phase2_layout(paths, cfg, n, sample_ratio):
    if os.path.exists(paths.cache_layout):
        print("-> Cached layout found. Skipping Phase 2.")
        return

    t0 = time.time()
    src, dst = load_edges(paths, sample_ratio)
    u, v = symmetrize(src, dst, n)
    del src, dst
    gc.collect()

    backend = cfg["layout"].get("backend", "auto")
    coords = memberships = None
    if backend in ("auto", "gpu"):
        log_phase("Phase 2: Layout + Communities (GPU / cuGraph)")
        try:
            coords, memberships = _layout_gpu(u, v, n, cfg)
        except Exception as e:
            if backend == "gpu":
                raise
            import traceback

            print(f"\n   GPU path failed: {type(e).__name__}: {e}")
            traceback.print_exc()
            print("   Falling back to CPU.")
            free_gpu_memory()
    if coords is None:
        log_phase("Phase 2: Layout + Communities (CPU / igraph)")
        coords, memberships = _layout_cpu(u, v, n, cfg, paths)

    del u, v
    gc.collect()

    n_comm = len(np.unique(memberships))
    print(f"   {n_comm:,} communities")

    pd.DataFrame(
        {
            "vertex": np.arange(n, dtype=np.int32),
            "x": coords[:, 0],
            "y": coords[:, 1],
            "community": memberships,
        }
    ).to_parquet(paths.cache_layout, compression="zstd", index=False)
    print(f"   Phase 2 complete in {elapsed(t0)}")


# ---------------------------------------------------------------------------
# Phase 3 — merge and attach titles
# ---------------------------------------------------------------------------

def phase3_merge(paths):
    t0 = time.time()
    log_phase("Phase 3: Merge + Attach Titles")

    layout = pd.read_parquet(paths.cache_layout)
    metrics = pd.read_parquet(paths.cache_metrics)
    nodes = layout.merge(metrics, on="vertex", how="left")

    # Ids are dense 0..N-1, so titles attach by position — no dict of millions
    # of strings, no hash lookups.
    titles = pq.read_table(paths.titles, columns=["id", "title"])
    order = titles.column("id").to_numpy()
    names = np.asarray(titles.column("title").to_pylist(), dtype=object)
    if not np.array_equal(order, np.arange(len(order))):
        names = names[np.argsort(order)]

    if len(names) != len(nodes):
        raise SystemExit(
            f"titles.parquet has {len(names):,} rows but the graph has "
            f"{len(nodes):,} vertices — caches are stale, rerun with --reset"
        )

    nodes["vertex"] = names
    nodes = nodes[["vertex", "x", "y", "community", "pagerank", "degree"]]

    nodes.to_parquet(paths.nodes, compression="zstd", index=False)
    print(f"   {len(nodes):,} nodes → {paths.nodes}")
    print(f"   Phase 3 complete in {elapsed(t0)}")


def main():
    ap = argparse.ArgumentParser(description="Wikipedia Graph Compute Pipeline")
    ap.add_argument("--sample", type=float, default=None,
                    help="edge sample ratio, e.g. 0.01 (smoke tests only)")
    ap.add_argument("--reset", action="store_true", help="delete caches first")
    ap.add_argument("--data-dir", default=None, help="override pipeline.data_dir")
    ap.add_argument(
        "--phases", default="1,2,3",
        help="which phases to run, e.g. '1' or '2,3'. Phase 1 (PageRank) is "
             "CPU-capable at full scale; phase 2 (layout) wants a GPU, so the "
             "two halves often belong on different machines.",
    )
    args = ap.parse_args()

    try:
        phases = {int(p) for p in args.phases.split(",") if p.strip()}
    except ValueError:
        raise SystemExit(f"--phases must be comma-separated numbers, got {args.phases!r}")
    if not phases <= {1, 2, 3}:
        raise SystemExit(f"--phases may only contain 1, 2 or 3; got {sorted(phases)}")

    cfg = load_config()
    paths = Paths(args.data_dir or cfg["pipeline"]["data_dir"])
    os.makedirs(paths.data_dir, exist_ok=True)

    if args.reset:
        reset_caches(paths)

    paths.require_parser_outputs()
    sample = args.sample if args.sample is not None else cfg["pipeline"]["sample_ratio"]
    check_manifest(paths, sample, cfg)

    n = paths.n_articles()
    total = time.time()

    if 1 in phases:
        phase1_metrics(paths, cfg, n, sample)
    if 2 in phases:
        phase2_layout(paths, cfg, n, sample)
    if 3 in phases:
        # Phase 3 needs both caches; say so plainly rather than dying on a
        # missing-file traceback halfway through a long run.
        missing = [
            p for p in (paths.cache_metrics, paths.cache_layout) if not os.path.exists(p)
        ]
        if missing:
            raise SystemExit(
                "Phase 3 needs both caches, missing: "
                + ", ".join(missing)
                + "\n  Run the earlier phases first (see --phases)."
            )
        phase3_merge(paths)

    print(f"\n{'=' * 60}")
    print(f"Phases {sorted(phases)} complete in {elapsed(total)}")
    if 3 in phases:
        print(f"  → {paths.nodes}")
    print(f"{'=' * 60}")


if __name__ == "__main__":
    main()
