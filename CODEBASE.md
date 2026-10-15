# wiki-graph codebase

Every file, what it does, and what it produces. Written 16 August 2026, last
swept 15 September 2026.

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
| `duel.rs` | ~180 | Head-to-head rooms, mounted only under `--enable-duels`. Read-aloud room codes, presence refereed server-side, broadcast relay per room — kilobytes each on the same process. The page shows the duel controls when `/api/meta` reports `duels: true`; everyone adopts the room's board as a normal issued run, so duels never touch the leaderboard. |
| `runs.rs` | 466 | Run issuing, path validation, compass charges, leaderboard. Never trusts a submitted score; re-walks the submitted path. |
| `identity.rs` | 186 | HMAC-signed anonymous player cookie. |
| `ratelimit.rs` | 111 | Per-IP token bucket. Hand-rolled — it is a HashMap and some arithmetic. |
| `lib.rs` | 20 | Exposes the above so the binaries share one definition of article identity. |

### Binaries

| File | Lines | Role |
|---|---:|---|
| `bin/serve.rs` | ~1500 | The HTTP service. Holds the whole graph in memory; every response derives from the parser's Parquet, so the optimal path reported is optimal *in the world the player is playing in*. |
| `bin/pathfind.rs` | 86 | CLI shortest path between two articles. |
| `bin/static_export.rs` | ~280 | Emits the whole game as a static file tree (shards, the page, and dailies/rounds pre-generated a year ahead by default — ~2 KB/day, so `--days 3650` is a decade; no search buckets — the jump search bar is hidden on static, so `/api/search` is live-only). Puzzle endpoints (dailies, rounds, random pools) carry full start/goal article objects (desc, in-degree, views, x/y) so the goal card and the map's red goal dot match the live server. The page's `data-static` adapter consumes it and derives "today" from the client clock. Also emits `robots.txt` (data dirs disallowed), a one-URL `sitemap.xml`, `404.html` (Cloudflare Pages serves it automatically), and the PWA set: `manifest.webmanifest`, four PNG icons (from `static/`, generated from the ⛳ emoji), and `sw.js` — a service worker whose cache name embeds the export timestamp, so each export invalidates the previous cache wholesale; navigations are network-first, the data tree cache-first. The page registers it only under `data-static`. Every puzzle carries one baked shortest route (ban honoured) for the post-race reveal. Simple English: ~60 s; enwiki: ~34 min, 17 GB, ~14.2k files. |
| `bin/pools.rs` | ~110 | Generates `pools.parquet` on the PC (~2 h at enwiki scale, once per dump; 32 min measured on Simple English). The server picks it up at startup; without it, puzzle generation falls back to rejection sampling unchanged. |
| `e2e/` | 5 tests | Playwright smoke suite against a DEPLOYED site (`BASE_URL`, default production) — CI can't build the 18 GB tree. Each test is a real playtest regression: give-up must keep the route reveal and never print "could not check", a lagged double-click counts once, random goals carry real in-degrees, a chosen par is honoured. `cd e2e && npm ci && npx playwright test`. |
| `.github/workflows/ci.yml` | 4 jobs | Rust (fmt, clippy -D warnings, tests), Python (pyflakes), Playwright e2e and Lighthouse against production — on push, PR, and a daily schedule, so it doubles as the uptime monitor. `image.yml` publishes the game image to GHCR; `dependabot.yml` groups weekly bumps per ecosystem. |

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
| `/api/puzzle` | 41 ms | Also accepts `from`/`to` (ids) or `from_title`/`to_title`, and `par=` for an exact-par pool draw (pool-only; refuses rather than lie). |
| `/api/daily` | 41 ms | Seeded by the day. |
| `/api/compass` | 1.4 s first per goal | Then cached; charges scale with par (clamp(par-2,1,4)). Static build: compass-lite — `compass/{n}.json` per daily and `compass/g{id}.json` per random-pool goal, complete truncated BFS levels (~250k ids/goal, delta-encoded, own depth `d`), generated by `static_export` (`--compass-days`, default 365 + archive), oracle-tested against `distances_to`. The post-race route reveal is baked instead: every puzzle file carries one shortest route (ban honoured, exact-par by construction) — compass-level walking only reached a third of races at enwiki depth 2-3. |
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

