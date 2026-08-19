# Wikipedia Graph Pipeline

Parse, compute, and visualize the English Wikipedia link network — a Rust two-pass
parser that resolves article identity, then a Python pipeline that computes
PageRank, communities and a force-directed layout, plus a Rust game server that
plays wiki-race on the same graph.

Everything runs on CPU. There is no working GPU path — see
[Layout](#layout-is-cpu-sfdp) for why.

## Architecture

```
enwiki-*-pages-articles-multistream.xml.bz2   (~27 GB)
        │
        │  wiki-parser  — pass 1: titles + redirect resolution
        │                 pass 2: normalize, resolve, drop red links
        ▼
   titles.parquet  ·  redirects.parquet  ·  edges.parquet     (int32)
        │
        │  01_graph_compute.py
        │    Phase 1  PageRank + in-degree      (directed)
        │    Phase 2  SFDP layout + Leiden      (undirected)
        │    Phase 3  merge + attach titles
        ▼
   nodes.parquet
        │
        ├──▶ 02_video_stats.py     orphans, dead ends, reciprocity
        ├──▶ 03_name_clusters.py   LLM community labels
        ├──▶ 04_app.py             Panel + Datashader viewer
        ├──▶ 05_export_png.py      high-resolution PNG
        ├──▶ 06_community_stats.py per-community JSON
        └──▶ 08_export_gephi.py    core as Gephi CSVs

   titles/edges/nodes.parquet
        │
        └──▶ serve (Rust)          the wiki-race game, no Python at all
```

### The parser decides what the graph means

Most of the difficulty in this project is not computation, it is **article identity**.
A naive parser emits link *strings*; this one emits article *ids*. It:

- reads the authoritative `<ns>` element instead of guessing namespaces from colons
  (so `Star Trek: The Next Generation` survives as an article),
- applies MediaWiki's title rules — underscores→spaces, whitespace collapse,
  `#anchor` stripping, first-letter capitalization,
- resolves redirect chains, so `USA` and `United States` are one node,
- and **drops red links**: a target naming no real article is not a node.

On full English Wikipedia that discards **a third of all raw links** as noise:

```
articles           7,219,290
redirects         11,996,595   (27,913 broken)

raw links seen   354,808,900
  duplicates     102,470,692  (28.9%)   case variants, redirect collapsing
  red links       15,465,145   (4.4%)
  namespace        4,948,037   (1.4%)
  self links         243,457
edges            231,681,569
```

Skipping this is why an earlier version of this pipeline produced 28M "vertices"
and 482M edges for a ~7M article encyclopedia, and then ran out of VRAM trying
to lay them out.

The red-link rate is strongly scale-dependent — **29.9% on Simple English versus
4.4% here** — because a large wiki has far fewer links pointing at articles that
do not exist. Duplicates move the other way (15.6% → 28.9%): redirect collapsing
does more work at scale.

Sanity check on the result — the most linked-to articles:

```
263,903  Association football      125,364  AllMusic
246,090  United States             119,818  India
218,891  World War II              117,001  World War I
193,994  The New York Times        107,257  The Guardian
146,419  New York City             107,018  Germany
```

### Directed vs undirected

| Phase | Graph | Why |
|-------|-------|-----|
| PageRank, in-degree | Directed | Link direction is what makes an article important |
| SFDP layout | Undirected | Force-directed physics needs symmetric attraction |
| Leiden communities | Undirected | Community structure is mutual connectivity |

## Getting the dump

Use the **multistream** file and a **dated** directory rather than `latest`, so
your numbers stay reproducible.

```bash
mkdir -p data/dumps && cd data/dumps
wget -c https://dumps.wikimedia.org/enwiki/20260801/enwiki-20260801-pages-articles-multistream.xml.bz2
sha1sum -c <<< "dd27f408e60d3bc864d42547fb0a0d7408249c13  enwiki-20260801-pages-articles-multistream.xml.bz2"
```

**Multistream is not just a convenience.** It is a concatenation of independent
bz2 streams, so a parallel decompressor can split it across cores. Measured on
this dump:

| | throughput |
|---|---|
| `pbzip2` on multistream | **58.9 MB/s** |
| `bzip2`, single stream | 24.3 MB/s |

Decompression is the parser's bottleneck, so this is a 2.4× difference in
wall-clock time on a multi-hour job.

> **`pbzip2` segfaults partway through the full enwiki multistream dump**
> (observed at ~4.2M pages, on a file whose sha1 matches the published
> checksum). `pbzip2` only fully supports files it compressed itself.
> Prefer **`lbzip2`**, which handles arbitrary multi-stream bz2 in parallel,
> and fall back to plain `bzip2` — slower, but correct:
>
> ```bash
> ./target/release/wiki-parser <dump>.xml.bz2 --out data --decompressor bzip2
> ```
>
> The parser treats a non-zero decompressor exit as fatal, so this surfaces as
> a hard error rather than a silently truncated graph.

Wikimedia also publishes `pagelinks.sql.gz`, where the link table is already built.
This project parses wikitext instead on purpose: `pagelinks` records links **after
template expansion**, so every navbox becomes a clique — every US President linked
to every other — which wrecks community detection and dominates the layout.

**For development, use Simple English Wikipedia** (~354 MB, 284k articles). It is a
real graph with redirects, red links and genuine communities, and it runs end to end
on a laptop in a couple of minutes:

```bash
wget -c https://dumps.wikimedia.org/simplewiki/latest/simplewiki-latest-pages-articles.xml.bz2
```

Note that simplewiki is a *single* bz2 stream, so it cannot be parallel-decompressed
and takes ~120 s despite being 75× smaller than enwiki.

## Quick start

```bash
# 1. Parse the dump into an integer link graph
cargo build --release

# development — Simple English, ~2 min
./target/release/wiki-parser data/dumps/simplewiki-latest-pages-articles.xml.bz2 --out data/simple

# the real thing
./target/release/wiki-parser data/dumps/enwiki-20260801-pages-articles-multistream.xml.bz2 --out data

# 2. Environment. graph-tool does the layout and is the one hard requirement;
# RAPIDS is optional and only accelerates PageRank.
# One line on purpose: pasting a backslash-continued command mangles it.
mamba create -n wiki -c conda-forge python=3.11 graph-tool python-igraph scipy pyarrow pandas pyyaml tqdm datashader holoviews bokeh colorcet panel pillow -y
mamba activate wiki
pip install google-genai            # the only dep not on conda-forge

# Optional: RAPIDS, for GPU PageRank only. The layout does not use it.
# mamba install -c rapidsai -c conda-forge -c nvidia rapids=24.04 cuda-version=12.2
# export KVIKIO_COMPAT_MODE=ON      # Fedora: disable GPU Direct Storage

python -c "import graph_tool.all, igraph, scipy, pandas, pyarrow, yaml; print('ok')"

# 3. Compute
python python/01_graph_compute.py
python python/02_video_stats.py
GEMINI_API_KEY=... python python/03_name_clusters.py
python python/06_community_stats.py

# 4. View / export
panel serve python/04_app.py --show
python python/05_export_png.py --width 4096 --height 2160
```

Set `pipeline.data_dir` (or pass `--data-dir`) to pick which of the two you work on.

### Layout is CPU SFDP

`cugraph.force_atlas2` **segfaults and is not usable.** It is a legacy algorithm on
cuGraph's old C++ API and dies inside `cuCtxGetDevice` against modern NVIDIA drivers
— verified on an RTX 4080 / driver 580 on a *five vertex* graph, so it is not a
scale or VRAM problem. A segfault is not a Python exception, so `layout.backend:
"auto"` cannot fall back from it; the process simply dies. The default is `"cpu"`.

The real layout is graph-tool's SFDP (Hu's multilevel force-directed algorithm):
maintained C++, OpenMP-parallel, no driver dependency. Measured at full scale:
**7,216,559 of 7,219,290 articles laid out in about ten hours** on 32 threads.

