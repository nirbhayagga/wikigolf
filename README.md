# Wikipedia Graph Pipeline

A high-performance, GPU-accelerated pipeline to parse, compute, and visualize the entire English Wikipedia network (~6.3 million articles, ~300 million links).

## Architecture

```
┌─────────────────┐     ┌──────────────────┐     ┌──────────────────┐
│   Rust Parser    │────▶│  Phase 0 (CPU)   │────▶│  Phase 1 (GPU)   │
│ SAX + mimalloc   │     │ CSV → int map,   │     │ PageRank, InDeg  │
│ XML → edges.csv  │     │ dedup, parquet    │     │ Directed graph   │
└─────────────────┘     └──────────────────┘     └──────────────────┘
                                                         │
                              ┌───────────────────────────┘
                              ▼
┌─────────────────┐     ┌──────────────────┐     ┌──────────────────┐
│  Phase 2 (GPU)   │────▶│  Gemini API      │────▶│  Datashader +    │
│ ForceAtlas2,     │     │ Semantic labels  │     │  Panel web app   │
│ Leiden community │     └──────────────────┘     └──────────────────┘
└─────────────────┘                                      │
                              ┌───────────────────────────┘
                              ▼
                        ┌──────────────────┐
                        │  Docker / Traefik│
                        │  Self-hosted     │
                        └──────────────────┘
```

### How It Works

1. **Rust SAX Parser:** Streams compressed XML directly through memory. Uses `mimalloc` to prevent fragmentation.
2. **PyArrow I/O:** C++ multithreaded CSV ingestion (5x faster than Pandas).
3. **Phase 1 — GPU Directed Metrics (RAPIDS):** PageRank + In-Degree on the directed graph. Uses Arrow bridge to bypass Numba driver bugs.
4. **Phase 2 — GPU Layout + Communities (RAPIDS):**
   - **Pre-symmetrization on CPU:** Directed edges are converted to undirected (both directions + dedup) using 128GB system RAM. This avoids cuGraph's internal symmetrization which exceeds 16GB VRAM.
   - **ForceAtlas2 layout (GPU):** Barnes-Hut approximation on the pre-symmetrized graph. Minutes instead of hours.
   - **Leiden communities (GPU):** Community detection on the same undirected graph.
   - **CPU fallback:** If GPU is unavailable, falls back to igraph DRL + Leiden (C backend).
5. **LLM Naming (Gemini):** Auto-labels top communities based on PageRank leaders.
6. **Interactive Viewer:** Panel + Datashader slippy map with search, hover tooltips, and community legend.
7. **Static Export:** High-res 4K PNG rendering via Datashader.

### Directed vs Undirected

The pipeline uses **both** graph types where each is appropriate:

| Phase | Graph Type | Why |
|-------|-----------|-----|
| **PageRank + In-Degree** | Directed | Directionality matters — "who links to whom" determines importance |
| **FA2 Layout** | Undirected | Force-directed physics needs symmetric attraction — "who is connected" |
| **Leiden Communities** | Undirected | Community structure is about mutual connectivity, not link direction |

## Prerequisites
* **Parse Stage:** `pbzip2`, Rust toolchain
* **GPU Stage:** NVIDIA GPU (16GB+ VRAM), RAPIDS (`cugraph`, `cudf`), 128GB+ system RAM
* **CPU Fallback:** `igraph` (pip install, uses C backend)
* **Viz Stage:** Panel, Datashader, Bokeh

## Quick Start

```bash
# 1. Parse Wikipedia XML → edges.csv
cargo build --release
pbzip2 -dc enwiki-latest-pages-articles.xml.bz2 | ./target/release/wiki-parser > data/edges.csv

# 2. Set up environment
mamba create -n rapids-env -c rapidsai -c conda-forge -c nvidia rapids=24.04 python=3.11 cuda-version=12.2 -y
mamba activate rapids-env
pip install -r python/requirements.txt

# 3. Run pipeline (Fedora: disable GPU Direct Storage)
export KVIKIO_COMPAT_MODE=ON
python python/01_graph_compute.py          # Full pipeline with checkpointing
python python/02_video_stats.py            # Orphans, dead ends
export GEMINI_API_KEY="your_key"
python python/03_name_clusters.py          # LLM community labels
python python/06_community_stats.py        # Per-community statistics

# 4. Launch viewer
panel serve python/04_app.py --show

# 5. Export static PNG
python python/05_export_png.py --width 4096 --height 2160
```

## Configuration

Edit `config.yaml` to tune parameters without modifying code:

```yaml
pipeline:
  sample_ratio: 1.0     # 0.01 = 1% sample for dev iteration
layout:
  backend: "auto"       # "auto", "gpu", or "cpu"
  algorithm: "fa2"      # GPU: ForceAtlas2. CPU fallback: "drl" or "fr"
  max_iter: 500         # FA2 iterations
  cpu_edge_sample: 1.0  # Edge sampling ratio for CPU fallback
community:
  top_n: 20             # Communities to label
```

## CLI Flags

```bash
# Dev mode: sample 1% of edges for fast iteration
python python/01_graph_compute.py --sample 0.01

# Reset all caches and recompute
python python/01_graph_compute.py --reset

# Cache status and change detection
python python/07_incremental.py --status
python python/07_incremental.py --reset
```

## Pipeline Scripts

| Script | Purpose | Engine |
|--------|---------|--------|
| `01_graph_compute.py` | PageRank, InDegree, FA2 Layout, Leiden Communities | GPU (cuGraph) |
| `02_video_stats.py` | Orphans, dead ends, isolated nodes | CPU (PyArrow) |
| `03_name_clusters.py` | LLM semantic community labels | Gemini API |
| `04_app.py` | Interactive web viewer with search | Panel/Datashader |
| `05_export_png.py` | Static 4K PNG export | Datashader |
| `06_community_stats.py` | Per-community statistics JSON | CPU (Pandas) |
| `07_incremental.py` | Cache management and change detection | CPU |

## Production Tuning

* **Checkpointing:** Each phase saves results to disk. Crashes resume from the last checkpoint.
* **Pre-symmetrization:** Edges are symmetrized on CPU before GPU loading, avoiding the 16GB VRAM limit during cuGraph's internal symmetrization.
* **Arrow Bridge:** GPU→CPU transfers use `.to_arrow().to_pandas()` to bypass Numba driver bugs on GeForce cards.
* **VRAM Monitoring:** `nvidia-smi` usage printed before/after GPU operations.
* **Fedora:** Set `KVIKIO_COMPAT_MODE=ON` to disable GPU Direct Storage.
* **Swap:** For `systemd-oomd`, ensure NVMe swap is active during string mapping.

## Docker Deployment

```bash
docker-compose up -d
# Available at http://localhost:5006 (or wiki.yourdomain.com via Traefik)
```

The Docker image only includes visualization dependencies — no GPU or igraph needed since computation results are pre-baked into parquet files.

## Acknowledgments
Inspired by ["I Made a Graph of Wikipedia... This Is What I Found"](https://www.youtube.com/watch?v=JheGL6uSF-4) by adumb. This is a completely independent implementation using Rust, RAPIDS, and igraph.