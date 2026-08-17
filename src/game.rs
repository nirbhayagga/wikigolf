//! Wiki-race game state: map coordinates, puzzle generation, scoring.
//!
//! Coordinates are **optional**. `nodes.parquet` only exists after the Python
//! pipeline has run — the service has to be useful without it. With
//! coordinates the UI draws the race as a trajectory across the map; without
//! them it degrades to a plain link list rather than refusing to start.

use anyhow::{Context, Result};
use arrow::array::{Array, Float32Array, Float64Array, Int32Array};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::fs::File;
use std::path::Path;

use crate::graph::{Graph, PathFinder};
use crate::pools::{Pools, POOL_FILE};

/// Per-article map position and community, indexed by article id.
pub struct Layout {
    pub x: Vec<f32>,
    pub y: Vec<f32>,
    pub community: Vec<i32>,
}

/// Read a numeric column as f32 whether it was written as f32 or f64 — the
/// CPU and GPU layout paths do not agree on width.
fn f32_column(col: &dyn Array) -> Option<Vec<f32>> {
    if let Some(a) = col.as_any().downcast_ref::<Float32Array>() {
        return Some(a.values().to_vec());
    }
    if let Some(a) = col.as_any().downcast_ref::<Float64Array>() {
        return Some(a.values().iter().map(|v| *v as f32).collect());
    }
    None
}

impl Layout {
    /// `None` (not an error) when the pipeline has not produced nodes.parquet.
    pub fn load(data_dir: &Path, n: usize) -> Result<Option<Layout>> {
        let path = data_dir.join("nodes.parquet");
        if !path.exists() {
            return Ok(None);
        }
        let file = File::open(&path)?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
        let schema = builder.schema().clone();
        let idx = |name: &str| schema.index_of(name).ok();
        let (xi, yi, ci) = (idx("x"), idx("y"), idx("community"));
        let (Some(xi), Some(yi), Some(ci)) = (xi, yi, ci) else {
            anyhow::bail!("nodes.parquet is missing x/y/community columns");
        };

        let mut x = Vec::with_capacity(n);
        let mut y = Vec::with_capacity(n);
        let mut community = Vec::with_capacity(n);
        for batch in builder.build()? {
            let batch = batch?;
            x.extend(
                f32_column(batch.column(xi).as_ref()).context("x column is not floating point")?,
            );
            y.extend(
                f32_column(batch.column(yi).as_ref()).context("y column is not floating point")?,
            );
            let c = batch
                .column(ci)
                .as_any()
                .downcast_ref::<Int32Array>()
                .context("community column is not int32")?;
            community.extend_from_slice(c.values());
        }

        // Phase 3 writes rows in dense id order, so position *is* the article
        // id. If that ever stops being true the map would silently show every
        // article in the wrong place, so check rather than assume.
        if x.len() != n {
            anyhow::bail!(
                "nodes.parquet has {} rows but the graph has {n} articles — \
                 caches are stale, rerun 01_graph_compute.py --reset",
                x.len()
            );
        }
        Ok(Some(Layout { x, y, community }))
    }
}

/// Rank a title against a lowercase query. `None` means no match.
///
/// Match quality and popularity are combined rather than strictly tiered.
/// Strict tiering ranks "Einstein Bros. Bagels" (in-degree 1) above "Albert
/// Einstein" (241) for the query "einst", purely because it happens to start
/// with the letters — which is not what anyone typing that wants.
///
/// An exact title match always wins outright, so "cat" finds `Cat` rather than
/// the far more linked-to `Catholic Church`.
fn search_score(title_lower: &str, q: &str, in_degree: usize) -> Option<u64> {
    const EXACT: u64 = 1 << 60;
    if title_lower == q {
        return Some(EXACT);
    }
    let weight = if title_lower.starts_with(q) {
        100
    } else if title_lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w.starts_with(q))
    {
        30
    } else if title_lower.contains(q) {
        1
    } else {
        return None;
    };
    // +1 so a zero in-degree article still outranks a non-match.
    Some(weight * (in_degree as u64 + 1))
}

/// Precomputed search structures, built once at load.
///
/// The old search lowercased all 7.2M titles on every keystroke — roughly
/// 7.2M short-lived allocations per query, ~100 ms — and could not see
/// aliases at all. The index holds one entry per title AND per redirect
/// alias (~19M entries, ~600 MB at enwiki scale): one contiguous lowercase
/// buffer plus offsets, an owner map, and two orderings over the entries:
///
/// * `alpha` — ids sorted by lowercase title, so exact matches are a binary
///   search instead of a scan.
/// * `by_degree` — ids sorted by in-degree descending, so the scan visits
///   popular articles first and can stop as soon as no remaining article
///   could out-score the current top results (the max non-exact weight is
///   100, so once `100 * (degree + 1)` falls below the worst kept score,
///   nothing later can qualify).
///
/// Typical autocomplete queries terminate within a few thousand titles —
/// well under a millisecond. The worst case (a query matching nothing) is
/// still a full pass, but over the flat buffer with no allocations, several
/// times faster than before; the handler keeps running it on the blocking
/// pool for exactly that case.
pub(crate) struct SearchIndex {
    corpus: String,
    offsets: Vec<u32>,
    /// Entry -> the article it names. Identity for the first n_titles
    /// entries; the redirect target for alias entries. This is what makes
    /// "NYC" find New York City: aliases are first-class entries, and
    /// results collapse to the best-scoring entry per owner.
    owner: Vec<u32>,
    by_degree: Vec<u32>,
    alpha: Vec<u32>,
}