**What it does not do is produce a detailed map.** Four graph sizes were laid out
and every one came out a featureless disc:

| core | nodes | avg degree | max/mean density | median/mean |
|---|---:|---:|---:|---:|
| 0.5% | 36,093 | 154 | 4.4× | 0.63 |
| 1% | 72,185 | 153 | 3.8× | 0.63 |
| 10% | 721,929 | 231 | 2.2× | 0.98 |
| full | 7,216,559 | 58 | 3.7× | 0.56 |
| *uniform random disc* | — | — | *1.6×* | *1.25* |

Zooming into the full layout shows uniform noise: no filaments, no cores, no voids.
The dynamic range that exists is a radial falloff, not structure. Sweeping SFDP's
`C` and `p` made it worse, not better.

The untested lever is **LinLog mode**, which ForceAtlas2 has and SFDP does not, and
which exists specifically to separate clusters. `08_export_gephi.py` writes the
high-PageRank core as Gephi CSVs so that can be tested interactively rather than
overnight.

### The two halves want different machines

The parser is CPU- and I/O-bound and peaks around a few hundred MB of RAM. Phase 2
wants cores and RAM — the Leiden pass alone builds a 419M-edge igraph, roughly
20–25 GB. Since the parser's outputs are ~1–2 GB of Parquet against a 27 GB dump,
**parse wherever the dump already is and copy the Parquet**:

```bash
# on the machine holding the dump
./target/release/wiki-parser <dump>.xml.bz2 --out data

# then move ~1-2 GB instead of 27 GB
rsync -avP data/{titles,redirects,edges}.parquet desktop:~/wiki-graph/data/
```

The compute machine then needs neither the Rust toolchain nor the dump.

## Parser flags

| Flag | Effect |
|------|--------|
| `--strip-templates` | Drop `{{...}}` bodies, excluding infobox links. Off by default: navboxes are transcluded and so contain no links in raw wikitext, meaning templates mostly contribute *infobox* links, which are real relations. |
| `--strip-refs` | Drop `<ref>...</ref>` citation bodies |
| `--keep-citation-sections` | Keep References / External links / Further reading. These are cut by default; `See also` is always kept, being editorially curated related articles. |
| `--titles-only` | Stop after pass 1 |
| `--decompressor` | Override the bz2 decompressor (default: lbzip2 → pbzip2 → bzip2) |

HTML comments and `<nowiki>` are always removed — they do not render, so links
inside them are not links.

## Configuration

`config.yaml` tunes the compute and display stages; what counts as a *link* is
decided by the parser flags above, not here.

```yaml
pipeline:
  sample_ratio: 1.0     # sampling disconnects the graph; use simplewiki instead
layout:
  backend: "cpu"        # NOT "auto" — see Layout above
  cpu_method: "sfdp"    # or "coarsened"/"drl"/"fr"
  backbone_frac: 0.10   # 0 lays out every article; 0.10 lays out the top 10%
                        # by PageRank and places the rest at their neighbours
community:
  objective: "modularity"
  resolution: 6.0       # the map's granularity dial — 6.0 is the shipped
                        # enwiki value (2,970 communities). 1.0 gives ~25 usable
                        # communities for 7.2M articles, which is too coarse to
                        # read as regions; raise max_categories with it.
  top_n: 20             # communities sent to the LLM for labelling
```

## Cache management

Each phase writes a cache and is skipped if that cache exists. A `manifest.json`
fingerprints the inputs, so a cache built from different data is refused rather
than silently merged.

```bash
python python/07_incremental.py            # staleness check + status
python python/01_graph_compute.py --reset  # recompute from scratch
```

## Notes on scale

- **The parser streams.** Page buffers are reused, so memory is O(largest article)
  plus the title index — measured at 118 MB peak on Simple English Wikipedia, and
  expected in the low single-digit GB on full English. It runs on a laptop.
- **Vertex count comes from `titles.parquet`**, not from the edge list, so articles
  with no links in either direction remain nodes.
- **Deduplication is exact.** Targets are deduplicated per page, which is complete
  global deduplication since edge (A,B) can only be produced by page A.
- **Datashader's categorical aggregate costs `width × height × categories × 4`
  bytes.** The viewer and exporter cap categories (default 24, rest as "Other");
  an uncapped render at 4K with thousands of communities would ask for hundreds
  of gigabytes.

## WikiGolf — the game

