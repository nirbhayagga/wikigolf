"""
Wikipedia Graph Compute Pipeline
=================================
Phase 0 (CPU): Load CSV, map strings to integers, cache to parquet
Phase 1 (GPU): PageRank + In-Degree on directed graph
Phase 2 (GPU): ForceAtlas2 layout + Leiden communities via cuGraph
         (CPU fallback: igraph DRL with edge sampling + Leiden)
Phase 3 (CPU): Merge all results, reverse-map integers to article names

Features:
  --sample 0.01    Sample 1% of edges for fast dev iteration
  --reset          Delete all caches and start fresh
  nvidia-smi VRAM monitoring, tqdm progress bars, per-phase timing
"""

import warnings
warnings.filterwarnings("ignore", category=FutureWarning)
warnings.filterwarnings("ignore", category=UserWarning)

import os
import gc
import time
import argparse
import contextlib
import subprocess
import threading
import numpy as np
import pandas as pd
import yaml
from pyarrow import csv as pa_csv
from tqdm import tqdm

DATA_DIR = "data"
CACHE_EDGES = os.path.join(DATA_DIR, "cache_edges_int.parquet")
CACHE_MAPPING = os.path.join(DATA_DIR, "cache_mapping.parquet")
CACHE_DIRECTED_DONE = os.path.join(DATA_DIR, "cache_directed_done")
CACHE_PAGERANK = os.path.join(DATA_DIR, "cache_pagerank.parquet")
CACHE_INDEGREE = os.path.join(DATA_DIR, "cache_in_degree.parquet")
CACHE_LAYOUT_DONE = os.path.join(DATA_DIR, "cache_layout_done")
CACHE_LAYOUT = os.path.join(DATA_DIR, "cache_layout.parquet")
CACHE_HASH = os.path.join(DATA_DIR, "cache_edges_hash.txt")
OUTPUT_NODES = os.path.join(DATA_DIR, "nodes.parquet")
OUTPUT_EDGES = os.path.join(DATA_DIR, "edges.parquet")

ALL_CACHES = [
    CACHE_EDGES, CACHE_MAPPING, CACHE_DIRECTED_DONE,
    CACHE_PAGERANK, CACHE_INDEGREE, CACHE_LAYOUT_DONE,
    CACHE_LAYOUT, CACHE_HASH, OUTPUT_NODES, OUTPUT_EDGES,
]


def load_config():
    defaults = {
        "pipeline": {"sample_ratio": 1.0, "data_dir": "data"},
        "gpu": {"store_transposed": True},
        "layout": {"algorithm": "fa2", "backend": "auto", "max_iter": 500,
                   "cpu_edge_sample": 1.0, "lin_log": False},
        "community": {"algorithm": "leiden", "objective": "modularity", "top_n": 20},
    }
    try:
        with open("config.yaml") as f:
            user = yaml.safe_load(f)
        for k in defaults:
            if k in user:
                defaults[k].update(user[k])
    except FileNotFoundError:
        pass
    return defaults


def log_phase(name):
    """Print a phase header with timestamp."""
    ts = time.strftime("%H:%M:%S")
    print(f"\n{'='*60}")
    print(f"[{ts}] {name}")
    print(f"{'='*60}")


def elapsed(start):
    """Return human-readable elapsed time string."""
    secs = time.time() - start
    if secs < 60:
        return f"{secs:.1f}s"
    mins = secs / 60
    return f"{mins:.1f}m"


def vram_status():
    """Print GPU VRAM usage if nvidia-smi is available."""
    try:
        out = subprocess.check_output(
            ['nvidia-smi', '--query-gpu=memory.used,memory.total',
             '--format=csv,nounits,noheader'],
            stderr=subprocess.DEVNULL
        )
        used, total = out.decode().strip().split(', ')
        print(f"   VRAM: {used}/{total} MB")
    except (FileNotFoundError, subprocess.CalledProcessError):
        pass


def _timer_proc(event, desc):
    start = time.time()
    while not event.is_set():
        secs = int(time.time() - start)
        h, m, s = secs // 3600, (secs % 3600) // 60, secs % 60
        print(f"\r   {desc}: {h:02d}:{m:02d}:{s:02d}", end="", flush=True)
        event.wait(1)
    print()

