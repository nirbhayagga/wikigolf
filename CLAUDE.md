# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A pipeline that turns the English Wikipedia XML dump into an interactive force-directed graph: a Rust two-pass parser resolves `[[wikilinks]]` into an integer article link graph, then Python computes PageRank/layout/communities (GPU via RAPIDS, CPU fallback via igraph) and serves a Datashader/Panel viewer.

This is a **content project** — a public explorable map, quotable statistics, and a poster-grade render — so numbers stated publicly have to survive being checked. That is why the parser is strict about what counts as an article.

`codebase_reference.md` is a detailed per-file audit written 2026-06-24 — read it for deep context on any single file, but verify against the source since it can drift.

## Commands

```bash
# Rust parser (two passes over the dump; takes the path, not a pipe)
cargo build --release
cargo test --release
./target/release/wiki-parser data/dumps/simplewiki-latest-pages-articles.xml.bz2 --out data/simple
./target/release/wiki-parser data/dumps/enwiki-20260801-pages-articles-multistream.xml.bz2 --out data

# Link-selection flags (all default to the "keep" side except section cutting)
#   --strip-templates          drop {{...}} bodies, i.e. exclude infobox links
#   --strip-refs               drop <ref>...</ref> citation bodies
#   --keep-citation-sections   keep References / External links / Further reading
#   --titles-only              stop after pass 1

# Environment (RAPIDS must come from mamba, not pip)
mamba create -n rapids-env -c rapidsai -c conda-forge -c nvidia rapids=24.04 python=3.11 cuda-version=12.2 -y
mamba activate rapids-env
pip install -r python/requirements.txt
export KVIKIO_COMPAT_MODE=ON   # Fedora: disables GPU Direct Storage

# Pipeline (run from the repo root — all paths are relative)
python python/01_graph_compute.py              # phases 0-3, checkpointed
python python/01_graph_compute.py --sample 0.01  # 1% of edges: the dev loop
python python/01_graph_compute.py --reset      # wipe caches, recompute
python python/07_incremental.py --status       # which caches exist, sizes
python python/07_incremental.py --reset        # wipe caches without recomputing

# Downstream (all require data/nodes.parquet)
python python/02_video_stats.py
GEMINI_API_KEY=... python python/03_name_clusters.py
python python/06_community_stats.py
panel serve python/04_app.py --show
python python/05_export_png.py --width 4096 --height 2160

docker-compose up -d    # viewer only, on :5006
```

The Rust parser has unit + fixture tests (`cargo test`). The Python pipeline has none; validate it against **Simple English Wikipedia** (~339 MB dump, 284k articles), which runs end-to-end on a laptop in ~85 s. Prefer that over `--sample 0.01`, which shatters the graph into disconnected fragments and so cannot validate layout or community quality.

## Data contract

`wiki-parser` v0.2 owns **article identity** and writes Parquet directly:

| Output | Contents |
|---|---|
| `titles.parquet` | `id` (dense 0..N-1), `title` — real articles only |
| `redirects.parquet` | `alias`, `article_id` — for search-by-alias later |
| `edges.parquet` | `src`, `dst` int32, deduped, no self-loops |

There is **no `edges.csv` and no string→integer mapping phase** — that work moved into the parser, which is why the pipeline no longer needs 128 GB of RAM. `nodes.parquet` (`vertex` title, `x`, `y`, `community`, `pagerank`, `degree`) remains the contract for scripts 02–06.

`python/common.py` holds the single source of truth for paths, config defaults, and cache bookkeeping — previously duplicated between 01 and 07, where it drifted.

**Cache manifest:** `manifest.json` records a fingerprint of the inputs (edge bytes/rows, title count, sample ratio). Phases refuse to run against caches built from different inputs, which closes the old hazard where a sampled phase silently merged with a full one.

Verified end-to-end on Simple English Wikipedia: parser ~110 s, pipeline 60 s, 283,997 nodes / 3,940,103 edges.

**Full English Wikipedia (20260801) is parsed:** 7,219,290 articles / 231,681,569 edges, 46.4 min, 3.10 GB peak RSS. The old pipeline's 28M vertices / 482M edges are gone. Outputs total ~1.05 GB, so copy the Parquet to the GPU machine, never the 26.67 GB dump. Phase 2 at this scale has still never been run — that is the remaining unknown.

**Use the `-multistream` dump.** It is concatenated bz2 streams, so `pbzip2`/`lbzip2` can decompress it across cores: 58.9 MB/s measured vs 24.3 MB/s for a single stream. Decompression is the parser's bottleneck, so this is a 2.4x wall-clock difference. (simplewiki is single-stream and cannot benefit, which is why it takes ~110 s despite being 75x smaller.)

**Parse where the dump is, then copy the Parquet.** The parser is CPU/IO-bound and laptop-sized; only Phase 2 wants a GPU. Outputs are ~1-2 GB against a 27 GB dump.

## Architecture

**Scripts communicate through files in `data/`,** with `common.py` shared for paths and config. Run everything **from the repo root** — relative paths are resolved against the working directory.

**Checkpointing:** a phase is skipped when its cache parquet exists. Editing phase logic has no effect until you delete that cache — `--reset`, or `07_incremental.py --reset`.

Phase flow inside `01_graph_compute.py`:
- **Phase 1:** PageRank + in-degree on the **directed** graph. GPU via cuGraph; CPU fallback is sparse power iteration (`scipy`), which the old pipeline lacked entirely — a missing cuGraph used to abort the run.
- **Phase 2:** layout + Leiden on the **undirected** graph, symmetrized on the CPU first. See below.
- **Phase 3:** merge caches and attach titles **by position** — ids are dense `0..N-1`, so this is an array index, not a dict of millions of strings.