impl SearchIndex {
    pub(crate) fn build<'a, I, F>(
        titles: &[String],
        aliases: F,
        degree_of: &dyn Fn(u32) -> usize,
    ) -> SearchIndex
    where
        F: Fn() -> I,
        I: Iterator<Item = (&'a str, u32)>,
    {
        // Two passes so every buffer is allocated once at its final size.
        // Growing by doubling instead cost ~1.4 GB of resident memory at
        // enwiki scale: the transient copies are freed, but the allocator
        // retains the pages, and the server carries them forever. The 1/64
        // slack absorbs the rare characters whose lowercase form is longer
        // than the original.
        let mut n_entries = titles.len();
        let mut total: usize = titles.iter().map(|t| t.len()).sum();
        for (a, _) in aliases() {
            n_entries += 1;
            total += a.len();
        }
        let mut corpus = String::with_capacity(total + total / 64 + 16);
        let mut offsets = Vec::with_capacity(n_entries + 1);
        let mut owner: Vec<u32> = Vec::with_capacity(n_entries);
        offsets.push(0u32);
        let mut push = |s: &str, o: u32, corpus: &mut String, offsets: &mut Vec<u32>| {
            for c in s.chars() {
                corpus.extend(c.to_lowercase());
            }
            offsets.push(corpus.len() as u32);
            owner.push(o);
        };
        for (i, t) in titles.iter().enumerate() {
            push(t, i as u32, &mut corpus, &mut offsets);
        }
        for (a, target) in aliases() {
            push(a, target, &mut corpus, &mut offsets);
        }
        corpus.shrink_to_fit();
        let n = owner.len() as u32;

        let mut by_degree: Vec<u32> = (0..n).collect();
        by_degree.sort_unstable_by_key(|&e| std::cmp::Reverse(degree_of(owner[e as usize])));

        let slice = |e: u32| -> &str {
            &corpus[offsets[e as usize] as usize..offsets[e as usize + 1] as usize]
        };
        let mut alpha: Vec<u32> = (0..n).collect();
        alpha.sort_unstable_by(|&a, &b| slice(a).cmp(slice(b)));

        SearchIndex { corpus, offsets, owner, by_degree, alpha }
    }

    #[inline]
    fn lower(&self, e: u32) -> &str {
        &self.corpus[self.offsets[e as usize] as usize..self.offsets[e as usize + 1] as usize]
    }
}

/// The ranked search behind `Game::search`, factored out so tests can drive
/// it with synthetic titles. Result ordering is identical to the old full
/// linear scan: score descending, ties broken by id ascending.
fn search_ranked(
    idx: &SearchIndex,
    degree_of: &dyn Fn(u32) -> usize,
    query: &str,
    limit: usize,
) -> Vec<u32> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let cap = limit.max(1);
    // (score, owner article). Several entries can name one article — its
    // title and any number of aliases — so ordering and truncation always
    // happen through `rank`, which dedups to the best entry per owner.
    let mut hits: Vec<(u64, u32)> = Vec::new();
    let rank = |hits: &mut Vec<(u64, u32)>| {
        hits.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut seen = std::collections::HashSet::new();
        hits.retain(|&(_, owner)| seen.insert(owner));
    };

    // Exact matches by binary search. Distinct entries can share a lowercase
    // form ("Cat" / "CAT" / an alias "cat"), so take the whole equal range.
    let lo = idx.alpha.partition_point(|&e| idx.lower(e) < q.as_str());
    for &e in &idx.alpha[lo..] {
        if idx.lower(e) != q {
            break;
        }
        let owner = idx.owner[e as usize];
        if let Some(score) = search_score(idx.lower(e), &q, degree_of(owner)) {
            hits.push((score, owner));
        }
    }

    // Popularity-ordered scan with early exit. `worst` is the cap-th best
    // deduplicated score once known; strict `<` in the cut-off keeps the
    // tie ordering identical to a full scan.
    let mut worst: Option<u64> = None;
    for &e in &idx.by_degree {
        let owner = idx.owner[e as usize];
        let deg = degree_of(owner) as u64;
        if let Some(w) = worst {
            if 100 * (deg + 1) < w {
                break;
            }
        }
        let t = idx.lower(e);
        if t == q {
            continue; // already counted via the exact range
        }
        if let Some(score) = search_score(t, &q, deg as usize) {
            hits.push((score, owner));
            if hits.len() >= cap * 4 {
                rank(&mut hits);
                if hits.len() >= cap {
                    hits.truncate(cap);
                    worst = Some(hits[cap - 1].0);
                }
            }
        }
    }

    rank(&mut hits);
    hits.truncate(limit);
    hits.into_iter().map(|(_, owner)| owner).collect()
}

/// How many of the most linked-to articles are eligible as race endpoints.
/// Capped at a quarter of the wiki so small wikis are not reduced to a
/// handful of candidates.
const PLAYABLE_POOL: usize = 50_000;

