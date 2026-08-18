# wiki-graph codebase

Every file, what it does, and what it produces. Written 16 August 2026, last
swept 17 August 2026.

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
| `extras.rs` | ~400 | The v3 template extractions from raw wikitext: {{Short description}}, disambiguation flags, first infobox kind, {{coord}} (preferring display=title), featured/good flags. Heuristic by design; absence degrades to pre-v3 behaviour. |
| `titles.rs` | 215 | Title normalization — MediaWiki's own rules. Decides whether two link strings are the same article. Also category extraction and maintenance-category filtering. |
| `wikitext.rs` | 318 | Comment/nowiki/ref/template stripping and `[[wikilink]]` extraction. Every removal is behind a flag. |
| `output.rs` | 217 | Parquet writers. The parser emits final artifacts directly, so nothing downstream re-reads a multi-GB CSV. |
| `graph.rs` | ~800 | CSR link graph plus the search algorithms: bidirectional BFS, meet-in-the-middle shortest-path counting (60-90x the single-direction version; oracle-tested against it), reverse distance maps. |
| `game.rs` | ~1100 | Game state: map coordinates, puzzle generation, search ranking, hub bans, categories, aliases, region naming. Search runs on a prebuilt index (exact by binary search, popularity-ordered scan with early exit) — sub-ms for typical queries vs the old ~100 ms full scan. |
| `pools.rs` | ~560 | Precomputed puzzle pools: parallel BFS bucketing by (difficulty, par) with reservoir sampling, route counting on survivors only, parquet round-trip with a dump fingerprint that refuses stale files, weighted draws (hard from few-routes pairs, easy from many). |
| `duel.rs` | ~180 | Head-to-head rooms, **dark**: mounted only under `--enable-duels`, no UI references it. Read-aloud room codes, presence refereed server-side, broadcast relay per room — kilobytes each on the same process. |
| `runs.rs` | 466 | Run issuing, path validation, compass charges, leaderboard. Never trusts a submitted score; re-walks the submitted path. |
| `identity.rs` | 186 | HMAC-signed anonymous player cookie. |
| `ratelimit.rs` | 111 | Per-IP token bucket. Hand-rolled — it is a HashMap and some arithmetic. |
| `lib.rs` | 20 | Exposes the above so the binaries share one definition of article identity. |

### Binaries

| File | Lines | Role |
|---|---:|---|
| `bin/serve.rs` | ~1500 | The HTTP service. Holds the whole graph in memory; every response derives from the parser's Parquet, so the optimal path reported is optimal *in the world the player is playing in*. |
| `bin/pathfind.rs` | 86 | CLI shortest path between two articles. |
| `bin/static_export.rs` | ~280 | Emits the whole game as a static file tree (shards, search buckets, the page, and dailies/rounds pre-generated a year ahead by default — ~2 KB/day, so `--days 3650` is a decade). The page's `data-static` adapter consumes it and derives "today" from the client clock. Simple English: 2 s; enwiki: ~10 min, 8.3 GB, ~11k files. |
| `bin/pools.rs` | ~110 | Generates `pools.parquet` on the PC (~2 h at enwiki scale, once per dump; 32 min measured on Simple English). The server picks it up at startup; without it, puzzle generation falls back to rejection sampling unchanged. |

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
- **Run ids are STRINGS on the wire.** They span the full u64 range, and
  JavaScript parses JSON numbers as doubles — everything past 2^53 rounds
  silently, which broke compass/routes/submit intermittently for as long as
  each existed (curl and python round-trip exactly, so no server-side test
  could see it). Any future id or large count that a browser echoes back
  must ship as a string.
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
| `04_app.py` | ~380 | PC | Panel + Datashader interactive viewer. Click any point (or a search hit) to highlight its incoming/outgoing links, isolate one region, inspect degree and neighbours. Edge highlighting wants `edges.parquet` and ~3 GB extra; degrades to the plain raster without it. Pure-numpy grid index for click resolution — the viewer image bans scipy. |
| `05_export_png.py` | 238 | PC | 4K poster export. `--color winner` colours each pixel by its dominant community. |
| `06_community_stats.py` | 91 | laptop | Per-community JSON. Runs in 3.5 s. |
| `07_incremental.py` | 92 | either | Cache status and reset. |
| `08_export_gephi.py` | 82 | laptop | Exports the high-PageRank core as Gephi CSVs for the LinLog test. |
| `09_pageviews.py` | 143 | laptop | Joins Wikimedia monthly pageviews onto article ids. |
| `diag_gpu.py` | 146 | PC | Bisects the cuGraph ForceAtlas2 segfault. Kept as a record; FA2 is dead. |