### Phase 2's GPU workarounds (dead code — kept only as a record)

**None of this runs.** `_phase2_gpu` is unreachable in practice because
`force_atlas2` segfaults, and `layout.backend` defaults to `"cpu"`. It is
documented because the workarounds were expensive to find and would be needed
again if cuGraph ever ships a working force layout on its current API.

`_phase2_gpu` existed to fit 28M vertices / 482M edges into 16GB VRAM. Each
step was load-bearing:

1. Edges are **pre-symmetrized on CPU** (both directions + dedup + self-loop
   removal) because cuGraph's internal symmetrization blows past VRAM.
2. The CSR is built **on CPU with scipy**, then handed over via
   `from_cudf_adjlist` — `from_cudf_edgelist` needs huge temporary sort
   buffers.
3. RMM is reinitialized with `managed_memory=True` so overflow pages to
   system RAM over PCIe.
4. The graph is constructed as `directed=True`, then
   `G.graph_properties.directed` is flipped to `False` so FA2/Leiden skip
   their own `to_undirected()`. Note the assert: `is_directed()` reads
   `graph_properties.directed`, not `properties.directed`.
5. `G.nodes()` is **monkey-patched** on the instance to return a precomputed
   range — cuGraph otherwise reconstructs and concats the edge list (~8GB,
   `std::bad_alloc`).

All GPU→CPU transfers go through `.to_arrow().to_pandas()` to bypass Numba
driver bugs on GeForce cards. `layout.backend: "auto"` swallows GPU
*exceptions* and falls through to the CPU path, `"gpu"` re-raises so failures
stay visible — but neither helps against a segfault, which kills the
interpreter outright.

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
| `community_labels.json` | — | LLM region names. Optional; merged per-region with the category-vote fallback, and every source passes the same vacuous-name filter and cross-region uniqueness (`merge_region_names`) — this file once bypassed the "Living people" fix entirely. |
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
| `<dataset-sha256>/leaderboard.jsonl` | Append-only accepted runs for this exact graph. Boards use the `v2:` active-clock version. Legacy root-level logs are preserved but not mixed into these rankings. |
| `.wiki-race-secret` | HMAC key behind the identity cookie. Losing it logs every player out permanently. |
| `analytics.jsonl` | One JSON delta line per hour: per-endpoint counters plus hashed-IP daily uniques. Capacity planning, not accounting — losable. |

These live under `--state`, which **must not** be the data directory — deployments
mount that read-only and the process dies at startup otherwise.

### Game interface and correctness updates (October 2026)

`page.rs` embeds `static/index.html` and its CSS/JavaScript modules into one
self-contained page for both live and static editions. `game.js` handles play;
`data.js` adapts the static export; `race-link.js` encodes puzzle identity;
`map.js` draws the optional explorer; `experience.js` handles dialogs and saved
progress; `reader-duels.js` contains the reader and multiplayer controls.

The race occupies the main screen. Explore opens community cards, the sampled
point map with a region filter, or the current article's outgoing-link list.
The existing SFDP positions are unchanged. Canvas animation stops when the
view settles or closes. Paused links are inert, dialogs manage keyboard focus,
and saved progress is validated before resuming (live restores are practice).

The map fits the displayed sample rather than the full graph's bounds; Fit
also respects the selected region. Community cards use a prominent article
as their heading and explicitly identify their inferred category as an
automatic label. A failed map request can be retried without restarting the
race. The explorer uses content-sized header rows on narrow screens.

Challenge URLs include the dataset, endpoints/rules, or daily/round number.
Static shared pairs must exist in the exported pools; failure is explicit,
never a silent random replacement. Old exports fall back to a legacy identity
based on counts and epoch, which cannot detect every same-size dataset change.
Future dailies are blocked by the client date; static files remain inspectable.

`dataset.rs` hashes titles and edges with streaming SHA-256 at load time.
New pools record that identity; mismatches fail. Legacy pools without a hash
warn and retain their previous count checks. Python pipeline cache fingerprints
are unchanged. The extra sequential file read has not been benchmarked at
full English Wikipedia scale.

