# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A pipeline that turns the English Wikipedia XML dump into an interactive force-directed graph: a Rust two-pass parser resolves `[[wikilinks]]` into an integer article link graph, then Python computes PageRank (scipy or cuGraph), layout (graph-tool SFDP, CPU) and communities (Leiden), and serves a Datashader/Panel viewer. A second Rust binary (`serve`) runs the wiki-race game off the same Parquet.

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

**Full English Wikipedia (20260801) is parsed:** 7,219,290 articles / 231,681,569 edges, 46.4 min, 3.10 GB peak RSS. The old pipeline's 28M vertices / 482M edges are gone. Outputs total ~1.05 GB, so copy the Parquet between machines, never the 26.67 GB dump.

**Phase 2 has run at full scale.** Symmetrization gives 419,049,910 edges in 26 s. A 10% backbone is 721,929 nodes / 83,399,460 induced edges (19.9% of all). What is still unproven is a *full* (`backbone_frac: 0`) layout, and whether either produces a map with real structure — see the density note below.

**A new dump means a full re-run.** Article ids are dense `0..N-1` assigned in parse order, so a single added or removed article shifts every id after it: parse, PageRank, and layout all have to be redone, and anything that stored an id (leaderboard rows, saved puzzles) is silently wrong afterwards. Making refreshes incremental means persisting a title→id map and only appending ids for new articles, which would also let the layout warm-start from the previous positions. That work has not been done.

**Use the `-multistream` dump.** It is concatenated bz2 streams, so `pbzip2`/`lbzip2` can decompress it across cores: 58.9 MB/s measured vs 24.3 MB/s for a single stream. Decompression is the parser's bottleneck, so this is a 2.4x wall-clock difference. (simplewiki is single-stream and cannot benefit, which is why it takes ~110 s despite being 75x smaller.)

**Parse where the dump is, then copy the Parquet.** The parser is CPU/IO-bound and laptop-sized. Outputs are ~1-2 GB against a 27 GB dump. Nothing in the pipeline needs a GPU any more; Phase 2 wants cores and RAM, and SFDP is OpenMP-parallel.

## Architecture

**Scripts communicate through files in `data/`,** with `common.py` shared for paths and config. Run everything **from the repo root** — relative paths are resolved against the working directory.

**Checkpointing:** a phase is skipped when its cache parquet exists. Editing phase logic has no effect until you delete that cache — `--reset`, or `07_incremental.py --reset`.

Phase flow inside `01_graph_compute.py`:
- **Phase 1:** PageRank + in-degree on the **directed** graph. GPU via cuGraph; CPU fallback is sparse power iteration (`scipy`), which the old pipeline lacked entirely — a missing cuGraph used to abort the run.
- **Phase 2:** layout + Leiden on the **undirected** graph, symmetrized on the CPU first. See below.
- **Phase 3:** merge caches and attach titles **by position** — ids are dense `0..N-1`, so this is an array index, not a dict of millions of strings.

**Vertex count comes from `titles.parquet`, not `max(edge id)+1`** — articles with no links in either direction are still nodes (0.5% of simplewiki).

**Directed vs undirected is deliberate:** PageRank/in-degree use the directed graph (link direction = importance); ForceAtlas2 and Leiden use the symmetrized graph.

### Layout is CPU SFDP. The GPU path is dead.

**`cugraph.force_atlas2` segfaults and is not usable.** It is a legacy algorithm on cuGraph's old C++ API and dies inside `cuCtxGetDevice` against modern NVIDIA drivers — verified on an RTX 4080 / driver 580 on a *five vertex* graph, so it is not a scale or VRAM problem. Four hypotheses were eliminated (cuda-python version, RMM managed memory, our CSR/monkey-patch workarounds, numba context initialization). This reframed the project's old "Phase 2 has never completed at full scale" blocker: the 4x graph inflation was real *and* the layout was failing for an unrelated reason no amount of VRAM would have fixed.

A segfault is not a Python exception, so **`layout.backend: "auto"` cannot fall back from it** — the process simply dies. That is why the default is `"cpu"`.