@contextlib.contextmanager
def run_with_timer(desc):
    """Run a block of code with a ticking threading timer to avoid CUDA fork issues."""
    stop_event = threading.Event()
    timer = threading.Thread(target=_timer_proc, args=(stop_event, desc))
    timer.start()
    try:
        yield
    finally:
        stop_event.set()
        timer.join()


def reset_caches():
    """Delete all cache files."""
    for path in ALL_CACHES:
        if os.path.exists(path):
            os.remove(path)
            print(f"   Deleted {path}")
    print("   All caches cleared.")


def phase0_prepare_edges(sample_ratio=1.0):
    if os.path.exists(CACHE_EDGES) and os.path.exists(CACHE_MAPPING):
        print("-> Cached integer edges found. Skipping Phase 0.")
        return

    csv_path = os.path.join(DATA_DIR, "edges.csv")
    if not os.path.exists(csv_path):
        print("ERROR: data/edges.csv not found.")
        print("  Run the Rust parser first:")
        print("    cargo build --release")
        print("    pbzip2 -dc enwiki-latest-pages-articles.xml.bz2 | ./target/release/wiki-parser > data/edges.csv")
        raise SystemExit(1)

    t0 = time.time()
    log_phase("Phase 0: String → Integer Mapping (CPU)")

    print("   Loading CSV via PyArrow (C++ multithreaded)...")
    edges_df = pa_csv.read_csv(csv_path).to_pandas()
    print(f"   Raw edges: {len(edges_df):,}")

    print("   Deduplicating edges...")
    before = len(edges_df)
    edges_df.drop_duplicates(inplace=True)
    print(f"   After dedup: {len(edges_df):,}  (removed {before - len(edges_df):,})")

    if sample_ratio < 1.0:
        n = int(len(edges_df) * sample_ratio)
        edges_df = edges_df.sample(n=n, random_state=42)
        print(f"   Sampled {sample_ratio*100:.1f}%: {len(edges_df):,} edges")

    # Save string-format edges BEFORE integer conversion (avoids re-reading 16GB CSV)
    print("   Saving string-format edge parquet...")
    edges_df.to_parquet(OUTPUT_EDGES, compression='zstd', index=False)

    print("   Building string → integer mapping...")
    unique_strings = pd.concat([edges_df['Source'], edges_df['Target']]).unique()
    print(f"   Unique articles: {len(unique_strings):,}")

    # Vectorized mapping — no Python for-loop over 6M+ strings
    mapping = dict(zip(unique_strings, np.arange(len(unique_strings), dtype=np.int32)))

    mapping_df = pd.DataFrame({
        'vertex_id': np.arange(len(unique_strings), dtype=np.int32),
        'vertex_name': unique_strings,
    })
    mapping_df.to_parquet(CACHE_MAPPING, compression='zstd', index=False)

    print("   Converting edge columns to integers...")
    with tqdm(total=3, desc="   Int mapping", unit="step") as pbar:
        edges_df['Source'] = edges_df['Source'].map(mapping).astype(np.int32)
        pbar.update(1)
        edges_df['Target'] = edges_df['Target'].map(mapping).astype(np.int32)
        pbar.update(1)
        edges_df.to_parquet(CACHE_EDGES, compression='zstd', index=False)
        pbar.update(1)

    # Save hash for incremental update detection
    size = os.path.getsize(csv_path)
    with open(CACHE_HASH, "w") as f:
        f.write(f"{size}")

    del edges_df, unique_strings, mapping, mapping_df
    gc.collect()
    print(f"   Phase 0 complete in {elapsed(t0)}")


