# wiki-graph codebase

Every file, what it does, and what it produces. Written 16 August 2026 against
commit `e3097a4`.

Two independent programs share one data format:

- **Rust** parses the dump and serves the game. No Python at runtime.
- **Python** computes PageRank, layout and communities, and renders the map.
  No Rust involved.

They meet at Parquet files in `data/`. Nothing calls across the boundary.

```
enwiki-…-multistream.xml.bz2  (26.7 GB)
        │
        │  wiki-parser (Rust, ~50 min, 3.1 GB RSS)
        ▼
titles · redirects · edges · categories · article_sizes   (.parquet)
        │                                        │
        │  01_graph_compute.py (Python)          │
        ▼                                        │
   nodes.parquet                                 │
        │                                        │
        ├─▶ 02/03/05/06  stats, labels, poster   │
        └────────────┬───────────────────────────┘
                     ▼
              serve (Rust) — the game
```

---

## Rust: `src/`

| File | Lines | Role |
|---|---:|---|
| `main.rs` | 259 | Parser entry point. Runs both passes, prints the funnel, writes every artifact. |
| `dump.rs` | 266 | Streaming MediaWiki XML reader. Buffers are reused across pages, so memory is O(largest article), not O(dump). |
| `index.rs` | 282 | **Pass 1.** Every ns=0 page gets a raw id; redirect chains are resolved (max 4 hops, cycle-safe); only real articles get a dense `article_id`. |
| `edges.rs` | 363 | **Pass 2.** Normalize each link, resolve through redirects, drop red links. Also collects categories and article sizes. |
| `titles.rs` | 215 | Title normalization — MediaWiki's own rules. Decides whether two link strings are the same article. Also category extraction and maintenance-category filtering. |
| `wikitext.rs` | 318 | Comment/nowiki/ref/template stripping and `[[wikilink]]` extraction. Every removal is behind a flag. |
| `output.rs` | 217 | Parquet writers. The parser emits final artifacts directly, so nothing downstream re-reads a multi-GB CSV. |
| `graph.rs` | 687 | CSR link graph plus the search algorithms: bidirectional BFS, shortest-path counting, reverse distance maps. |
| `game.rs` | 814 | Game state: map coordinates, puzzle generation, search ranking, hub bans, categories, aliases, region naming. |
| `runs.rs` | 466 | Run issuing, path validation, compass charges, leaderboard. Never trusts a submitted score; re-walks the submitted path. |
| `identity.rs` | 186 | HMAC-signed anonymous player cookie. |
| `ratelimit.rs` | 111 | Per-IP token bucket. Hand-rolled — it is a HashMap and some arithmetic. |
| `lib.rs` | 20 | Exposes the above so the binaries share one definition of article identity. |

### Binaries

| File | Lines | Role |
|---|---:|---|
| `bin/serve.rs` | 1028 | The HTTP service. Holds the whole graph in memory; every response derives from the parser's Parquet, so the optimal path reported is optimal *in the world the player is playing in*. |
| `bin/pathfind.rs` | 86 | CLI shortest path between two articles. |

### Load-bearing details

- **`titles.rs::normalize_title` decides article identity.** Capitalization is
  applied only when the uppercase mapping is a single character — Rust's
  `to_uppercase()` turns `ß` into `SS`, which once merged the article `ß` into
  a redirect and deleted a real article.
- **`dump.rs` must handle `Event::GeneralRef`.** quick-xml ≥0.32 emits entity
  references separately; ignoring them deletes every `&` from titles
  (`AT&T` → `ATT`).
- **Categories are read from raw wikitext, not cleaned text.** Cleaning
  truncates at the first citation section and categories sit below it. Reading
  post-clean gave 30% coverage; raw gives 97%.
- **Parse errors and decompressor exit codes are fatal.** A truncated `.bz2`
  otherwise looks exactly like a clean end-of-dump.
- **`max_blocking_threads(8)`.** Each concurrent graph request holds a 72 MB
  PathFinder (130 MB while counting paths). Tokio's 512-thread default was
  37 GB and an instant OOM.

---

## Python: `python/`

| File | Lines | Machine | Role |
|---|---:|---|---|
| `common.py` | 293 | — | Single source of truth for paths, config defaults, cache fingerprinting. |
| `01_graph_compute.py` | 840 | **PC** | PageRank (phase 1), SFDP layout + Leiden (phase 2), merge + attach titles (phase 3). |
| `02_video_stats.py` | 92 | **PC** | Orphans, dead ends, reciprocity. OOM-killed on 15 GB. |
| `03_name_clusters.py` | 95 | laptop | Gemini community labels. **The only script needing an API key.** Now optional — categories name regions for free. |
| `04_app.py` | 232 | PC | Panel + Datashader interactive viewer. |
| `05_export_png.py` | 238 | PC | 4K poster export. `--color winner` colours each pixel by its dominant community. |
| `06_community_stats.py` | 91 | laptop | Per-community JSON. Runs in 3.5 s. |
| `07_incremental.py` | 92 | either | Cache status and reset. |
| `08_export_gephi.py` | 82 | laptop | Exports the high-PageRank core as Gephi CSVs for the LinLog test. |
| `09_pageviews.py` | 143 | laptop | Joins Wikimedia monthly pageviews onto article ids. |
| `diag_gpu.py` | 146 | PC | Bisects the cuGraph ForceAtlas2 segfault. Kept as a record; FA2 is dead. |