`_layout_sfdp` (graph-tool SFDP, Hu's multilevel force-directed algorithm) is now the real layout: actively maintained C++, OpenMP-parallel, no GPU, no driver dependency. `layout.cpu_method` also accepts `"drl"`/`"fr"` (direct igraph layout, small graphs only) and `"coarsened"` (lays out the community meta-graph and spirals articles around their centre — seconds, but a much cruder map).

**Backbone mode** (`layout.backbone_frac`, default 0.10) lays out only the highest-PageRank articles and places the rest at the centroid of their placed neighbours, with seeded Gaussian jitter so identical neighbours form a small cloud instead of a pile. Measured on simplewiki: 105 s vs 2,304 s. No article or edge is dropped and PageRank/communities still use the full graph; what is lost is a tail article's ability to find its own position.

Things that will bite:
- **igraph accepts numpy `(E, 2)` arrays directly.** Calling `.tolist()` first materializes millions of Python lists and will OOM.
- **`run_with_timer` cannot tick during igraph/graph-tool calls** — those hold the GIL, so the clock thread is starved and the display freezes at `00:00:00`. It is not a hang.
- **No `groups=` on `sfdp_layout`.** graph-tool pairs group attraction with group *repulsion*, which shatters one connected component into isolated islands. Communities are for colour, not geometry — so the layout does not depend on `community.resolution`, and `cache_sfdp_raw.npz` is deliberately fingerprinted without it.
- **Lay out the giant component only.** A force layout applies no attraction between disconnected components and flings them arbitrarily far: on simplewiki 99% of articles landed within 55 units while stragglers reached 1,400, leaving the map on 3.9% of the frame.
- **`GraphView.get_2d_array` returns only the vertices the view keeps**, not an array of size `n`. Map it back with an explicit index, not a boolean mask.

**Density is the open problem at enwiki scale.** The 10% backbone (721,929 nodes / 83.4M induced edges, average degree ~231) laid out to a density statistically indistinguishable from uniform random points — max/mean 2.2x against 2.1x for a random disc. simplewiki's backbone under identical settings scores 6.4x. SFDP's `C` and `p` were swept and do not help (`p=3` → 4.8x, `p=4` → 4.0x, i.e. worse; `C=0.05` → unchanged). Treat a max/mean near 2 in the exporter's density line as "the graph was too dense to unfold", not as a rendering problem.

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

### Phase 2's GPU workarounds (dead code — kept only as a record)

**None of this runs.** `_phase2_gpu` is unreachable in practice because `force_atlas2` segfaults (see above), and `layout.backend` defaults to `"cpu"`. It is documented because the workarounds were expensive to find and would be needed again if cuGraph ever ships a working force layout on its current API.

`_phase2_gpu` existed to fit 28M vertices / 482M edges into 16GB VRAM. Each step was load-bearing:
1. Edges are **pre-symmetrized on CPU** (both directions + dedup + self-loop removal) because cuGraph's internal symmetrization blows past VRAM.
2. The CSR is built **on CPU with scipy**, then handed over via `from_cudf_adjlist` — `from_cudf_edgelist` needs huge temporary sort buffers.
3. RMM is reinitialized with `managed_memory=True` so overflow pages to system RAM over PCIe.
4. The graph is constructed as `directed=True`, then `G.graph_properties.directed` is flipped to `False` so FA2/Leiden skip their own `to_undirected()`. Note the assert: `is_directed()` reads `graph_properties.directed`, not `properties.directed`.
5. `G.nodes()` is **monkey-patched** on the instance to return a precomputed range — cuGraph otherwise reconstructs and concats the edge list (~8GB, `std::bad_alloc`).

All GPU→CPU transfers go through `.to_arrow().to_pandas()` to bypass Numba driver bugs on GeForce cards.

`layout.backend: "auto"` swallows GPU *exceptions* and falls through to the CPU path, `"gpu"` re-raises so failures stay visible — but neither helps against a segfault, which kills the interpreter outright. Do not set either without a specific reason.

### Config

`config.yaml` is read by five scripts, each with its **own** `load_config()` and its own hardcoded defaults dict. Adding a key means adding it to that script's defaults too, or it won't survive a missing/partial config file. `01_graph_compute.py` only merges top-level sections it already knows about.

CLI flags override config: `--sample` beats `pipeline.sample_ratio`, `--width/--height` beat `export.*`.

### Deployment

Two independent images that share no code:

**Viewer** (`Dockerfile` + `docker-compose.yml`) installs **only visualization dependencies** and copies just `04_app.py`, `05_export_png.py`, and `config.yaml`; `data/` is mounted read-only at runtime with results pre-baked. Keep those two scripts free of RAPIDS/igraph/scipy imports or the image breaks.

**Game** (`Dockerfile.game` + `docker-compose.game.yml`) is the Rust `serve` binary on debian-slim, with no Python at all, behind Traefik with Let's Encrypt. Measured at full enwiki scale: **3.79 GB RSS**, ~1 min to load 231.7M edges into CSR, then 22 ms for a bidirectional BFS and 17 ms for `/api/meta`. Load-bearing:

- **`--host 0.0.0.0`.** The binary defaults to `127.0.0.1`, which inside a container is unreachable from the host.
- **`--trust-proxy` is required behind a proxy and unsafe without one.** Without it every request appears to come from the proxy's IP and the per-IP rate limiter throttles all users as one bucket; with it on a directly-exposed port, a client can forge `X-Forwarded-For` and defeat rate limiting entirely.
- **`--secure-cookies` only over HTTPS.** A Secure cookie is never sent over plain http, so setting it in local development silently breaks identity.
- **The healthcheck needs a long `start-period`.** The process accepts no connections until the CSR is built, so a default start period makes the orchestrator kill it in a loop before it ever comes up.
- **`--state` must point somewhere writable and persistent, and must not be the data dir.** `Registry::open` and `Identity::load_or_create` write `leaderboard.jsonl` and `.wiki-race-secret`, and they default to `--data` — which a deployment mounts read-only, so the process dies at startup with `Read-only file system (os error 30)` and restarts forever. Traefik registers no router for a container in that state, so the symptom presents as "the hostname does not resolve" rather than as a write error. Losing the state volume invalidates every player cookie and the leaderboard with it.

Data is mounted, never baked in: it is ~1.3 GB and changes on a completely different cadence than the code.

## Repo conventions

- `.gitignore` excludes `*.md` except `README.md`, plus all of `/data/`, `*.parquet`, `*.csv`. Large artifacts and generated docs are intentionally untracked.
- Long-running GPU/CPU work is wrapped in `run_with_timer()`, which uses a thread (not a subprocess) because forking breaks CUDA context.
- Scripts that hit external state (`03_name_clusters.py`) save progress after every API call and resume from the on-disk JSON.
