# Wikipedia Graph Pipeline

Parse, compute, and visualize the English Wikipedia link network — a Rust two-pass
parser that resolves article identity, then a GPU-accelerated Python pipeline that
computes PageRank, communities, and a force-directed layout.

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
        │    Phase 2  ForceAtlas2 + Leiden      (undirected)
        │    Phase 3  merge + attach titles
        ▼
   nodes.parquet
        │
        ├──▶ 02_video_stats.py     orphans, dead ends, reciprocity
        ├──▶ 03_name_clusters.py   LLM community labels
        ├──▶ 04_app.py             Panel + Datashader viewer
        ├──▶ 05_export_png.py      high-resolution PNG
        └──▶ 06_community_stats.py per-community JSON
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
| ForceAtlas2 layout | Undirected | Force-directed physics needs symmetric attraction |
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

# 2. Environment (RAPIDS must come from mamba, not pip)
bash scripts/setup-gpu-env.sh   # one conda solve; see the script for the raw command
mamba activate rapids-env
pip install google-genai            # the only dep not on conda-forge
export KVIKIO_COMPAT_MODE=ON        # Fedora: disable GPU Direct Storage

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

The pipeline runs **without a GPU** — PageRank falls back to sparse power iteration
and layout falls back to a coarsened community layout — but the GPU path is what
produces the real ForceAtlas2 map.

### The two halves want different machines

The parser is CPU- and I/O-bound and peaks around a few hundred MB of RAM; Phase 2
is the part that wants a GPU. Since the parser's outputs are ~1–2 GB of Parquet
against a 27 GB dump, **parse wherever the dump already is and copy the Parquet**:

```bash
# on the machine holding the dump
./target/release/wiki-parser <dump>.xml.bz2 --out data

# then move ~1-2 GB instead of 27 GB
rsync -avP data/{titles,redirects,edges}.parquet desktop:~/wiki-graph/data/
```

The GPU machine then needs neither the Rust toolchain nor the dump.

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
  backend: "auto"       # "auto", "gpu", "cpu"
  max_iter: 500         # ForceAtlas2 iterations
  cpu_method: "coarsened"  # or "drl"/"fr" for a direct layout on small graphs
community:
  objective: "modularity"
  top_n: 20
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

## Docker deployment

```bash
WEBSOCKET_ORIGIN=wiki.example.com docker-compose up -d
# http://localhost:5006
```

The image carries visualization dependencies only — results are precomputed into
Parquet, so no GPU, RAPIDS or igraph is needed to serve.

## Acknowledgments

Inspired by ["I Made a Graph of Wikipedia... This Is What I Found"](https://www.youtube.com/watch?v=JheGL6uSF-4)
by adumb. This is an independent implementation using Rust, RAPIDS and igraph.