/// Endpoints must be articles a player has a chance of recognising.
///
/// A fixed in-degree floor does not survive a change of wiki: 25 inbound
/// links is respectable on Simple English (284k articles) but admits a vast
/// obscure tail on full English (7.2M), which produced races like
/// "Legong -> Bara Jumla". Rank instead, and keep the head.
///
/// Free-standing (not a method) so the pools generator can rank endpoints
/// from a bare `Graph` — it must not go through `Game::load`, which refuses
/// to start over a stale pools file, the very thing the generator exists to
/// replace.
pub fn playable_pool(graph: &Graph) -> Vec<u32> {
    let mut ranked: Vec<u32> = (0..graph.len() as u32)
        .filter(|&v| graph.forward.degree(v) >= 10)
        .collect();
    let keep = PLAYABLE_POOL.min(ranked.len().div_ceil(4));
    if keep > 0 && keep < ranked.len() {
        ranked.select_nth_unstable_by_key(keep - 1, |&v| {
            std::cmp::Reverse(graph.reverse.degree(v))
        });
        ranked.truncate(keep);
    }
    ranked
}

pub struct Game {
    pub graph: Graph,
    pub layout: Option<Layout>,
    /// Articles worth using as puzzle endpoints, by id.
    playable: Vec<u32>,
    /// The most linked-to articles, in descending in-degree order. Precomputed
    /// so the hub slider can name what it excludes without re-ranking 7.2M
    /// articles on every drag.
    hubs: Vec<u32>,
    /// The top MAP_POINTS articles by in-degree, ranked once at load. The map
    /// is identical for every visitor and changes only with the dump, so it is
    /// computed here rather than rebuilt on each request.
    map_order: Vec<u32>,
    /// Community id -> human name, from community_labels.json when the LLM
    /// naming step has run. Empty otherwise, and the UI falls back to
    /// "Region N" — a missing label is cosmetic, never fatal.
    pub region_names: std::collections::HashMap<i32, String>,
    /// What each article is *about*, from the wikitext's own [[Category:...]]
    /// links. Empty when the parse predates category extraction.
    pub categories: PerArticle,
    /// Redirect titles pointing at each article — "also known as".
    pub aliases: PerArticle,
    /// Wikitext bytes per article; empty when the parse predates it.
    pub sizes: Vec<u32>,
    /// Monthly pageviews per article; empty until 09_pageviews.py has run.
    /// Readership is the fame signal players recognise — in-degree is
    /// editor behaviour.
    pub views: Vec<u32>,
    /// Lowercased-title search structures, built once at load.
    search: SearchIndex,
    /// Precomputed puzzle pairs with route counts, when pools.parquet exists.
    /// None degrades to rejection sampling, exactly as before.
    pools: Option<Pools>,
}

/// How deep the hub ranking goes; the slider cannot exclude more than this.
pub const MAX_HUBS: usize = 50_000;

/// The most points the map endpoint will ever return, and so the size of the
/// precomputed ranking behind it. Every extra point is ~10 bytes on the wire
/// and one more thing to draw each frame; past this the map is a solid mass
/// and the cost buys nothing.
pub const MAP_POINTS: usize = 120_000;

/// A generated race.
pub struct Puzzle {
    pub start: u32,
    pub goal: u32,
    pub ban_degree: Option<usize>,
    pub optimal: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Difficulty {
    Easy,
    Medium,
    Hard,
}

impl Difficulty {
    pub fn parse(s: &str) -> Difficulty {
        match s {
            "hard" => Difficulty::Hard,
            "medium" => Difficulty::Medium,
            _ => Difficulty::Easy,
        }
    }

    /// The hub ban and the minimum optimal length that makes a race worth
    /// playing. Unrestricted Wikipedia paths are almost always 2 clicks, so
    /// difficulty comes from banning hubs, not from picking obscure articles.
    /// Public because the pools generator must bucket with the same rules the
    /// server will draw with.
    pub fn rules(self) -> (Option<usize>, usize) {
        match self {
            Difficulty::Easy => (None, 3),
            Difficulty::Medium => (Some(5_000), 3),
            Difficulty::Hard => (Some(1_000), 4),
        }
    }
}

/// xorshift64*, so puzzle generation needs no rand dependency.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

impl Game {
    pub fn load(data_dir: &Path) -> Result<Game> {
        Self::load_with(data_dir, true)
    }