def phase1_directed_gpu(config):
    if os.path.exists(CACHE_DIRECTED_DONE):
        print("-> Cached directed metrics found. Skipping Phase 1.")
        return

    t0 = time.time()
    log_phase("Phase 1: PageRank + In-Degree (GPU)")
    vram_status()

    import cudf
    import cugraph

    print("   Loading integer edges into GPU VRAM...")
    edges_gdf = cudf.read_parquet(CACHE_EDGES)
    vram_status()

    print("   Building directed GPU graph...")
    G = cugraph.Graph(directed=True)
    G.from_cudf_edgelist(
        edges_gdf, source='Source', destination='Target',
        renumber=False,
        store_transposed=config['gpu']['store_transposed']
    )
    del edges_gdf
    gc.collect()
    vram_status()

    print("   Computing In-Degree...")
    in_degree_pd = G.in_degree().to_arrow().to_pandas()
    in_degree_pd.to_parquet(CACHE_INDEGREE, index=False)

    print("   Computing PageRank...")
    pagerank_pd = cugraph.pagerank(G).to_arrow().to_pandas()
    pagerank_pd.to_parquet(CACHE_PAGERANK, index=False)

    del G
    gc.collect()

    # Force FULL CUDA memory release — RAPIDS memory pool holds onto VRAM
    # even after del + gc.collect(), causing OOM when Phase 2 builds a new graph.
    print("   Releasing all GPU memory...")
    try:
        import cupy as cp
        cp.get_default_memory_pool().free_all_blocks()
        cp.get_default_pinned_memory_pool().free_all_blocks()
    except (ImportError, Exception):
        pass
    try:
        import rmm
        rmm.reinitialize()
    except (ImportError, Exception):
        pass
    gc.collect()
    vram_status()

    open(CACHE_DIRECTED_DONE, "w").close()
    print(f"   Phase 1 complete in {elapsed(t0)}")


