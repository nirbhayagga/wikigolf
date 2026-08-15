//! Wiki-race game state: map coordinates, puzzle generation, scoring.
//!
//! Coordinates are **optional**. `nodes.parquet` only exists after the Python
//! pipeline has run, and the GPU layout has not been run at full enwiki scale
//! yet — so the service has to be useful without it. With coordinates the UI
//! draws the race as a trajectory across the map; without them it degrades to
//! a plain link list rather than refusing to start.

use anyhow::{Context, Result};
use arrow::array::{Array, Float32Array, Float64Array, Int32Array};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::fs::File;
use std::path::Path;

use crate::graph::{Graph, PathFinder};

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

/// How many of the most linked-to articles are eligible as race endpoints.
/// Capped at a quarter of the wiki so small wikis are not reduced to a
/// handful of candidates.
const PLAYABLE_POOL: usize = 50_000;

pub struct Game {
    pub graph: Graph,
    pub layout: Option<Layout>,
    /// Articles worth using as puzzle endpoints, by id.
    playable: Vec<u32>,
    /// The most linked-to articles, in descending in-degree order. Precomputed
    /// so the hub slider can name what it excludes without re-ranking 7.2M
    /// articles on every drag.
    hubs: Vec<u32>,
}

/// How deep the hub ranking goes; the slider cannot exclude more than this.
pub const MAX_HUBS: usize = 50_000;

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
    fn rules(self) -> (Option<usize>, usize) {
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
        let graph = Graph::load(data_dir)?;
        let layout = Layout::load(data_dir, graph.len())?;

        // Endpoints must be articles a player has a chance of recognising.
        //
        // A fixed in-degree floor does not survive a change of wiki: 25 inbound
        // links is respectable on Simple English (284k articles) but admits a
        // vast obscure tail on full English (7.2M), which produced races like
        // "Legong -> Bara Jumla". Rank instead, and keep the head.
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
        let playable = ranked;

        let mut hubs: Vec<u32> = (0..graph.len() as u32).collect();
        let k = MAX_HUBS.min(hubs.len());
        if k < hubs.len() {
            hubs.select_nth_unstable_by_key(k - 1, |&v| {
                std::cmp::Reverse(graph.reverse.degree(v))
            });
            hubs.truncate(k);
        }
        hubs.sort_unstable_by_key(|&v| std::cmp::Reverse(graph.reverse.degree(v)));

        Ok(Game { graph, layout, playable, hubs })
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
    pub fn puzzle(&self, pf: &mut PathFinder, d: Difficulty, rng: &mut Rng) -> Option<Puzzle> {
        let (ban, min_len) = d.rules();
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

    /// Title search over every article, ranked by match quality x popularity.
    ///
    /// A linear scan of 7.2M titles (~100 ms), hence the caller running it off
    /// the async runtime.
    pub fn search(&self, query: &str, limit: usize) -> Vec<u32> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(u64, u32)> = Vec::new();
        for (id, t) in self.graph.titles.iter().enumerate() {
            let lower = t.to_lowercase();
            if let Some(score) = search_score(&lower, &q, self.graph.reverse.degree(id as u32)) {
                hits.push((score, id as u32));
            }
        }
        // Highest score first; ties broken by id so results are stable.
        hits.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        hits.into_iter().take(limit).map(|(_, id)| id).collect()
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
        let mut ids: Vec<u32> = (0..self.graph.len() as u32).collect();
        let k = limit.min(ids.len());
        ids.select_nth_unstable_by_key(k.saturating_sub(1), |&v| {
            std::cmp::Reverse(self.graph.reverse.degree(v))
        });
        ids.truncate(k);
        ids.into_iter()
            .map(|v| (l.x[v as usize], l.y[v as usize], l.community[v as usize]))
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
}