### Load-bearing details

- **A phase is skipped when its cache exists.** Editing phase logic has no
  effect until the cache is deleted.
- **`--reset-layout` keeps `cache_sfdp_raw.npz`;** `--reset-sfdp` discards it.
  The raw positions cost ten hours and do not depend on `community.resolution`.
- **igraph accepts numpy `(E, 2)` arrays directly.** `.tolist()` first
  materializes millions of Python lists and OOMs.
- **No `groups=` on `sfdp_layout`.** graph-tool pairs group attraction with
  group *repulsion*, which shatters one component into islands.
- **`run_with_timer` cannot tick during igraph/graph-tool calls** — they hold
  the GIL. A frozen `00:00:00` is not a hang.

---

## Data files

### Parser outputs — the contract everything else reads

| File | enwiki size | Contents |
|---|---:|---|
| `titles.parquet` | 96.6 MB | `id` (dense 0..N-1), `title`. Real articles only. |
| `redirects.parquet` | 156.6 MB | `article_id`, `alias`. 12M aliases. |
| `edges.parquet` | 803.0 MB | `src`, `dst` int32. Deduped, no self-loops. |
| `categories.parquet` | *new* | `article_id`, `category`. ≤6 per article, maintenance filtered. |
| `article_sizes.parquet` | *new* | `id`, `bytes`. One u32 per article, ~29 MB. |

*The last two exist only after a parse from commit `3827ef5` or later.*

### Pipeline caches and outputs

| File | Size | Contents |
|---|---:|---|
| `cache_metrics.parquet` | 77.4 MB | PageRank + in-degree. Phase 1. |
| `cache_sfdp_raw.npz` | **3.34 GB** | Raw SFDP positions. **The most expensive artifact in the project** — ten hours. |
| `cache_layout.parquet` | 88.6 MB | Placed coordinates + community. Phase 2. |
| `nodes.parquet` | 206.7 MB | `vertex`, `x`, `y`, `community`, `pagerank`, `degree`. The contract for scripts 02–06 and the game's map. |
| `manifest.json` | 348 B | Input + config fingerprint. Refuses to mix caches from different inputs. |
| `community_stats.json` | — | Per-community sizes and exemplars. |
| `community_labels.json` | — | LLM region names. Optional; categories supersede it. |
| `pageviews.parquet` | — | `id`, `views`. Optional, external data. |

### Which map is which

| File | What it is |
|---|---|
| `nodes_backbone.parquet` | 10% core laid out, tail propagated. Superseded. |
| `nodes_res1.parquet` | Full layout, 25 communities. Regions too coarse. |
| `resrun/nodes.parquet` | Full layout, 126 communities. **The one to ship.** |

### Runtime state — back this up

| File | Contents |
|---|---|
| `leaderboard.jsonl` | Append-only accepted runs. |
| `.wiki-race-secret` | HMAC key behind the identity cookie. Losing it logs every player out permanently. |

Both live in `--state`, which **must not** be the data directory — deployments
mount that read-only and the process dies at startup otherwise.

---

## Measured numbers

| | |
|---|---|
| Articles / edges | 7,219,290 / 231,681,569 |
| Parse | 46.4 min, 3.10 GB peak RSS |
| Symmetrized edges | 419,049,910 in 26 s |
| Full SFDP layout | ~10 h, 7,216,559 articles placed (99.96%) |
| Leiden re-run at resolution 6 | **14 min**, reusing cached positions |
| Server RSS | 3.79 GB, flat under load |
| Bidirectional BFS | 22 ms |
| `/api/map` (45k points) | 15 ms, 1.72 MB, ETag-cached |
| Article fetch | 3.7 ms, 272/s sustained |
| Compass BFS | 1.4 s first per goal, then cached |

---

## Things that are true and easy to forget

- **A new dump means re-running everything.** Ids are dense `0..N-1` in parse
  order, so one added article shifts every id after it. Anything storing an id
  — leaderboard rows, saved puzzles — becomes silently wrong.
- **`cugraph.force_atlas2` segfaults** on a five-vertex graph against driver
  580. Not a scale problem. `layout.backend: "auto"` cannot rescue it because a
  segfault is not a Python exception.
- **The layout has no structure below continent scale.** Four graph sizes from
  36k to 7.2M nodes all produced featureless discs. Communities *are* real
  (median spread 0.44× the map), but zooming reveals nothing.
- **Map distance is not a usable hint.** Measured at a 1.18× lift over
  guessing; articles with real force-layout positions score 1.23×, so a better
  layout does not rescue it.
- **72.3% of playable pairs are par 3.** Rejection sampling cannot produce a
  difficulty curve; a precomputed distance matrix is the only practical route.
- **The page is embedded with `include_str!`.** A broken `static/index.html`
  cannot fail the build — it ships. Two tests in `bin/serve.rs` guard against
  that after a slice-based edit once silently deleted the whole custom-race
  picker.