def _phase2_gpu(config, edges_df, n_vertices):
    """GPU path: cuGraph ForceAtlas2 + cuGraph Leiden.

    Problem: cuGraph FA2 internally forces symmetrization (to_undirected()),
    and cuGraph's from_cudf_edgelist also symmetrizes for undirected graphs.
    Both OOM on 16GB VRAM with 351M edges.

    Solution: Pre-symmetrize on CPU (128GB RAM), load as directed graph
    (no cuGraph symmetrization), then flip the directed flag so FA2 skips
    its internal to_undirected(). Everything fits in VRAM, runs at full speed.
    Uses RMM managed memory (CUDA UVM) so overflows spill to system RAM.
    """
    # --- Release any leftover GPU allocations from Phase 1 ---
    print("   [GPU] Releasing stale GPU memory before Phase 2...")
    try:
        import cupy as cp
        cp.get_default_memory_pool().free_all_blocks()
        cp.get_default_pinned_memory_pool().free_all_blocks()
    except (ImportError, Exception):
        pass
    gc.collect()

    # --- Enable managed memory (CUDA Unified Virtual Memory) ---
    # With 28M vertices and 482M edges, cuGraph's internal graph construction
    # needs ~10-12 GB of temporary buffers on top of the CSR data itself.
    # 16 GB VRAM is not enough. Managed memory lets CUDA transparently page
    # overflow to the 128 GB system RAM via the PCIe bus.
    import rmm
    rmm.reinitialize(managed_memory=True)
    print("   [GPU] RMM managed memory enabled (VRAM + system RAM via UVM)")
    vram_status()

    import cudf
    import cugraph

    # --- Pre-symmetrize on CPU (128GB RAM handles this easily) ---
    print("   [CPU] Pre-symmetrizing edges (avoids GPU OOM)...")
    t_sym = time.time()
    src = edges_df['Source'].values
    dst = edges_df['Target'].values

    # Create both directions: A→B becomes A→B AND B→A
    all_src = np.concatenate([src, dst])
    all_dst = np.concatenate([dst, src])
    del src, dst

    # Deduplicate + remove self-loops
    sym_df = pd.DataFrame({'src': all_src, 'dst': all_dst})
    del all_src, all_dst
    gc.collect()

    before = len(sym_df)
    sym_df.drop_duplicates(inplace=True)
    sym_df = sym_df[sym_df['src'] != sym_df['dst']]
    print(f"   Symmetric edges: {len(sym_df):,}  "
          f"(from {len(edges_df):,} directed, deduped {before - len(sym_df):,})")
    print(f"   Pre-symmetrize done in {elapsed(t_sym)}")

    # --- Build CSR natively on CPU to bypass GPU memory peaks ---
    # cuGraph's from_cudf_edgelist requires massive temporary GPU buffers to sort.
    # Instead, we build the CSR (Compressed Sparse Row) on the CPU using scipy,
    # which has 128GB RAM, and hand the compressed graph directly to the GPU.
    # Combined with RMM managed memory, this minimizes VRAM pressure.
    print("   [CPU] Building CSR matrix...")
    import scipy.sparse as sp
    
    # Use int8 for weights (1 byte) just to satisfy COO format cheaply
    weights = np.ones(len(sym_df), dtype=np.int8)
    coo = sp.coo_matrix((weights, (sym_df['src'].values, sym_df['dst'].values)), 
                        shape=(n_vertices, n_vertices))
    
    del sym_df, weights
    gc.collect()
    
    csr = coo.tocsr()
    del coo
    gc.collect()

    print("   [GPU] Loading compressed CSR into managed memory...")
    # Cast to int32 — 28M vertices and 482M edges both fit in int32 range,
    # saves ~110MB vs scipy's default int64 indptr
    offsets_gdf = cudf.Series(csr.indptr.astype(np.int32))
    indices_gdf = cudf.Series(csr.indices.astype(np.int32))
    
    del csr
    gc.collect()
    vram_status()

    print("   [GPU] Initializing graph...")
    G = cugraph.Graph(directed=True)
    G.from_cudf_adjlist(offsets_gdf, indices_gdf, None)
    
    del offsets_gdf, indices_gdf
    gc.collect()
    vram_status()

    # Mark as undirected so FA2/Leiden skip their internal to_undirected()
    # NOTE: cuGraph's is_directed() checks graph_properties.directed, NOT properties.directed
    G.graph_properties.directed = False
    assert not G.is_directed(), \
        "Failed to set graph as undirected — FA2 will try to re-symmetrize and OOM"
    print(f"   Graph marked undirected (is_directed={G.is_directed()}) — "
          f"FA2 will NOT re-symmetrize")

    # --- HOTFIX: Bypass OOM in G.nodes() ---
    # cugraph's force_atlas2 calls G.nodes() to get the list of vertices for the output.
    # When initialized from CSR, cuGraph reconstructs the edgelist and concats it to find unique nodes.
    # Concatenating two 480M-row arrays takes ~8GB VRAM and causes std::bad_alloc.
    # We pre-inject the nodes array to bypass this computation.
    # NOTE: This is a fragile monkey-patch that overrides the instance method.
    # It works because FA2/Leiden access G.nodes() on the instance, not the class.
    # If cuGraph changes internal access patterns, this may need updating.
    nodes_series = cudf.Series(np.arange(n_vertices, dtype=np.int32), name='vertex')
    if hasattr(G, '_nodes'):
        G._nodes = nodes_series
    G.nodes = lambda: nodes_series


    # --- ForceAtlas2 layout (GPU, Barnes-Hut) ---
    fa2_iters = config['layout'].get('max_iter', 500)
    print(f"   [GPU] Running ForceAtlas2 ({fa2_iters} iterations, Barnes-Hut)...")
    t_layout = time.time()
    with run_with_timer(f"FA2 ({fa2_iters} iters)"):
        pos = cugraph.force_atlas2(
            G,
            max_iter=fa2_iters,
            barnes_hut_optimize=True,
            outbound_attraction_distribution=True,
            lin_log_mode=config['layout'].get('lin_log', False),
            verbose=False,
        )
    print(f"   Layout complete in {elapsed(t_layout)}")
    vram_status()

    # Extract coordinates
    pos_pd = pos.to_pandas().sort_values('vertex').reset_index(drop=True)
    coords = pos_pd[['x', 'y']].values.astype(np.float32)
    del pos
    gc.collect()

    # --- Leiden communities (GPU) ---
    obj = config['community'].get('objective', 'modularity')
    res = config['community'].get('resolution', 1.0)
    print(f"   [GPU] Running Leiden communities (objective={obj})...")
    t_comm = time.time()

    with run_with_timer("Leiden"):
        parts, _ = cugraph.leiden(G, resolution=res)
    parts_pd = parts.to_pandas().sort_values('vertex').reset_index(drop=True)
    memberships = parts_pd['partition'].values.astype(np.int32)
    n_communities = len(np.unique(memberships))
    print(f"   {n_communities} communities found in {elapsed(t_comm)}")

    del G, parts, parts_pd
    gc.collect()

    return coords, memberships, n_vertices