    /// `alias_search: false` leaves the 12M redirect aliases out of the
    /// search index, saving ~450 MB of resident memory. For small boxes:
    /// search still works over every title, "NYC" just stops finding New
    /// York City.
    pub fn load_with(data_dir: &Path, alias_search: bool) -> Result<Game> {
        let graph = Graph::load(data_dir)?;
        let n_articles = graph.len();
        let layout = Layout::load(data_dir, graph.len())?;

        let playable = playable_pool(&graph);

        let mut hubs: Vec<u32> = (0..graph.len() as u32).collect();
        let k = MAX_HUBS.min(hubs.len());
        if k < hubs.len() {
            hubs.select_nth_unstable_by_key(k - 1, |&v| {
                std::cmp::Reverse(graph.reverse.degree(v))
            });
            hubs.truncate(k);
        }
        hubs.sort_unstable_by_key(|&v| std::cmp::Reverse(graph.reverse.degree(v)));

        // The map's point order, ranked once here rather than per request.
        //
        // map_sample used to build a 7.2M-element Vec and partially sort it on
        // every call — roughly 29 MB of allocation and a pass over every
        // article for each page load, when the answer is identical for every
        // visitor and never changes until the next dump. MAP_POINTS caps what
        // the endpoint can ever be asked for, so this slice covers all of it.
        let mut map_order: Vec<u32> = (0..graph.len() as u32).collect();
        let m = MAP_POINTS.min(map_order.len());
        if m < map_order.len() {
            map_order.select_nth_unstable_by_key(m - 1, |&v| {
                std::cmp::Reverse(graph.reverse.degree(v))
            });
            map_order.truncate(m);
        }
        map_order.shrink_to_fit();

        // Optional: only exists if 03_name_clusters.py has been run.
        let llm_names: std::collections::HashMap<i32, String> =
            std::fs::read_to_string(data_dir.join("community_labels.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<std::collections::HashMap<String, String>>(&t).ok())
            .map(|m| {
                m.into_iter()
                    .filter_map(|(k, v)| k.parse::<i32>().ok().map(|k| (k, v)))
                    .collect()
            })
            .unwrap_or_default();

        // All three are optional: an older parse simply has none of them, and
        // every consumer degrades to showing nothing rather than failing.
        let categories = PerArticle::load(&data_dir.join("categories.parquet"), n_articles, 8)?;
        let aliases =
            PerArticle::load(&data_dir.join("redirects.parquet"), n_articles, MAX_ALIASES)?;
        let sizes = read_u32_column(&data_dir.join("article_sizes.parquet"), n_articles, "article_sizes")?;
        // What people actually read, not what editors link — from
        // 09_pageviews.py, and absent until it has run.
        let views = read_u32_column(&data_dir.join("pageviews.parquet"), n_articles, "pageviews")?;

        // Region names, preferring what editors wrote over what a model
        // guessed. A region is an emergent cluster with no category of its
        // own, but its members carry categories, and the most common one
        // among them is a fair name — free, deterministic, and grounded in
        // the encyclopedia rather than in an API call.
        let region_names = if !llm_names.is_empty() {
            llm_names
        } else {
            derive_region_names(&layout, &categories, n_articles)
        };

        let search = if alias_search {
            SearchIndex::build(&graph.titles, || graph.alias_entries(), &|v| {
                graph.reverse.degree(v)
            })
        } else {
            SearchIndex::build(&graph.titles, std::iter::empty, &|v| graph.reverse.degree(v))
        };

        // Optional but never silently wrong: a missing pools file degrades to
        // rejection sampling, a stale one refuses to load (see pools.rs).
        let pools = Pools::load(
            &data_dir.join(POOL_FILE),
            n_articles,
            graph.forward.edges(),
        )?;
        if let Some(p) = &pools {
            eprintln!("  puzzle pools: {}", p.summary());
        }

        Ok(Game {
            graph,
            layout,
            playable,
            hubs,
            map_order,
            region_names,
            categories,
            aliases,
            sizes,
            views,
            search,
            pools,
        })
    }

    /// Race endpoints worth generating puzzles from, most-linked first once
    /// sorted by the caller. Public for the pools generator.
    pub fn playable(&self) -> &[u32] {
        &self.playable
    }

    pub fn has_pools(&self) -> bool {
        self.pools.is_some()
    }

    /// The in-degree limit that excludes roughly the top `n` articles, with a
    /// sample of what that excludes.
    ///
    /// "Roughly" is honest: articles tie on in-degree, and the ban is a
    /// threshold rather than a list, so a tie straddling the cut takes its
    /// whole group with it.
    pub fn hub_cut(&self, n: usize, sample: usize) -> (Option<usize>, Vec<u32>, usize) {
        if n == 0 {
            return (None, Vec::new(), 0);
        }
        let n = n.min(self.hubs.len());
        // Ban anything strictly above the degree of the first article we keep.
        let limit = if n < self.hubs.len() {
            self.graph.reverse.degree(self.hubs[n])
        } else {
            0
        };
        let excluded = self
            .hubs
            .iter()
            .take_while(|&&v| self.graph.reverse.degree(v) > limit)
            .count();
        let names = self.hubs.iter().take(sample).copied().collect();
        (Some(limit), names, excluded)
    }

    pub fn coords(&self, id: u32) -> Option<(f32, f32, i32)> {
        let l = self.layout.as_ref()?;
        Some((l.x[id as usize], l.y[id as usize], l.community[id as usize]))
    }

    /// Generate a race meeting the difficulty's rules, or `None` if no
    /// candidate pair qualified within the attempt budget.
    ///
    /// With pools present, draws come from the precomputed buckets — that is
    /// what makes par 5+ races and the route-count difficulty split possible
    /// at all. Every drawn pair is still verified with a live search before
    /// being served: the pool stores candidates, the graph stays the truth,
    /// and a failed verification falls through to rejection sampling rather
    /// than serving a wrong par. The custom hub-slider path never uses pools
    /// (its ban level isn't one they were computed for).
    pub fn puzzle(&self, pf: &mut PathFinder, d: Difficulty, rng: &mut Rng) -> Option<Puzzle> {
        let (ban, min_len) = d.rules();
        if let Some(pools) = &self.pools {
            let rev = &self.graph.reverse;
            let banned: Box<dyn Fn(u32) -> bool + '_> = match ban {
                Some(limit) => Box::new(move |v: u32| rev.degree(v) > limit),
                None => Box::new(|_| false),
            };
            for _ in 0..8 {
                let Some((a, b)) = pools.pick(d, rng) else { break };
                let Some(path) = pf.shortest_path(&self.graph, a, b, &banned) else {
                    continue;
                };
                let optimal = path.len() - 1;
                if optimal >= min_len && optimal <= min_len + 3 {
                    return Some(Puzzle { start: a, goal: b, ban_degree: ban, optimal });
                }
            }
        }
        self.puzzle_with(pf, ban, min_len, rng)
    }

    /// Generate against an explicit hub cut, as the slider supplies.
    pub fn puzzle_with(
        &self,
        pf: &mut PathFinder,
        ban_degree: Option<usize>,
        min_len: usize,
        rng: &mut Rng,
    ) -> Option<Puzzle> {
        if self.playable.len() < 2 {
            return None;
        }
        let rev = &self.graph.reverse;
        let banned: Box<dyn Fn(u32) -> bool + '_> = match ban_degree {
            Some(limit) => Box::new(move |v: u32| rev.degree(v) > limit),
            None => Box::new(|_| false),
        };

        for _ in 0..60 {
            let a = self.playable[rng.below(self.playable.len())];
            let b = self.playable[rng.below(self.playable.len())];
            if a == b {
                continue;
            }
            let Some(path) = pf.shortest_path(&self.graph, a, b, &banned) else {
                continue;
            };
            let optimal = path.len() - 1;
            // Cap the upper end too: a 9-hop race is not hard, it is tedious.
            if optimal >= min_len && optimal <= min_len + 3 {
                return Some(Puzzle { start: a, goal: b, ban_degree, optimal });
            }
        }
        None
    }

