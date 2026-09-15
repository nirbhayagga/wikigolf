# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A pipeline that turns the English Wikipedia XML dump into an interactive force-directed graph: a Rust two-pass parser resolves `[[wikilinks]]` into an integer article link graph, then Python computes PageRank (scipy or cuGraph), layout (graph-tool SFDP, CPU) and communities (Leiden), and serves a Datashader/Panel viewer. A second Rust binary (`serve`) runs the wiki-race game off the same Parquet.

This is a **content project** — a public explorable map, quotable statistics, and a poster-grade render — so numbers stated publicly have to survive being checked. That is why the parser is strict about what counts as an article.

`CODEBASE.md` is the per-file reference: every Rust module and Python script, every data file with its real size, the endpoint surface, and the measurements. It is tracked, unlike the generated audit it replaced. The operational runbook (deploy procedures, machine split, incident history) is maintained privately outside this repo. Keep the docs in step when behavior changes.

This file stays at the level of commands and traps. When a detail here grows a table, a size, or a benchmark, it belongs in CODEBASE.md.

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
python python/01_graph_compute.py --reset-layout --phases 2,3
                                               # redo Leiden + merge; KEEPS the
                                               # 10-hour cache_sfdp_raw.npz
python python/07_incremental.py --status       # which caches exist, sizes
python python/07_incremental.py --reset        # wipe caches without recomputing

# Downstream (all require data/nodes.parquet)
python python/02_video_stats.py
GEMINI_API_KEY=... python python/03_name_clusters.py
python python/06_community_stats.py
panel serve python/04_app.py --show
python python/05_export_png.py --width 4096 --height 2160
python python/08_export_gephi.py               # CSVs for the manual LinLog test
python python/09_pageviews.py                  # joins external pageview counts

# Wiki-race game server (loads titles+edges; the rest degrade gracefully)
cargo build --release --bin serve
./target/release/serve --data data --state <writable-dir> --host 127.0.0.1 --port 8080

# Puzzle pools: PC job, ~2 h at enwiki scale, once per dump. Optional —
# without data/pools.parquet the server rejection-samples as before; WITH a
# stale one (different dump) it refuses to start, by design.
cargo build --release --bin pools
./target/release/pools --data data

# Static build: the whole game as files for free hosting (Cloudflare Pages).
# Dailies pre-generate a year ahead (~2 KB each) and advance on the visitor's
# clock via the shared epoch; every puzzle carries one baked shortest route,
# and every daily and random-pool goal gets a compass-lite file. Needs
# pools.parquet for the Random button. Layout, dials and measured costs:
# CODEBASE.md (bin/static_export.rs).
cargo build --release --bin static_export
./target/release/static_export --data data --out static_site

# End-to-end smoke tests (Playwright) run against a deployed site, never a
# build. Default target is production; point BASE_URL at a local
# `python -m http.server` over static_site to test an export before deploy.
cd e2e && npm ci && npx playwright test          # or BASE_URL=http://127.0.0.1:8765 npx playwright test

# Head-to-head duels exist but are DARK: routes mount only with
# --enable-duels, and no UI references them. Kilobytes per room when on.