def _phase2_cpu(config, edges_df, n_vertices):
    """CPU fallback: igraph DRL with edge sampling for layout, full edges for Leiden."""
    import igraph as ig

    n_edges = len(edges_df)
    # Sample edges for layout to make DRL feasible on CPU
    layout_sample = config['layout'].get('cpu_edge_sample', 0.15)
    use_sampling = layout_sample < 1.0 and n_edges > 5_000_000

    if use_sampling:
        n_sample = int(n_edges * layout_sample)
        print(f"   [CPU] Sampling {layout_sample*100:.0f}% of edges for layout "
              f"({n_sample:,} / {n_edges:,})...")
        sampled = edges_df.sample(n=n_sample, random_state=42)
    else:
        sampled = edges_df

    # --- Layout on sampled edges (visual approximation is fine) ---
    print("   [CPU] Building edge tuple list for layout...")
    src = sampled['Source'].values.tolist()
    dst = sampled['Target'].values.tolist()
    del sampled
    gc.collect()
    edge_tuples = list(tqdm(zip(src, dst), total=len(src),
                            desc="   Edges", unit="edges", unit_scale=True))
    del src, dst
    gc.collect()

    print("   [CPU] Constructing igraph object for layout...")
    g_layout = ig.Graph(n=n_vertices, edges=edge_tuples, directed=False)
    g_layout.simplify()
    del edge_tuples
    gc.collect()
    print(f"   Layout graph: {g_layout.vcount():,} vertices, {g_layout.ecount():,} edges")

    algo = config['layout'].get('algorithm', 'drl')
    if algo not in ('drl', 'fr'):
        print(f"   [CPU] Note: '{algo}' not available in igraph, using DRL")
        algo = 'drl'
    print(f"   [CPU] Running {algo.upper()} layout...")
    t_layout = time.time()

    def _run_layout(graph, algorithm):
        if algorithm == "fr":
            return graph.layout_fruchterman_reingold()
        return graph.layout_drl()

    with run_with_timer(f"{algo.upper()} layout"):
        layout = _run_layout(g_layout, algo)
    print(f"   Layout complete in {elapsed(t_layout)}")

    coords = np.array(layout.coords, dtype=np.float32)
    del g_layout, layout
    gc.collect()

    # --- Leiden on FULL edge set (community quality requires all edges) ---
    if use_sampling:
        print("   [CPU] Building full-edge graph for Leiden...")
        src_full = edges_df['Source'].values.tolist()
        dst_full = edges_df['Target'].values.tolist()
        del edges_df
        gc.collect()
        full_tuples = list(tqdm(zip(src_full, dst_full), total=len(src_full),
                                desc="   Full edges", unit="edges", unit_scale=True))
        del src_full, dst_full
        gc.collect()
        g_comm = ig.Graph(n=n_vertices, edges=full_tuples, directed=False)
        g_comm.simplify()
        del full_tuples
        gc.collect()
        print(f"   Community graph: {g_comm.vcount():,} vertices, {g_comm.ecount():,} edges")
    else:
        # No sampling — rebuild from edges_df (layout graph was already freed)
        print("   [CPU] Building graph for Leiden...")
        src_full = edges_df['Source'].values.tolist()
        dst_full = edges_df['Target'].values.tolist()
        del edges_df
        gc.collect()
        full_tuples = list(tqdm(zip(src_full, dst_full), total=len(src_full),
                                desc="   Edges", unit="edges", unit_scale=True))
        del src_full, dst_full
        gc.collect()
        g_comm = ig.Graph(n=n_vertices, edges=full_tuples, directed=False)
        g_comm.simplify()
        del full_tuples
        gc.collect()

    obj = config['community'].get('objective', 'modularity')
    print(f"   [CPU] Running Leiden (objective={obj})...")
    t_comm = time.time()

    with run_with_timer("Leiden"):
        partition = g_comm.community_leiden(objective_function=obj)
    memberships = np.array(partition.membership, dtype=np.int32)
    n_communities = len(set(partition.membership))
    print(f"   {n_communities} communities found in {elapsed(t_comm)}")

    del g_comm, partition
    gc.collect()

    return coords, memberships, n_vertices