    /// A race where both endpoints come from one map region (community).
    ///
    /// Regions are topical — that is the whole point of Leiden — so this is
    /// "get from one physicist to another" rather than across the map.
    /// Rejection sampling is fine here: the pool cannot help (it has no
    /// region column) and in-region pairs are dense enough that the accept
    /// rate stays high. The difficulty's hub ban still applies; its minimum
    /// length is relaxed by one (floor 2) because a tight community often has
    /// no pair further apart than that, and a 2-click race inside a topic is
    /// still a real race in a way a 2-click race across all of Wikipedia is
    /// not.
    pub fn puzzle_in_region(
        &self,
        pf: &mut PathFinder,
        region: i32,
        d: Difficulty,
        rng: &mut Rng,
    ) -> Option<Puzzle> {
        let layout = self.layout.as_ref()?;
        let (ban, min_len) = d.rules();
        let min_len = min_len.saturating_sub(1).max(2);
        let members: Vec<u32> = self
            .playable
            .iter()
            .copied()
            .filter(|&v| layout.community[v as usize] == region)
            .filter(|&v| ban.is_none_or(|limit| self.graph.reverse.degree(v) <= limit))
            .collect();
        if members.len() < 2 {
            return None;
        }
        let rev = &self.graph.reverse;
        let banned: Box<dyn Fn(u32) -> bool + '_> = match ban {
            Some(limit) => Box::new(move |v: u32| rev.degree(v) > limit),
            None => Box::new(|_| false),
        };
        for _ in 0..60 {
            let a = members[rng.below(members.len())];
            let b = members[rng.below(members.len())];
            if a == b {
                continue;
            }
            let Some(path) = pf.shortest_path(&self.graph, a, b, &banned) else {
                continue;
            };
            let optimal = path.len() - 1;
            if optimal >= min_len && optimal <= min_len + 4 {
                return Some(Puzzle { start: a, goal: b, ban_degree: ban, optimal });
            }
        }
        None
    }