**Vertex count comes from `titles.parquet`, not `max(edge id)+1`** — articles with no links in either direction are still nodes (0.5% of simplewiki).

**Directed vs undirected is deliberate:** PageRank/in-degree use the directed graph (link direction = importance); ForceAtlas2 and Leiden use the symmetrized graph.

### CPU layout is coarsened, and is an approximation

Direct DRL/FR on the full graph is **not viable**: on simplewiki's 7.3M symmetric edges it had not finished after 10 minutes, and enwiki is far larger. `_layout_cpu` instead runs Leiden first (~6 s), lays out the ~1.4k-node community meta-graph, and places articles on a golden-angle spiral around their community centre with hubs at the middle. Set `layout.cpu_method: "drl"` to force a direct layout on graphs small enough to afford it.

The GPU ForceAtlas2 path is the one that produces the real layout; the CPU path exists so the pipeline runs end-to-end without a GPU.

Two things that will bite:
- **igraph accepts numpy `(E, 2)` arrays directly.** Calling `.tolist()` first materializes millions of Python lists and will OOM.
- **`run_with_timer` cannot tick during igraph/cuGraph calls** — those hold the GIL, so the clock thread is starved and the display freezes at `00:00:00`. It is not a hang.

### Rust parser (`src/`) — where graph identity is decided

Two passes, because link targets can only be resolved once every title is known:

1. **`index.rs`** — every ns=0 page (article *and* redirect) gets a raw id; redirect chains are followed (max 4 hops, cycle-safe); only real articles get a dense `article_id`.
2. **`edges.rs`** — links are normalized, resolved through redirects, and **dropped if they name no real article**. On simplewiki this discards 30% of raw links as red links; it is what keeps enwiki near ~7M nodes instead of 28M.

Load-bearing details:

- **`titles.rs::normalize_title` decides whether two link strings are the same article.** It applies MediaWiki's rules: underscores→spaces, whitespace collapse, anchor stripping, leading-colon stripping, first-letter capitalization. Capitalization is applied **only when the uppercase mapping is a single character** — Rust's full Unicode `to_uppercase()` turns `ß` into `SS`, which merged the article `ß` into the redirect `SS`→`Schutzstaffel` and silently deleted a real article.
- **Namespace prefix filtering is an optimization, not correctness.** Anything that is not a real article is dropped by the red-link filter anyway. A hand-maintained interwiki prefix list would wrongly reject real articles like `It: Chapter Two`.
- **Per-page target dedup is exact global dedup**, since edge (A,B) can only be produced by page A — but `edges.rs` still guards against emitting one source id twice, because two `<page>` elements *can* normalize to the same title.
- **`dump.rs` must handle `Event::GeneralRef`.** quick-xml ≥0.32 emits entity references as separate events; ignoring them deletes every `&` from titles (`AT&T` → `ATT`).
- **Parse errors and decompressor exit codes are fatal.** A truncated `.bz2` otherwise looks exactly like a clean end-of-dump and yields a silently partial graph.

### Phase 2's GPU workarounds (fragile — read before touching)

`_phase2_gpu` exists to fit 28M vertices / 482M edges into 16GB VRAM. Each step is load-bearing:
1. Edges are **pre-symmetrized on CPU** (both directions + dedup + self-loop removal) because cuGraph's internal symmetrization blows past VRAM.
2. The CSR is built **on CPU with scipy**, then handed over via `from_cudf_adjlist` — `from_cudf_edgelist` needs huge temporary sort buffers.
3. RMM is reinitialized with `managed_memory=True` so overflow pages to system RAM over PCIe.
4. The graph is constructed as `directed=True`, then `G.graph_properties.directed` is flipped to `False` so FA2/Leiden skip their own `to_undirected()`. Note the assert: `is_directed()` reads `graph_properties.directed`, not `properties.directed`.
5. `G.nodes()` is **monkey-patched** on the instance to return a precomputed range — cuGraph otherwise reconstructs and concats the edge list (~8GB, `std::bad_alloc`).

All GPU→CPU transfers go through `.to_arrow().to_pandas()` to bypass Numba driver bugs on GeForce cards. Per `codebase_reference.md`, Phase 2 had not yet completed a full-scale run on a 16GB RTX 4080 — this is the project's central blocker.

`_phase2_cpu` (igraph DRL + Leiden) is the fallback; `layout.backend: "auto"` swallows GPU errors and falls through to it, while `"gpu"` re-raises so failures stay visible.

### Config

`config.yaml` is read by five scripts, each with its **own** `load_config()` and its own hardcoded defaults dict. Adding a key means adding it to that script's defaults too, or it won't survive a missing/partial config file. `01_graph_compute.py` only merges top-level sections it already knows about.

CLI flags override config: `--sample` beats `pipeline.sample_ratio`, `--width/--height` beat `export.*`.

### Deployment

The Docker image installs **only visualization dependencies** and copies just `04_app.py`, `05_export_png.py`, and `config.yaml`; `data/` is mounted read-only at runtime with results pre-baked. Keep those two scripts free of RAPIDS/igraph/scipy imports or the image breaks.

## Repo conventions

- `.gitignore` excludes `*.md` except `README.md`, plus all of `/data/`, `*.parquet`, `*.csv`. Large artifacts and generated docs are intentionally untracked.
- Long-running GPU/CPU work is wrapped in `run_with_timer()`, which uses a thread (not a subprocess) because forking breaks CUDA context.
- Scripts that hit external state (`03_name_clusters.py`) save progress after every API call and resume from the on-disk JSON.