### Endpoints the game serves

| Route | Cost | Notes |
|---|---|---|
| `/api/meta` | 17 ms | Article and edge counts, map bounds, whether pools and pageviews are loaded. |
| `/api/regions` | free | Region names, from categories or the LLM file. |
| `/api/map` | 15 ms | 45k points, 1.72 MB (~a third gzipped), ETag so repeats are free. |
| `/api/article/{id}` | 3.7 ms | Links with categories, short description, infobox kind, disambig/featured flags, read count, aliases and byte size — each field absent until its data file exists. |
| `/api/search` | sub-ms typical | Prebuilt index over titles AND redirect aliases ("NYC" finds New York City), deduped to the best entry per article. Worst case (no match) is still a full pass, but allocation-free. |
| `/api/path` | 22 ms | Bidirectional BFS. |
| `/api/puzzle` | 41 ms | Also accepts `from`/`to` (ids) or `from_title`/`to_title`. |
| `/api/daily` | 41 ms | Seeded by the day. |
| `/api/compass` | 1.4 s first per goal | Then cached; charges scale with par. |
| `/api/routes` | ~1-10 ms | Shortest-route count (meet-in-the-middle; was 31-142 ms single-direction), asked after the race. |
| `/api/dist` | free | Clicks histogram of one daily board — the "38% made par" line. |
| `/api/course` | ~9x41 ms | 3- or 9-hole round: exact-par holes from the pools, seeded per day, each hole a normal issued run. |
| `/api/submit` · `/api/leaderboard` | — | Re-walks the submitted path; never trusts a score. |
| `/api/duel/new` · `/api/duel/ws/{code}` | dark | Mounted only under `--enable-duels`. Room create + websocket relay; duels never touch the leaderboard. |

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
| `categories.parquet` | 258.6 MB | `article_id`, `category`. 28.1M rows (~3.9/article), maintenance filtered. |
| `article_sizes.parquet` | 36.6 MB | `id`, `bytes`. One u32 per article. |

| `short_descriptions.parquet` | sparse | `id`, `description` — {{Short description}} gloss (v3; rare on Simple English, most articles on enwiki). |
| `infobox_types.parquet` | sparse | `id`, `kind` — first infobox name ("person", "film"); 46% coverage on Simple English. |
| `article_flags.parquet` | dense | `id`, `flags` bitmask — 1 disambig, 2 featured, 4 good. Disambig pages are auto-excluded from generated race endpoints and from the pools. |
| `coords.parquet` | sparse | `id`, `lat`, `lon` from {{coord}} — visualizer/content material, not served by the game. |

*Categories, sizes and the v3 files all come from re-parses that reproduce
`titles.parquet` and `edges.parquet` byte-for-byte — the parse is deterministic,
so ids are stable across re-parses of the same dump.*

### Pipeline caches and outputs