    /// A race between two articles the player chose, rather than a generated
    /// pair.
    ///
    /// Endpoints are exempt from the hub ban. Picking "United States" as your
    /// goal is a legitimate race; refusing it because the ban level happens to
    /// exclude it would look like a bug, and banning the start would make the
    /// race unstartable. Everything in between is still banned, so par stays
    /// consistent with what the player is allowed to click.
    ///
    /// Returns None when the pair is invalid or no route exists under the ban.
    pub fn puzzle_between(
        &self,
        pf: &mut PathFinder,
        a: u32,
        b: u32,
        ban_degree: Option<usize>,
    ) -> Option<Puzzle> {
        let n = self.graph.len() as u32;
        if a == b || a >= n || b >= n {
            return None;
        }
        let rev = &self.graph.reverse;
        let banned: Box<dyn Fn(u32) -> bool + '_> = match ban_degree {
            Some(limit) => Box::new(move |v: u32| v != a && v != b && rev.degree(v) > limit),
            None => Box::new(|_| false),
        };
        let path = pf.shortest_path(&self.graph, a, b, &banned)?;
        Some(Puzzle { start: a, goal: b, ban_degree, optimal: path.len() - 1 })
    }

    /// Search over every article title and redirect alias, ranked by match
    /// quality x popularity — "NYC" finds New York City.
    ///
    /// Sub-millisecond for typical autocomplete queries via the prebuilt
    /// index (see `SearchIndex`); the no-match worst case is still a linear
    /// pass, hence the caller keeps running it off the async runtime.
    pub fn search(&self, query: &str, limit: usize) -> Vec<u32> {
        search_ranked(&self.search, &|v| self.graph.reverse.degree(v), query, limit)
    }

    /// One label per community: its most linked-to article, placed at that
    /// article's position.
    ///
    /// This is what turns a cloud of dots into something readable as a map.
    /// The most-linked article in a region is a fair name for it — the
    /// "Association football" cluster really is centred on that article — and
    /// unlike the LLM community labels it needs no API key and no extra file.
    pub fn landmarks(&self, limit: usize) -> Vec<(String, f32, f32, i32)> {
        let Some(l) = self.layout.as_ref() else {
            return Vec::new();
        };
        let ncom = l.community.iter().copied().max().unwrap_or(0).max(0) as usize + 1;
        let mut best: Vec<(usize, u32)> = vec![(0, u32::MAX); ncom];
        let mut size: Vec<u32> = vec![0; ncom];
        for v in 0..self.graph.len() as u32 {
            let c = l.community[v as usize];
            if c < 0 {
                continue;
            }
            let c = c as usize;
            size[c] += 1;
            let d = self.graph.reverse.degree(v);
            if best[c].1 == u32::MAX || d > best[c].0 {
                best[c] = (d, v);
            }
        }
        let mut order: Vec<usize> = (0..ncom).filter(|&c| best[c].1 != u32::MAX).collect();
        order.sort_unstable_by_key(|&c| std::cmp::Reverse(size[c]));
        order.truncate(limit);
        order
            .into_iter()
            .map(|c| {
                let v = best[c].1;
                (
                    self.graph.title(v).to_string(),
                    l.x[v as usize],
                    l.y[v as usize],
                    c as i32,
                )
            })
            .collect()
    }

    /// Background points for the map, as the highest in-degree articles.
    ///
    /// The full node set is far too large to ship to a browser (7.2M points),
    /// and the most-linked articles are both the cheapest to send and the ones
    /// that give the map its recognisable shape.
    pub fn map_sample(&self, limit: usize) -> Vec<(f32, f32, i32)> {
        let Some(l) = self.layout.as_ref() else {
            return Vec::new();
        };
        let k = limit.min(self.map_order.len());
        self.map_order[..k]
            .iter()
            .map(|&v| (l.x[v as usize], l.y[v as usize], l.community[v as usize]))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn difficulty_parses_and_bans_hubs() {
        assert_eq!(Difficulty::parse("hard"), Difficulty::Hard);
        assert_eq!(Difficulty::parse("medium"), Difficulty::Medium);
        assert_eq!(Difficulty::parse("nonsense"), Difficulty::Easy);
        // Easy must not ban anything; hard must ban and demand a longer route.
        assert_eq!(Difficulty::Easy.rules().0, None);
        assert!(Difficulty::Hard.rules().0.unwrap() < Difficulty::Medium.rules().0.unwrap());
        assert!(Difficulty::Hard.rules().1 > Difficulty::Easy.rules().1);
    }

    #[test]
    fn rng_is_deterministic_and_in_range() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let mut r = Rng::new(7);
        for _ in 0..1000 {
            assert!(r.below(10) < 10);
        }
    }

    #[test]
    fn exact_title_beats_a_far_more_popular_prefix() {
        // "cat" must find Cat, not Catholic Church, however lopsided the
        // in-degrees are.
        let cat = search_score("cat", "cat", 2_817).unwrap();
        let catholic = search_score("catholic church", "cat", 93_701).unwrap();
        assert!(cat > catholic);
    }

    #[test]
    fn popular_word_prefix_beats_obscure_title_prefix() {
        // The bug this scoring exists to fix.
        let albert = search_score("albert einstein", "einst", 241).unwrap();
        let bagels = search_score("einstein bros. bagels", "einst", 1).unwrap();
        assert!(albert > bagels, "Albert Einstein must outrank a bagel shop");
    }

    #[test]
    fn match_quality_still_dominates_within_reason() {
        // A plain substring match should not leapfrog a title prefix on
        // popularity alone at comparable magnitudes.
        let prefix = search_score("france", "franc", 141_719).unwrap();
        let substr = search_score("the weinstein company", "einst", 48).unwrap();
        assert!(prefix > substr);
        assert_eq!(search_score("unrelated", "einst", 999), None);
    }

    #[test]
    fn word_prefix_requires_a_word_boundary() {
        // "einst" is inside "weinstein" but does not start a word there.
        assert_eq!(search_score("the weinstein company", "einst", 5), Some(5 + 1));
        assert_eq!(search_score("albert einstein", "einst", 5), Some(30 * (5 + 1)));
    }

    #[test]
    fn rng_seed_zero_still_advances() {
        // seed | 1 guards the xorshift fixed point at zero.
        let mut r = Rng::new(0);
        assert_ne!(r.next_u64(), 0);
    }

    /// The reference implementation the index replaced: lowercase every
    /// title, score every title, full sort. The index must return exactly
    /// this, in exactly this order.
    fn search_reference(
        titles: &[String],
        degrees: &[usize],
        query: &str,
        limit: usize,
    ) -> Vec<u32> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(u64, u32)> = Vec::new();
        for (id, t) in titles.iter().enumerate() {
            if let Some(score) = search_score(&t.to_lowercase(), &q, degrees[id]) {
                hits.push((score, id as u32));
            }
        }
        hits.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        hits.into_iter().take(limit).map(|(_, id)| id).collect()
    }

    #[test]
    fn search_index_matches_the_linear_scan() {
        let titles: Vec<String> = [
            "Cat",
            "Catholic Church",
            "Albert Einstein",
            "Einstein Bros. Bagels",
            "The Weinstein Company",
            "France",
            "CAT", // lowercase collision with "Cat"
            "Catalonia",
            "Category theory",
            "Einstein",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let degrees = vec![2_817usize, 93_701, 241, 1, 48, 141_719, 5, 900, 60, 300];

        let idx = SearchIndex::build(&titles, std::iter::empty, &|v| degrees[v as usize]);
        for q in ["cat", "einst", "einstein", "franc", "e", "zzz-no-match", "  ", "CAT"] {
            for limit in [1, 3, 10] {
                assert_eq!(
                    search_ranked(&idx, &|v| degrees[v as usize], q, limit),
                    search_reference(&titles, &degrees, q, limit),
                    "query {q:?} limit {limit}"
                );
            }
        }
    }

    #[test]
    fn search_early_exit_survives_many_matches() {
        // 500 titles all sharing a prefix, so compaction and the worst-score
        // cut-off both trigger; the winners must still be the most linked.
        let titles: Vec<String> = (0..500).map(|i| format!("Topic {i}")).collect();
        let degrees: Vec<usize> = (0..500).map(|i| (i * 7) % 499).collect();
        let idx = SearchIndex::build(&titles, std::iter::empty, &|v| degrees[v as usize]);
        assert_eq!(
            search_ranked(&idx, &|v| degrees[v as usize], "topic", 5),
            search_reference(&titles, &degrees, "topic", 5),
        );
    }

    #[test]
    fn aliases_find_their_article_and_dedup() {
        let titles: Vec<String> =
            ["New York City", "Nyctalus", "Albert Einstein"].iter().map(|s| s.to_string()).collect();
        let degrees = [50_000usize, 3, 241];
        let aliases: Vec<(&str, u32)> =
            vec![("NYC", 0), ("The Big Apple", 0), ("Einstein", 2)];
        let idx = SearchIndex::build(
            &titles,
            || aliases.iter().copied(),
            &|v| degrees[v as usize],
        );
        let search = |q: &str, n: usize| search_ranked(&idx, &|v| degrees[v as usize], q, n);

        // An exact alias match outranks a title that merely starts with it.
        assert_eq!(search("nyc", 2), vec![0, 1]);
        // A substring hit through an alias still resolves to the article.
        assert_eq!(search("big apple", 3), vec![0]);
        // Title and alias of the same article both match "einstein":
        // exactly one result for it, not two.
        assert_eq!(search("einstein", 5), vec![2]);
        // Nothing invents matches.
        assert!(search("zzz", 5).is_empty());
    }
}