def phase2_layout(config):
    if os.path.exists(CACHE_LAYOUT_DONE):
        print("-> Cached layout found. Skipping Phase 2.")
        return

    t0 = time.time()

    print("   Loading integer edges...")
    edges_df = pd.read_parquet(CACHE_EDGES)
    n_vertices = int(max(edges_df['Source'].max(), edges_df['Target'].max()) + 1)
    print(f"   Vertices: {n_vertices:,}  Edges: {len(edges_df):,}")

    # Try GPU path first, fall back to CPU
    use_gpu = config['layout'].get('backend', 'auto')
    if use_gpu in ('auto', 'gpu'):
        try:
            log_phase("Phase 2: Layout + Communities (GPU / cuGraph)")
            coords, memberships, n_v = _phase2_gpu(config, edges_df, n_vertices)
        except (ImportError, Exception) as e:
            if use_gpu == 'gpu':
                raise  # User explicitly requested GPU, don't hide errors
            import traceback
            print(f"\n   GPU path failed: {e}")
            print("   --- Full traceback ---")
            traceback.print_exc()
            print("   --- End traceback ---")
            print("   Falling back to CPU...")
            edges_df = pd.read_parquet(CACHE_EDGES)  # reload in case GPU path modified memory
            log_phase("Phase 2: Layout + Communities (CPU fallback)")
            coords, memberships, n_v = _phase2_cpu(config, edges_df, n_vertices)
    else:
        log_phase("Phase 2: Layout + Communities (CPU / igraph)")
        coords, memberships, n_v = _phase2_cpu(config, edges_df, n_vertices)

    layout_df = pd.DataFrame({
        'vertex': np.arange(n_v, dtype=np.int32),
        'x': coords[:, 0], 'y': coords[:, 1],
        'community': memberships,
    })
    layout_df.to_parquet(CACHE_LAYOUT, compression='zstd', index=False)

    del coords, memberships, layout_df
    gc.collect()

    open(CACHE_LAYOUT_DONE, "w").close()
    print(f"   Phase 2 complete in {elapsed(t0)}")


def phase3_merge_and_save():
    t0 = time.time()
    log_phase("Phase 3: Merge + Reverse Map")

    layout_df = pd.read_parquet(CACHE_LAYOUT)
    pagerank_df = pd.read_parquet(CACHE_PAGERANK)
    indegree_df = pd.read_parquet(CACHE_INDEGREE)

    # cuGraph in_degree() may return 'degree' or 'in_degree' depending on version.
    # Normalize to 'degree' for consistency.
    if 'in_degree' in indegree_df.columns and 'degree' not in indegree_df.columns:
        indegree_df = indegree_df.rename(columns={'in_degree': 'degree'})

    nodes = layout_df.merge(pagerank_df, on='vertex', how='left')
    nodes = nodes.merge(indegree_df, on='vertex', how='left')
    nodes['pagerank'] = nodes['pagerank'].fillna(0.0)
    nodes['degree'] = nodes['degree'].fillna(0).astype(np.int32)

    print("   Reverse-mapping integers → article names...")
    mapping_df = pd.read_parquet(CACHE_MAPPING)
    id_to_name = dict(zip(mapping_df['vertex_id'], mapping_df['vertex_name']))
    nodes['vertex'] = nodes['vertex'].map(id_to_name)
    nodes = nodes.dropna(subset=['vertex'])

    print(f"   Final: {len(nodes):,} nodes")
    nodes.to_parquet(OUTPUT_NODES, compression='zstd', index=False)
    print(f"   Saved → {OUTPUT_NODES}")
    print(f"   Phase 3 complete in {elapsed(t0)}")


def main():
    parser = argparse.ArgumentParser(description="Wikipedia Graph Compute Pipeline")
    parser.add_argument('--sample', type=float, default=None,
                        help='Sample ratio (e.g., 0.01 for 1%% of edges)')
    parser.add_argument('--reset', action='store_true',
                        help='Delete all caches and start fresh')
    args = parser.parse_args()

    config = load_config()
    os.makedirs(DATA_DIR, exist_ok=True)

    if args.reset:
        reset_caches()

    sample = args.sample if args.sample is not None else config['pipeline']['sample_ratio']

    t_total = time.time()
    phase0_prepare_edges(sample_ratio=sample)
    phase1_directed_gpu(config)
    phase2_layout(config)
    phase3_merge_and_save()

    print(f"\n{'='*60}")
    print(f"Pipeline complete in {elapsed(t_total)}")
    print(f"  → {OUTPUT_NODES}")
    print(f"  → {OUTPUT_EDGES}")
    print(f"{'='*60}")


if __name__ == "__main__":
    main()