`POST /api/clock` starts, pauses, resumes and finishes a player-bound live run.
Finish validates and freezes the route and active duration before score entry;
submit must match. This aligns timing with the UI but is not strong anti-cheat:
clients can still inspect graph data and control pause requests. Static races
are local only. Live compass BFS now excludes banned intermediate hubs and
caches by goal plus ban threshold. Static compass data remains unrestricted,
so distances are labelled lower bounds when hubs are excluded. Distribution
copy explicitly describes the retained best scores (at most 100 per board).

`viewer_index.py` holds the NumPy nearest-point and edge indexes. Selection
checks the active region, searches exactly when the grid cannot prove a nearest
candidate, and preserves the identity of search hits. Degree counts are not
confused with the chosen display direction. The viewer image uses one worker
to avoid duplicating its large edge index; full-scale RSS has not been remeasured.

`tools/preview.py` serves current UI with an existing export or the committed
synthetic `e2e/fixtures` dataset. Default Playwright tests exercise current
sources locally; `BASE_URL` selects production monitoring. CI runs the local
suite on changes and deployed-site checks on its schedule. No full export or
layout recomputation is required for the fixture tests.

`static_export --ui-only --out static_site` validates an existing export, then
refreshes only `index.html`, the share card, manifest/icons, `404.html`, crawler
files and the versioned service worker. It does not load the graph and leaves
`meta.json`, shards, dailies, random pools and compass/map data untouched. The
service-worker cache rotates as on a full export, so returning clients fetch
data again. This command creates local output; deployment remains separate.

Original project code is now MIT-licensed (`LICENSE`, Cargo package metadata
and README agree). Wikipedia-derived content and dependencies retain their
separate licenses.

### Puzzle-pool quality audit (October 2026)

`tools/audit_puzzles.py --site static_site` audits all random-pool rows and
checks a deterministic mixed set of 20 races against their exported article
shards. Outputs are local `reports/puzzle-audit.html` and `.json`. Route checks
cover IDs/titles, actual links, banned intermediate hubs, disambiguation
endpoints and baked path length. They do not independently recompute shortest
paths or optimal-route counts. Pageviews are explicitly a familiarity proxy,
not an exclusion rule; the editorial shortlist is not human playtest evidence.

The existing 800-row pools have 6/3/6 duplicate pairs (easy/medium/hard).
162/174/315 races respectively contain an endpoint below 1,000 exported monthly
views; 95/93/155 endpoint occurrences lack descriptions. The hard pool repeats
Scaritinae 31 times. All 2,400 baked paths pass structural checks; all 20 sampled
routes pass the deeper checks across 90 exported shards. This sample combines
showcase candidates, low-view/repeated endpoints and seeded spot checks, so it
is not statistically representative.

The auditor also decodes each reviewed goal's compass file and counts measured
legal opening choices. In 11 of these 20 races, no legal first choice has a
numeric distance at the exported depth. This does not mean the route is
unreachable; the hint simply cannot distinguish those options at that point.
Hub-banned races use unrestricted static distances, hence lower bounds.

The audit found that `Pools::pick_full` (static export) drew from a whole par
bucket while `pick` (live) applied the intended easy/high-route and hard/low-route
halves. Both now use the same implementation and RNG sequence, with regression
coverage. Live/daily selection keeps its prior algorithm; future random exports
change. Existing static pools and published daily puzzles were not regenerated.
Static practice now uses `puzzle-choice.js`: duplicate pair rows get one entry
in the draw, pairs from the last 60 draws are avoided when possible, and among
those candidates it prefers endpoints least used in the last six practice
races. Pair history is per difficulty, endpoint history spans difficulties,
and both are scoped to the dataset. Exhausted or small pools fall back to the
least repetitive available option. Shared pairs, daily and round selection
bypass this policy. Export-level endpoint caps, unique-pair export sampling
and human playtesting remain follow-up work.

Both static and live compass responses now include `charged`. When every
legal option has an unknown distance (or there are no legal options), the
reading is free. Numeric readings on blocked hubs alone do not count. The
UI says the hint was saved and enables it again at the next article. Live
availability is checked before computation and useful readings are charged
atomically afterwards; concurrent requests cannot exceed the budget. A lost
response after a useful server reading can still cost a charge. The static
adapter rechecks its budget after asynchronous loads. Live requests reject
out-of-range article IDs before looking up adjacency.

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