/// Per-article string lists packed as CSR: article -> its categories, or
/// article -> the redirect titles that point at it.
///
/// Stored as offsets into one shared `Vec<String>` rather than a
/// `Vec<Vec<String>>`: at enwiki scale that is millions of separate
/// allocations saved, and the values are read far more often than written
/// (which is never).
#[derive(Default)]
pub struct PerArticle {
    offsets: Vec<u32>,
    values: Vec<String>,
}

impl PerArticle {
    pub fn get(&self, id: u32) -> &[String] {
        let i = id as usize;
        if i + 1 >= self.offsets.len() {
            return &[];
        }
        &self.values[self.offsets[i] as usize..self.offsets[i + 1] as usize]
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Read `(u32 id, utf8 value)` rows into CSR, keeping at most `cap` per
    /// article.
    ///
    /// Missing file is not an error: categories only exist after a parse that
    /// produced them, and every caller degrades to showing nothing.
    fn load(path: &Path, n: usize, cap: usize) -> Result<PerArticle> {
        if !path.exists() {
            return Ok(PerArticle::default());
        }
        let file = File::open(path)?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;

        // Two passes over the batches would mean re-reading the file, so
        // collect then bucket. Rows arrive in article order in practice, but
        // nothing here depends on that.
        let mut rows: Vec<(u32, String)> = Vec::new();
        for batch in reader {
            let batch = batch?;
            let ids = batch
                .column(0)
                .as_any()
                .downcast_ref::<arrow::array::UInt32Array>()
                .context("column 0 is not uint32")?;
            let vals = batch
                .column(1)
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .context("column 1 is not utf8")?;
            for i in 0..batch.num_rows() {
                let id = ids.value(i);
                if (id as usize) < n {
                    rows.push((id, vals.value(i).to_string()));
                }
            }
        }

        let mut counts = vec![0u32; n + 1];
        for (id, _) in &rows {
            let c = &mut counts[*id as usize];
            if (*c as usize) < cap {
                *c += 1;
            }
        }
        let mut offsets = vec![0u32; n + 1];
        let mut running = 0u32;
        for i in 0..n {
            offsets[i] = running;
            running += counts[i];
        }
        offsets[n] = running;

        let mut values = vec![String::new(); running as usize];
        let mut fill = offsets.clone();
        for (id, v) in rows {
            let i = id as usize;
            if fill[i] < offsets[i + 1] {
                values[fill[i] as usize] = v;
                fill[i] += 1;
            }
        }
        Ok(PerArticle { offsets, values })
    }
}

/// Alternative titles shown per article. Wikipedia has redirects for every
/// misspelling and abbreviation; a handful is context, the full list is noise.
const MAX_ALIASES: usize = 5;

/// Wikitext byte length per article, indexed by id. Empty when absent.
/// Read an (id: u32, value: u32) parquet into a dense per-article vector.
/// Serves both article_sizes.parquet and pageviews.parquet — same shape,
/// same "missing file is just empty" semantics.
fn read_u32_column(path: &Path, n: usize, what: &str) -> Result<Vec<u32>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(path)?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    let mut out = vec![0u32; n];
    for batch in reader {
        let batch = batch?;
        let ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::UInt32Array>()
            .with_context(|| format!("{what} column 0 is not uint32"))?;
        let bytes = batch
            .column(1)
            .as_any()
            .downcast_ref::<arrow::array::UInt32Array>()
            .with_context(|| format!("{what} column 1 is not uint32"))?;
        for i in 0..batch.num_rows() {
            let id = ids.value(i) as usize;
            if id < n {
                out[id] = bytes.value(i);
            }
        }
    }
    Ok(out)
}