| File | Size | Contents |
|---|---:|---|
| `cache_metrics.parquet` | 77.4 MB | PageRank + in-degree. Phase 1. |
| `cache_sfdp_raw.npz` | **3.34 GB** | Raw SFDP positions. **The most expensive artifact in the project** — ten hours. |
| `cache_layout.parquet` | 88.6 MB | Placed coordinates + community. Phase 2. |
| `nodes.parquet` | 206.7 MB | `vertex`, `x`, `y`, `community`, `pagerank`, `degree`. The contract for scripts 02–06 and the game's map. Canonical copy carries res-6 communities (2,970 distinct). |
| `manifest.json` | 348 B | Input + config fingerprint. Refuses to mix caches from different inputs. |
| `community_stats.json` | — | Per-community sizes and exemplars. |
| `community_labels.json` | — | LLM region names. Optional; categories supersede it. |
| `pageviews.parquet` | 32.6 MB | `id`, `views`. Optional, external (09_pageviews.py; July 2026: 6.56B en views, 99.8% article coverage, ~10 min to regenerate from a cached dump). |
| `endpoint_deny.txt` | — | Optional editorial curation: one title per line, resolved through redirects; denied articles never appear as generated race endpoints. |
| `pools.parquet` | a few MB | `difficulty`, `par`, `src`, `dst`, `routes`. Optional; from the `pools` binary. Carries the graph's article/edge counts as metadata — the server refuses it against a different dump. |

### Which map is which

| File | What it is |
|---|---|
| `nodes.parquet` | Full layout, res-6 communities (2,970; 126 over 10k members). **The canonical map.** |
| `nodes_backbone.parquet` | 10% core laid out, tail propagated. Superseded. |
| `resrun/nodes_res1.parquet` | Full layout, res-1 communities (2,760). Regions too coarse. |

### Runtime state — back this up

| File | Contents |
|---|---|
| `leaderboard.jsonl` | Append-only accepted runs. |
| `.wiki-race-secret` | HMAC key behind the identity cookie. Losing it logs every player out permanently. |
| `analytics.jsonl` | One JSON delta line per hour: per-endpoint counters plus hashed-IP daily uniques. Capacity planning, not accounting — losable. |

Both live in `--state`, which **must not** be the data directory — deployments
mount that read-only and the process dies at startup otherwise.

---

## Deployment files

| File | What it deploys |
|---|---|
| `Dockerfile` + `docker-compose.yml` | The Python Panel/Datashader viewer. Visualization dependencies only. |
| `Dockerfile.game` | The Rust binary on debian-slim. Multi-stage; `static/` is a **build** dependency because the page is embedded with `include_str!`. |
| `docker-compose.game.yml` | The game behind an existing Traefik. Joins an external network, carries routing labels, publishes only on loopback. |
| `docker-compose.game.direct.yml` | The game on a host port, no proxy. A separate complete file, not an override — Compose appends sequences, so an override adding a port gives two mappings and a conflict, and a service's network list cannot be subtracted from. |
| `compose.local.yml` | Gitignored per-host overrides, so the tracked files never need editing on a server. |
| `.env.example` | Every variable, with the real values discovered on the LAN Traefik (`https`, not `websecure`). |

Both game compose files accept `IMAGE=` to run a prebuilt image instead of
compiling on the server. The binary is 12.5 MB and links only libc, libm and
libgcc, so there is nothing to gain from building where it runs — and building
there costs 2–3 GB of RAM plus a ~1.5 GB toolchain image, which is most of the
headroom on a 6 GB box.

---

## Measured numbers

| | |
|---|---|
| Articles / edges | 7,219,290 / 231,681,569 |
| Parse | 46.4 min, 3.10 GB peak RSS |
| Symmetrized edges | 419,049,910 in 26 s |
| Full SFDP layout | ~10 h, 7,216,559 articles placed (99.96%) |
| Leiden re-run at resolution 6 | **14 min**, reusing cached positions |
| Server RSS | 7.1 GB, flat under load (3.79 GB before categories/aliases/sizes and the title+alias search index; `--no-alias-search` saves ~450 MB) |
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
  Size it at 20,000 articles (400 MB) not 50,000 (2.5 GB) — the larger table
  would thrash a 6 GB server, and the tail of the 50k pool is where races like
  "Legong → Bara Jumla" come from. It is a per-dump artifact built on the PC in
  ~40 min, never on the server, where it would take ~7 hours and peg the box.
- **One BFS per source yields distance *and* route count together**, so both
  come out of the same pass at no extra cost.
- **The page is embedded with `include_str!`.** A broken `static/index.html`
  cannot fail the build — it ships. Two tests in `bin/serve.rs` guard against
  that after a slice-based edit once silently deleted the whole custom-race
  picker.