A second Rust binary serves **WikiGolf** off the same Parquet: golf across
Wikipedia, reaching the goal article in the fewest clicks. **Par is the
shortest route that actually exists** — the server computes it on its own
copy of the graph, so the optimal it reports is optimal in the world the
player is playing in. Par ⛳, Bogey, Double bogey.

```bash
cargo build --release --bin serve
./target/release/serve --data data --state ./state --host 0.0.0.0 --port 8080
```

What it serves: a seeded **daily** in three difficulties with streaks and a
Wordle-style share grid; a 3- or 9-hole **round** with one scorecard; random
races drawn from precomputed **pools** with route-count-aware difficulty; a
rationed exact-distance **compass**; title+alias **search** (sub-ms); links
annotated with categories, short descriptions, read counts and infobox
glyphs; and the force-layout **map** with the race drawn across it.

Measured at full enwiki scale: **7.1 GB resident** (~2 min to load; the
title+alias search index is the largest optional slice — `--no-alias-search`
saves ~450 MB), then **22 ms per bidirectional BFS**. Every data file except
`titles`/`edges` is optional and degrades gracefully.

**Head-to-head duels exist but ship dark**: rooms, spoken-aloud codes and a
websocket relay are compiled in, mounted only under `--enable-duels`, and no
UI references them yet — multiplayer costs kilobytes per room, so the
engineering is banked while the feature waits for players.

## Docker deployment

Two independent images that share no code.

**Viewer** — Panel + Datashader, visualization dependencies only:

```bash
VIEW_HOST=map.example.com docker compose -f docker-compose.yml up -d
```

**Game** — the Rust binary on debian-slim, no Python, behind an existing Traefik:

```bash
RACE_HOST=race.example.com docker compose -f docker-compose.game.yml up -d --build
```

See `.env.example` for the variables. Two flags are load-bearing: `--trust-proxy`
is required behind a proxy and unsafe without one (a client can otherwise forge
`X-Forwarded-For` past the rate limiter), and `--state` must point somewhere
writable that is **not** the data directory, which deployments mount read-only.

Data is mounted, never baked into either image: it is ~1.6 GB and changes on a
completely different cadence than the code.

## The static build — the whole game as files

`static_export` emits WikiGolf as a tree of plain files: article shards,
the map, compass files, and every daily and round pre-generated N days
ahead with pars and route counts baked in. The page detects the tree and
swaps its fetch layer — no server process anywhere. (Search is live-only:
the static build hides the jump bar and ships no search index.)

```bash
cargo run --release --bin static_export -- --data data --out static_site
```

Host it free on Cloudflare Pages (`wrangler pages deploy static_site`), put a
domain on it, done: ~$10/year total. What the files cannot answer is hidden
rather than broken: custom-pair races, the compass and leaderboards need the
live server. Dailies are pre-generated a year ahead by default (each costs
~2 KB; `--days 3650` buys a decade) and advance on the visitor's clock; past
the horizon the page says the build needs a re-export.

## Acknowledgments

Inspired by ["I Made a Graph of Wikipedia... This Is What I Found"](https://www.youtube.com/watch?v=JheGL6uSF-4)
by adumb (March 2024). That project's source is distributed through
[GitHub Sponsors](https://github.com/sponsors/adumb-codes/) rather than published,
and the method is not documented publicly, so nothing here is derived from it.

This is an independent implementation: Rust for parsing and the game, graph-tool
and igraph for layout and communities, Datashader for rendering.

## Run it yourself

The full game runs from one container and ~1.8 GB of data (attached to the
GitHub release; CC BY-SA 4.0, derived from the English Wikipedia dump of
1 Aug 2026):

```bash
gh release download v1.0.0 --pattern '*.parquet' --dir data
docker run -d -p 8080:8080 -v ./data:/data:ro ghcr.io/OWNER/wikigolf:latest
# ~7 GB RAM, ~2 minutes to load, then http://localhost:8080
```

`titles` + `edges` are required; the rest degrade gracefully. Add
`--no-alias-search` to the command line inside the container to save ~450 MB.

## License

[AGPL-3.0-or-later](LICENSE). Run it, fork it, learn from it; if you host a
modified version as a service, its users get the source too. The
Wikipedia-derived data is CC BY-SA and is not part of this repository.