/// Name each region by the category most common among its members.
///
/// Replaces the LLM naming step for most purposes: an emergent Leiden cluster
/// has no category of its own, but if 40,000 of its members are filed under
/// "American film actors" then that is what the region is, and no model was
/// needed to find out.
///
/// Only the head of each region is sampled. A region can hold hundreds of
/// thousands of articles and the modal category converges long before that;
/// counting all of them would cost a full pass for a label.
fn derive_region_names(
    layout: &Option<Layout>,
    categories: &PerArticle,
    n: usize,
) -> std::collections::HashMap<i32, String> {
    use std::collections::HashMap;
    let Some(l) = layout.as_ref() else {
        return HashMap::new();
    };
    if categories.is_empty() {
        return HashMap::new();
    }

    const SAMPLE_PER_REGION: usize = 20_000;
    let mut seen: HashMap<i32, usize> = HashMap::new();
    let mut tally: HashMap<i32, HashMap<&str, u32>> = HashMap::new();

    for id in 0..n {
        let c = l.community[id];
        let count = seen.entry(c).or_default();
        if *count >= SAMPLE_PER_REGION {
            continue;
        }
        *count += 1;
        let e = tally.entry(c).or_default();
        for name in categories.get(id as u32) {
            *e.entry(name.as_str()).or_default() += 1;
        }
    }

    tally
        .into_iter()
        .filter_map(|(c, counts)| {
            counts
                .into_iter()
                // A category shared by two articles is a coincidence, not a
                // region's identity.
                .filter(|&(_, n)| n >= 3)
                .max_by_key(|&(_, n)| n)
                .map(|(name, _)| (c, name.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod region_name_tests {
    use super::*;

    fn per_article(rows: &[(u32, &str)], n: usize, cap: usize) -> PerArticle {
        let mut counts = vec![0u32; n + 1];
        for (id, _) in rows {
            let c = &mut counts[*id as usize];
            if (*c as usize) < cap {
                *c += 1;
            }
        }
        let mut offsets = vec![0u32; n + 1];
        let mut running = 0;
        for i in 0..n {
            offsets[i] = running;
            running += counts[i];
        }
        offsets[n] = running;
        let mut values = vec![String::new(); running as usize];
        let mut fill = offsets.clone();
        for (id, v) in rows {
            let i = *id as usize;
            if fill[i] < offsets[i + 1] {
                values[fill[i] as usize] = v.to_string();
                fill[i] += 1;
            }
        }
        PerArticle { offsets, values }
    }

    fn flat_layout(communities: Vec<i32>) -> Option<Layout> {
        let n = communities.len();
        Some(Layout { x: vec![0.0; n], y: vec![0.0; n], community: communities })
    }

    #[test]
    fn a_region_is_named_by_its_commonest_category() {
        // Region 0 is four physicists, region 1 is four footballers.
        let layout = flat_layout(vec![0, 0, 0, 0, 1, 1, 1, 1]);
        let cats = per_article(
            &[
                (0, "Physicists"),
                (1, "Physicists"),
                (2, "Physicists"),
                (3, "Physicists"),
                (3, "Nobel laureates"),
                (4, "Footballers"),
                (5, "Footballers"),
                (6, "Footballers"),
                (7, "Footballers"),
                (7, "Cyclists"),
            ],
            8,
            8,
        );
        let names = derive_region_names(&layout, &cats, 8);
        assert_eq!(names.get(&0).map(String::as_str), Some("Physicists"));
        assert_eq!(names.get(&1).map(String::as_str), Some("Footballers"));
    }

    #[test]
    fn a_category_shared_by_two_articles_is_not_a_region_name() {
        // Below the floor. Without it, a coincidence between a couple of
        // articles would become the name of a whole region.
        let layout = flat_layout(vec![0, 0, 0, 0]);
        let cats = per_article(&[(0, "Rare"), (1, "Rare")], 4, 8);
        assert!(derive_region_names(&layout, &cats, 4).is_empty());
    }

    #[test]
    fn no_layout_or_no_categories_yields_no_names() {
        let cats = per_article(&[(0, "X"), (1, "X"), (2, "X")], 4, 8);
        assert!(derive_region_names(&None, &cats, 4).is_empty());
        let layout = flat_layout(vec![0; 4]);
        assert!(derive_region_names(&layout, &PerArticle::default(), 4).is_empty());
    }
}