docker compose -f docker-compose.game.yml up -d         # game behind Traefik (.env: RACE_HOST etc.)
docker compose -f docker-compose.game.direct.yml up -d  # game on a bare host port
docker compose -f docker-compose.game.tunnel.yml up -d  # game behind a Cloudflare Tunnel (.env: CF_TUNNEL_TOKEN; hostname lives in the CF dashboard)
docker compose -f docker-compose.game.yml -f docker-compose.backup.yml up -d  # + daily state-volume backups (leaderboard + cookie secret)
docker compose -f docker-compose.yml up -d              # map viewer behind Traefik (VIEW_HOST)
docker compose -f docker-compose.game.yml -f docker-compose.yml up -d  # game + viewer together
```

The Rust parser has unit + fixture tests (`cargo test`). The Python pipeline has none; validate it against **Simple English Wikipedia** (~339 MB dump, 284k articles), which runs end-to-end on a laptop in ~85 s. Prefer that over `--sample 0.01`, which shatters the graph into disconnected fragments and so cannot validate layout or community quality.

## Data contract

The parser owns **article identity** and writes the final Parquet directly — there is no `edges.csv` and no string→integer mapping phase, which is why the pipeline no longer needs 128 GB of RAM. The file-by-file contract, with real sizes, lives in CODEBASE.md; `python/common.py` is the single source of truth for paths, config defaults, and cache bookkeeping.

Rules that protect the contract:

- **Cache manifest:** `manifest.json` fingerprints the inputs (edge bytes/rows, title count, sample ratio); phases refuse to run against caches built from different inputs. This closed the old hazard where a sampled phase silently merged with a full one.
- **A new dump means a full re-run.** Ids are dense `0..N-1` in parse order, so one added or removed article shifts every id after it: parse, PageRank, and layout all redo, and anything that stored an id (leaderboard rows, saved puzzles) is silently wrong afterwards. Incremental refresh (persist a title→id map, append ids for new articles, warm-start the layout) has not been built.
- **Parsing is deterministic.** Re-parses of the same dump reproduce `titles.parquet` and `edges.parquet` byte-for-byte, so ids are stable and re-parsing to add a column is safe.
- **Use the `-multistream` dump.** Concatenated bz2 streams let `pbzip2`/`lbzip2` decompress across cores — a measured 2.4x on wall clock, and decompression is the parser's bottleneck. (simplewiki is single-stream and cannot benefit.)
- **Parse where the dump is, then copy the Parquet** (~1-2 GB against a 27 GB dump). Nothing in the pipeline needs a GPU; Phase 2 wants cores and RAM, and SFDP is OpenMP-parallel.

## Architecture

Scripts communicate through files in `data/`, with `common.py` shared for paths and config. Run everything **from the repo root** — relative paths resolve against the working directory. A phase is skipped when its cache parquet exists, so editing phase logic has no effect until you delete that cache (`--reset`, or `07_incremental.py --reset`).

- **Vertex count comes from `titles.parquet`, not `max(edge id)+1`** — articles with no links in either direction are still nodes.
- **Directed vs undirected is deliberate:** PageRank/in-degree use the directed graph (link direction = importance); layout and Leiden use the symmetrized graph.
- **Phase 3 attaches titles by position** — dense ids make it an array index, not a dict of millions of strings.

### Layout is CPU SFDP. The GPU path is dead.

`cugraph.force_atlas2` segfaults inside `cuCtxGetDevice` on a *five-vertex* graph (RTX 4080, driver 580) — a legacy algorithm on cuGraph's old C++ API, not a scale or VRAM problem; four hypotheses were eliminated before that conclusion. A segfault is not a Python exception, so `layout.backend: "auto"` cannot fall back from it — the process simply dies. The default is `"cpu"` (graph-tool SFDP, Hu's multilevel algorithm; `layout.cpu_method` also accepts `"drl"`/`"fr"` for small graphs and `"coarsened"` for a crude fast map). Do not set `"auto"` or `"gpu"` without a specific reason. The GPU-era workarounds are archived in CODEBASE.md in case cuGraph ever ships a working force layout.

**The most expensive artifact in the project is `cache_sfdp_raw.npz`** — the ~10-hour full-scale SFDP positions. `--reset-layout` deliberately preserves it, and a Phase 2 rerun that prints anything other than "Reusing raw SFDP positions" is about to redo ten hours and should be killed. The layout does not depend on `community.resolution`, and the cache is deliberately fingerprinted without it.

**Backbone mode** (`layout.backbone_frac`, default 0.10) lays out only the highest-PageRank articles and places the rest at the centroid of their placed neighbours with seeded jitter — ~20x faster; nothing is dropped, but a tail article loses the ability to find its own position.

Things that will bite:
- **igraph accepts numpy `(E, 2)` arrays directly.** Calling `.tolist()` first materializes millions of Python lists and will OOM.
- **`run_with_timer` cannot tick during igraph/graph-tool calls** — those hold the GIL, so the clock thread is starved and the display freezes at `00:00:00`. It is not a hang.
- **No `groups=` on `sfdp_layout`.** graph-tool pairs group attraction with group *repulsion*, which shatters one connected component into isolated islands. Communities are for colour, not geometry.
- **Lay out the giant component only.** A force layout applies no attraction between disconnected components and flings them arbitrarily far, leaving the map on a few percent of the frame.
- **`GraphView.get_2d_array` returns only the vertices the view keeps**, not an array of size `n`. Map it back with an explicit index, not a boolean mask.

**Density is the open problem at enwiki scale.** The full layout has real regions but no structure below them at zoom; SFDP's `C` and `p` were swept and do not help. Treat a max/mean density near 2 in the exporter's density line as "the graph was too dense to unfold", not as a rendering problem. **The one untested lever is ForceAtlas2's LinLog mode**, which graph-tool does not offer — `08_export_gephi.py` writes the CSVs for testing it in Gephi by hand. Measurements are in CODEBASE.md.

### Rust parser — where graph identity is decided

Two passes, because link targets can only be resolved once every title is known: pass 1 (`index.rs`) ids every ns=0 page and resolves redirect chains; pass 2 (`edges.rs`) normalizes links, resolves through redirects, and drops red links — which is what keeps enwiki near ~7M nodes instead of 28M. Per-file roles are in CODEBASE.md. The traps:

- **`titles.rs::normalize_title` decides whether two link strings are the same article.** It applies MediaWiki's rules; capitalization is applied **only when the uppercase mapping is a single character** — Rust's full Unicode `to_uppercase()` turns `ß` into `SS`, which merged the article `ß` into the redirect `SS`→`Schutzstaffel` and silently deleted a real article.
- **Namespace prefix filtering is an optimization, not correctness.** Anything that is not a real article is dropped by the red-link filter anyway. A hand-maintained interwiki prefix list would wrongly reject real articles like `It: Chapter Two`.
- **Per-page target dedup is exact global dedup**, since edge (A,B) can only be produced by page A — but `edges.rs` still guards against emitting one source id twice, because two `<page>` elements *can* normalize to the same title.
- **`dump.rs` must handle `Event::GeneralRef`.** quick-xml ≥0.32 emits entity references as separate events; ignoring them deletes every `&` from titles (`AT&T` → `ATT`).
- **Parse errors and decompressor exit codes are fatal.** A truncated `.bz2` otherwise looks exactly like a clean end-of-dump and yields a silently partial graph.

### Config

`config.yaml` is read by most of the Python scripts, each with its **own** `load_config()` and its own hardcoded defaults dict. Adding a key means adding it to that script's defaults too, or it won't survive a missing/partial config file. `01_graph_compute.py` only merges top-level sections it already knows about.

CLI flags override config: `--sample` beats `pipeline.sample_ratio`, `--width/--height` beat `export.*`.

`community.resolution: 6.0` and the two `max_categories: 96` are the **shipped enwiki values, deliberately tracked** — a working-tree edit of the resolution dial was once destroyed by `git reset --hard` and the next merge silently regressed the map to res-1. Do not "clean them up" back to the defaults-looking 1.0/24.

### Deployment

Two independent images that share no code. The **viewer** image installs only visualization dependencies and copies just `04_app.py`, `05_export_png.py` and `config.yaml` — keep those two scripts free of RAPIDS/igraph/scipy imports or the image breaks. The **game** image is the Rust `serve` binary on debian-slim, no Python at all; data is mounted, never baked in (it changes on a different cadence than the code). RSS and latency figures live in CODEBASE.md — **when the server starts loading something new, re-measure the peak and keep 2-3 GB of container-limit headroom**: a cgroup kill mid-load looks like a container that never comes up, with empty logs. Load-bearing:

- **`--host 0.0.0.0`.** The binary defaults to `127.0.0.1`, which inside a container is unreachable from the host.
- **`--trust-proxy` is required behind a proxy and unsafe without one.** Without it every request appears to come from the proxy's IP and the per-IP rate limiter throttles all users as one bucket; with it on a directly-exposed port, a client can forge `X-Forwarded-For` and defeat rate limiting entirely.
- **`--secure-cookies` only over HTTPS.** A Secure cookie is never sent over plain http, so setting it in local development silently breaks identity.
- **The healthcheck needs a long `start-period`.** The process accepts no connections until the CSR is built, so a default start period makes the orchestrator kill it in a loop before it ever comes up.
- **`--state` must point somewhere writable and persistent, and must not be the data dir.** `Registry::open` and `Identity::load_or_create` write `leaderboard.jsonl` and `.wiki-race-secret`, and they default to `--data` — which a deployment mounts read-only, so the process dies at startup and restarts forever. Traefik registers no router for a container in that state, so the symptom presents as "the hostname does not resolve" rather than as a write error. Losing the state volume invalidates every player cookie and the leaderboard with it.

## Repo conventions

- `.gitignore` excludes `*.md` except `README.md`, plus all of `/data/`, `*.parquet`, `*.csv`. Large artifacts and generated docs are intentionally untracked.
- Long-running GPU/CPU work is wrapped in `run_with_timer()`, which uses a thread (not a subprocess) because forking breaks CUDA context.
- Scripts that hit external state (`03_name_clusters.py`) save progress after every API call and resume from the on-disk JSON.
