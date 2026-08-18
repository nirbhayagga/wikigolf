//! The link graph in CSR form, plus shortest paths over it.
//!
//! This is the data structure the wiki-race service runs on. It is built
//! straight from the parser's Parquet output and needs nothing from the Python
//! pipeline — no layout, no PageRank, no GPU.
//!
//! Both directions are stored. Forward edges answer "where can I go from
//! here", reverse edges answer "what links here", and bidirectional search
//! needs both: it grows a frontier from the start *and* from the goal, which
//! is what keeps the search tractable on a graph this dense.

use anyhow::{bail, Context, Result};
use arrow::array::{Int32Array, StringArray, UInt32Array};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use rustc_hash::FxHashMap;
use std::fs::File;
use std::path::Path;

use crate::titles::normalize_title;

pub const NONE: u32 = u32::MAX;

/// A compressed sparse row adjacency list.
pub struct Csr {
    /// `offsets[v] .. offsets[v+1]` indexes into `targets`.
    offsets: Vec<u32>,
    targets: Vec<u32>,
}

impl Csr {
    #[inline]
    pub fn neighbors(&self, v: u32) -> &[u32] {
        let a = self.offsets[v as usize] as usize;
        let b = self.offsets[v as usize + 1] as usize;
        &self.targets[a..b]
    }

    #[inline]
    pub fn degree(&self, v: u32) -> usize {
        (self.offsets[v as usize + 1] - self.offsets[v as usize]) as usize
    }

    pub fn edges(&self) -> usize {
        self.targets.len()
    }
}

pub struct Graph {
    pub forward: Csr,
    pub reverse: Csr,
    pub titles: Vec<String>,
    /// Normalized title (article or redirect alias) -> article id.
    lookup: FxHashMap<Box<str>, u32>,
    pub n_redirect_aliases: usize,
}

/// Read both edge columns from the parquet, calling `f(src, dst)` per row.
fn scan_edges(path: &Path, mut f: impl FnMut(u32, u32)) -> Result<()> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    for batch in reader {
        let batch = batch?;
        let src = batch
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .context("edges.parquet column 0 (src) is not int32")?;
        let dst = batch
            .column(1)
            .as_any()
            .downcast_ref::<Int32Array>()
            .context("edges.parquet column 1 (dst) is not int32")?;
        for i in 0..batch.num_rows() {
            f(src.value(i) as u32, dst.value(i) as u32);
        }
    }
    Ok(())
}

/// Turn per-vertex counts into CSR offsets, in place.
fn counts_to_offsets(counts: &mut Vec<u32>) {
    let mut running = 0u32;
    for c in counts.iter_mut() {
        let d = *c;
        *c = running;
        running += d;
    }
    counts.push(running);
}

impl Graph {
    pub fn load(data_dir: &Path) -> Result<Self> {
        let titles_path = data_dir.join("titles.parquet");
        let edges_path = data_dir.join("edges.parquet");
        if !titles_path.exists() || !edges_path.exists() {
            bail!(
                "missing parser output in {}. Run wiki-parser first:\n  \
                 ./target/release/wiki-parser <dump>.xml.bz2 --out {}",
                data_dir.display(),
                data_dir.display()
            );
        }

        // ---- titles ------------------------------------------------------
        let file = File::open(&titles_path)?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
        let mut titles: Vec<String> = Vec::new();
        for batch in reader {
            let batch = batch?;
            let ids = batch
                .column(0)
                .as_any()
                .downcast_ref::<UInt32Array>()
                .context("titles.parquet column 0 (id) is not uint32")?;
            let names = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .context("titles.parquet column 1 (title) is not utf8")?;
            for i in 0..batch.num_rows() {
                let id = ids.value(i) as usize;
                if id >= titles.len() {
                    titles.resize(id + 1, String::new());
                }
                titles[id] = names.value(i).to_string();
            }
        }
        let n = titles.len() as u32;

        let mut lookup: FxHashMap<Box<str>, u32> = FxHashMap::default();
        lookup.reserve(titles.len() * 2);
        for (id, t) in titles.iter().enumerate() {
            if let Some(norm) = normalize_title(t) {
                lookup.insert(norm.into_boxed_str(), id as u32);
            }
        }

        // Redirect aliases make search behave the way a reader expects: typing
        // "USA" should find "United States". Articles win on collision, since
        // an alias must never shadow a real article.
        let mut n_redirect_aliases = 0usize;
        let redirects_path = data_dir.join("redirects.parquet");
        if redirects_path.exists() {
            let file = File::open(&redirects_path)?;
            let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
            for batch in reader {
                let batch = batch?;
                let ids = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<UInt32Array>()
                    .context("redirects.parquet column 0 (article_id) is not uint32")?;
                let alias = batch
                    .column(1)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .context("redirects.parquet column 1 (alias) is not utf8")?;
                for i in 0..batch.num_rows() {
                    let target = ids.value(i);
                    if target >= n {
                        continue;
                    }
                    if let Some(norm) = normalize_title(alias.value(i)) {
                        lookup.entry(norm.into_boxed_str()).or_insert_with(|| {
                            n_redirect_aliases += 1;
                            target
                        });
                    }
                }
            }
        }

        // ---- edges, two passes: count then fill --------------------------
        let mut out_counts = vec![0u32; n as usize];
        let mut in_counts = vec![0u32; n as usize];
        let mut n_edges = 0usize;
        scan_edges(&edges_path, |s, d| {
            if s < n && d < n {
                out_counts[s as usize] += 1;
                in_counts[d as usize] += 1;
                n_edges += 1;
            }
        })?;

        counts_to_offsets(&mut out_counts);
        counts_to_offsets(&mut in_counts);

        let mut fwd_targets = vec![0u32; n_edges];
        let mut rev_targets = vec![0u32; n_edges];
        let mut fwd_cursor = out_counts.clone();
        let mut rev_cursor = in_counts.clone();
        scan_edges(&edges_path, |s, d| {
            if s < n && d < n {
                fwd_targets[fwd_cursor[s as usize] as usize] = d;
                fwd_cursor[s as usize] += 1;
                rev_targets[rev_cursor[d as usize] as usize] = s;
                rev_cursor[d as usize] += 1;
            }
        })?;

        Ok(Graph {
            forward: Csr { offsets: out_counts, targets: fwd_targets },
            reverse: Csr { offsets: in_counts, targets: rev_targets },
            titles,
            lookup,
            n_redirect_aliases,
        })
    }

    pub fn len(&self) -> usize {
        self.titles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.titles.is_empty()
    }

    /// Resolve a user-typed title, through redirects, using the parser's own
    /// normalization rules.
    pub fn resolve(&self, query: &str) -> Option<u32> {
        let norm = normalize_title(query)?;
        self.lookup.get(norm.as_str()).copied()
    }

    pub fn title(&self, id: u32) -> &str {
        &self.titles[id as usize]
    }

    /// Every redirect alias with its target article, as normalized strings.
    ///
    /// The lookup map holds both real titles and aliases; the filter keeps
    /// only the aliases. Sourced from the same map `resolve` consults, so
    /// what search-by-alias finds and what typing the alias resolves to can
    /// never disagree.
    pub fn alias_entries(&self) -> impl Iterator<Item = (&str, u32)> {
        self.lookup.iter().filter_map(|(k, &id)| {
            if k.as_ref() != self.titles[id as usize].as_str() {
                Some((k.as_ref(), id))
            } else {
                None
            }
        })
    }
}

/// Reusable bidirectional BFS scratch space.
///
/// The arrays are sized once and cleared by rewinding only the vertices a
/// query actually touched. Zeroing 7.2M entries per request would dominate the
/// cost of a search that typically visits a few hundred thousand.
pub struct PathFinder {
    dist_f: Vec<u8>,
    dist_b: Vec<u8>,
    parent_f: Vec<u32>,
    parent_b: Vec<u32>,
    touched_f: Vec<u32>,
    touched_b: Vec<u32>,
    /// Shortest-route counts for the two BFS directions, used only by
    /// `count_shortest_paths`. Allocated lazily on the first count — a
    /// finder that only ever does path queries never pays the 2 x 58 MB —
    /// and reused ever after, cleared through the touched lists like the
    /// distance arrays. Allocating per call was measured to roughly double
    /// the counting phase's cost at enwiki scale.
    sigma: Vec<u64>,
    sigma_b: Vec<u64>,
}

/// Sentinel for "not reached". Real BFS depths stay far below this.
const UNSEEN: u8 = u8::MAX;

impl PathFinder {
    pub fn new(n: usize) -> Self {
        PathFinder {
            dist_f: vec![UNSEEN; n],
            dist_b: vec![UNSEEN; n],
            parent_f: vec![NONE; n],
            parent_b: vec![NONE; n],
            touched_f: Vec::new(),
            touched_b: Vec::new(),
            sigma: Vec::new(),
            sigma_b: Vec::new(),
        }
    }

    fn reset(&mut self) {
        for &v in &self.touched_f {
            self.dist_f[v as usize] = UNSEEN;
            self.parent_f[v as usize] = NONE;
            if !self.sigma.is_empty() {
                self.sigma[v as usize] = 0;
            }
        }
        for &v in &self.touched_b {
            self.dist_b[v as usize] = UNSEEN;
            self.parent_b[v as usize] = NONE;
            if !self.sigma_b.is_empty() {
                self.sigma_b[v as usize] = 0;
            }
        }
        self.touched_f.clear();
        self.touched_b.clear();
    }

    /// Shortest directed path from `start` to `goal`, as a vertex list
    /// including both endpoints. `None` if unreachable.
    ///
    /// `banned` is consulted for every intermediate vertex, which is how the
    /// hub-ban game mode works: it re-runs the same search with the mega-hubs
    /// removed. Endpoints are never banned.
    pub fn shortest_path(
        &mut self,
        g: &Graph,
        start: u32,
        goal: u32,
        banned: &dyn Fn(u32) -> bool,
    ) -> Option<Vec<u32>> {
        self.reset();
        if start == goal {
            return Some(vec![start]);
        }

        let mut frontier_f = vec![start];
        let mut frontier_b = vec![goal];
        self.dist_f[start as usize] = 0;
        self.dist_b[goal as usize] = 0;
        self.touched_f.push(start);
        self.touched_b.push(goal);

        let mut depth_f = 0u8;
        let mut depth_b = 0u8;

        while !frontier_f.is_empty() && !frontier_b.is_empty() {
            // Expand whichever side is cheaper. On a graph with hubs the two
            // frontiers grow at wildly different rates, and always expanding
            // the smaller one is what makes this finish.
            let forward = frontier_f.len() <= frontier_b.len();
            let mut next = Vec::new();
            let mut best: Option<(u32, u32)> = None; // (total, meeting vertex)

            if forward {
                depth_f += 1;
                for &v in &frontier_f {
                    for &w in g.forward.neighbors(v) {
                        if self.dist_f[w as usize] != UNSEEN {
                            continue;
                        }
                        if w != goal && banned(w) {
                            continue;
                        }
                        self.dist_f[w as usize] = depth_f;
                        self.parent_f[w as usize] = v;
                        self.touched_f.push(w);
                        if self.dist_b[w as usize] != UNSEEN {
                            let total = depth_f as u32 + self.dist_b[w as usize] as u32;
                            if best.is_none_or(|(t, _)| total < t) {
                                best = Some((total, w));
                            }
                        }
                        next.push(w);
                    }
                }
                frontier_f = next;
            } else {
                depth_b += 1;
                for &v in &frontier_b {
                    for &w in g.reverse.neighbors(v) {
                        if self.dist_b[w as usize] != UNSEEN {
                            continue;
                        }
                        if w != start && banned(w) {
                            continue;
                        }
                        self.dist_b[w as usize] = depth_b;
                        self.parent_b[w as usize] = v;
                        self.touched_b.push(w);
                        if self.dist_f[w as usize] != UNSEEN {
                            let total = self.dist_f[w as usize] as u32 + depth_b as u32;
                            if best.is_none_or(|(t, _)| total < t) {
                                best = Some((total, w));
                            }
                        }
                        next.push(w);
                    }
                }
                frontier_b = next;
            }

            // Checking only after a whole level completes is what makes the
            // result actually shortest — the first meeting found mid-level can
            // be one hop longer than another meeting in the same level.
            if let Some((_, meet)) = best {
                return Some(self.reconstruct(meet, start, goal));
            }
        }
        None
    }

    #[allow(clippy::needless_range_loop)]
    fn reconstruct(&self, meet: u32, start: u32, goal: u32) -> Vec<u32> {
        let mut head = Vec::new();
        let mut v = meet;
        while v != start {
            head.push(v);
            v = self.parent_f[v as usize];
        }
        head.push(start);
        head.reverse();

        let mut v = self.parent_b[meet as usize];
        while v != NONE {
            head.push(v);
            if v == goal {
                break;
            }
            v = self.parent_b[v as usize];
        }
        head
    }

    /// How many *distinct shortest* routes run from `start` to `goal`.
    ///
    /// Returns `(length, count)`. This is the difficulty signal that path
    /// length alone cannot give: four clicks with one viable route is a much
    /// harder puzzle than four clicks with two hundred, and the player feels
    /// the difference immediately.
    ///
    /// Bidirectional, done carefully — the naive objection is that a
    /// bidirectional search never sees the whole shortest-path DAG, and it
    /// does not need to: every shortest path of length L crosses the cut
    /// between forward-depth a and a+1 exactly once, at a vertex whose
    /// forward depth is exactly a (a smaller depth would shorten the path).
    /// So with both sides expanded level-complete and carrying path counts,
    /// the answer is Σ σ_f(v)·σ_b(v) over the meeting level. The first
    /// version expanded from one side only, which made a par-6 count walk
    /// essentially the whole graph (~2-4 s at enwiki scale, a 6-9 hour pools
    /// run); meeting in the middle makes it cost like two half-depth
    /// searches.
    ///
    /// Counts saturate rather than wrap. Between two well-connected articles
    /// the number of four-click routes runs into the billions, and a silently
    /// wrapped count is a difficulty score made of noise.
    pub fn count_shortest_paths(
        &mut self,
        g: &Graph,
        start: u32,
        goal: u32,
        banned: &dyn Fn(u32) -> bool,
        max_depth: u8,
    ) -> Option<(usize, u64)> {
        self.reset();
        if start == goal {
            return Some((0, 1));
        }
        let n = g.len();
        if start as usize >= n || goal as usize >= n {
            return None;
        }
        if self.sigma.is_empty() {
            self.sigma = vec![0; n];
            self.sigma_b = vec![0; n];
        }
        let PathFinder { dist_f, dist_b, touched_f, touched_b, sigma, sigma_b, .. } = self;

        sigma[start as usize] = 1;
        dist_f[start as usize] = 0;
        touched_f.push(start);
        sigma_b[goal as usize] = 1;
        dist_b[goal as usize] = 0;
        touched_b.push(goal);

        let mut frontier_f = vec![start];
        let mut frontier_b = vec![goal];
        let (mut depth_f, mut depth_b) = (0u8, 0u8);

        while !frontier_f.is_empty()
            && !frontier_b.is_empty()
            && depth_f + depth_b < max_depth
        {
            // Expand whichever side is cheaper, exactly like shortest_path —
            // on a hubby graph the frontiers grow at wildly different rates.
            let forward = frontier_f.len() <= frontier_b.len();
            let mut next: Vec<u32> = Vec::new();

            if forward {
                depth_f += 1;
                for &v in &frontier_f {
                    let sv = sigma[v as usize];
                    for &w in g.forward.neighbors(v) {
                        if w != goal && banned(w) {
                            continue;
                        }
                        let d = dist_f[w as usize];
                        if d == UNSEEN {
                            dist_f[w as usize] = depth_f;
                            touched_f.push(w);
                            sigma[w as usize] = sv;
                            next.push(w);
                        } else if d == depth_f {
                            sigma[w as usize] = sigma[w as usize].saturating_add(sv);
                        }
                    }
                }
            } else {
                depth_b += 1;
                for &v in &frontier_b {
                    let sv = sigma_b[v as usize];
                    for &w in g.reverse.neighbors(v) {
                        if w != start && banned(w) {
                            continue;
                        }
                        let d = dist_b[w as usize];
                        if d == UNSEEN {
                            dist_b[w as usize] = depth_b;
                            touched_b.push(w);
                            sigma_b[w as usize] = sv;
                            next.push(w);
                        } else if d == depth_b {
                            sigma_b[w as usize] = sigma_b[w as usize].saturating_add(sv);
                        }
                    }
                }
            }

            // The level is complete; look for the meet. L is the smallest
            // total over the fresh frontier, and the cut is this side's new
            // level: the position-`depth` vertex of every shortest path sits
            // in `next` (its depth on this side is exact), with the other
            // side's level at L-depth already complete.
            let (own_depth, other_dist, own_sigma, other_sigma): (u8, &Vec<u8>, _, _) = if forward
            {
                (depth_f, &*dist_b, &*sigma, &*sigma_b)
            } else {
                (depth_b, &*dist_f, &*sigma_b, &*sigma)
            };
            let mut best: Option<u8> = None;
            for &v in &next {
                let od = other_dist[v as usize];
                if od != UNSEEN {
                    let total = own_depth + od;
                    if best.is_none_or(|b| total < b) {
                        best = Some(total);
                    }
                }
            }
            if let Some(total) = best {
                if total as usize > max_depth as usize {
                    return None;
                }
                let mut count = 0u64;
                let want = total - own_depth;
                for &v in &next {
                    if other_dist[v as usize] == want {
                        count = count
                            .saturating_add(own_sigma[v as usize].saturating_mul(other_sigma[v as usize]));
                    }
                }
                return Some((total as usize, count));
            }

            if forward {
                frontier_f = next;
            } else {
                frontier_b = next;
            }
        }
        None
    }

    /// Shortest-path distance from every article *to* `goal`, capped at
    /// `max_depth`. `UNSEEN` means further than the cap, or no route at all.
    ///
    /// A BFS from `goal` across reversed edges: a vertex is at distance d if
    /// it can reach the goal in d clicks. One pass answers "how far is this
    /// link from the target" for every link the player will ever see in this
    /// race, so the compass costs one BFS per puzzle rather than one per hint.
    ///
    /// The returned vector is `n` bytes — about 7 MB at enwiki scale — so it
    /// is worth caching per goal and sharing across players on the daily.
    pub fn distances_to(&mut self, g: &Graph, goal: u32, max_depth: u8) -> Vec<u8> {
        let n = g.len();
        let mut dist = vec![UNSEEN; n];
        if (goal as usize) < n {
            dist[goal as usize] = 0;
            let mut frontier = vec![goal];
            let mut depth = 0u8;
            while !frontier.is_empty() && depth < max_depth {
                depth += 1;
                let mut next = Vec::new();
                for &v in &frontier {
                    for &w in g.reverse.neighbors(v) {
                        if dist[w as usize] == UNSEEN {
                            dist[w as usize] = depth;
                            next.push(w);
                        }
                    }
                }
                frontier = next;
            }
        }
        dist
    }
}

/// Distance-to-goal levels for the static compass, truncated to a byte
/// budget: complete BFS levels only, stopping before the level that would
/// push the total past `cap`. `levels[i]` holds every article at distance
/// i+1; anything absent is honestly "further than levels.len()". Level 1 is
/// always complete even past the cap — truncating the direct in-neighbours
/// would label a one-click finish as "far", which is worse than a big file.
pub fn near_goal_levels(g: &Graph, goal: u32, max_depth: u8, cap: usize) -> Vec<Vec<u32>> {
    let n = g.len();
    if goal as usize >= n {
        return Vec::new();
    }
    let mut seen = vec![false; n];
    seen[goal as usize] = true;
    let mut levels: Vec<Vec<u32>> = Vec::new();
    let mut cur = vec![goal];
    let mut total = 0usize;
    for depth in 1..=max_depth {
        let mut next = Vec::new();
        for &v in &cur {
            for &w in g.reverse.neighbors(v) {
                if !seen[w as usize] {
                    seen[w as usize] = true;
                    next.push(w);
                }
            }
        }
        if next.is_empty() || (depth > 1 && total + next.len() > cap) {
            break;
        }
        total += next.len();
        cur = next.clone();
        levels.push(next);
    }
    levels
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    /// Build a graph directly from an edge list, bypassing Parquet.
    pub(crate) fn graph(n: u32, edges: &[(u32, u32)]) -> Graph {
        let mut out_counts = vec![0u32; n as usize];
        let mut in_counts = vec![0u32; n as usize];
        for &(s, d) in edges {
            out_counts[s as usize] += 1;
            in_counts[d as usize] += 1;
        }
        counts_to_offsets(&mut out_counts);
        counts_to_offsets(&mut in_counts);

        let mut fwd = vec![0u32; edges.len()];
        let mut rev = vec![0u32; edges.len()];
        let mut fc = out_counts.clone();
        let mut rc = in_counts.clone();
        for &(s, d) in edges {
            fwd[fc[s as usize] as usize] = d;
            fc[s as usize] += 1;
            rev[rc[d as usize] as usize] = s;
            rc[d as usize] += 1;
        }

        let titles: Vec<String> = (0..n).map(|i| format!("A{i}")).collect();
        let mut lookup = FxHashMap::default();
        for (i, t) in titles.iter().enumerate() {
            lookup.insert(t.clone().into_boxed_str(), i as u32);
        }
        Graph {
            forward: Csr { offsets: out_counts, targets: fwd },
            reverse: Csr { offsets: in_counts, targets: rev },
            titles,
            lookup,
            n_redirect_aliases: 0,
        }
    }

    fn path(g: &Graph, s: u32, t: u32) -> Option<Vec<u32>> {
        PathFinder::new(g.len()).shortest_path(g, s, t, &|_| false)
    }

    #[test]
    fn finds_a_straight_line() {
        let g = graph(4, &[(0, 1), (1, 2), (2, 3)]);
        assert_eq!(path(&g, 0, 3), Some(vec![0, 1, 2, 3]));
    }

    #[test]
    fn picks_the_shortest_of_several_routes() {
        // 0->1->4 is 2 hops; 0->2->3->4 is 3. Both meet the backward frontier.
        let g = graph(5, &[(0, 1), (1, 4), (0, 2), (2, 3), (3, 4)]);
        let p = path(&g, 0, 4).unwrap();
        assert_eq!(p.len() - 1, 2, "got {p:?}");
        assert_eq!(p, vec![0, 1, 4]);
    }

    #[test]
    fn respects_link_direction() {
        let g = graph(2, &[(0, 1)]);
        assert!(path(&g, 0, 1).is_some());
        assert_eq!(path(&g, 1, 0), None, "edges are one-way");
    }

    #[test]
    fn unreachable_is_none() {
        let g = graph(4, &[(0, 1), (2, 3)]);
        assert_eq!(path(&g, 0, 3), None);
    }

    #[test]
    fn start_equals_goal() {
        let g = graph(2, &[(0, 1)]);
        assert_eq!(path(&g, 1, 1), Some(vec![1]));
    }

    #[test]
    fn banned_vertices_are_routed_around() {
        // 0->1->3 is shortest, but 1 is banned, so it must take 0->2->4->3.
        let g = graph(5, &[(0, 1), (1, 3), (0, 2), (2, 4), (4, 3)]);
        let mut pf = PathFinder::new(g.len());
        let p = pf.shortest_path(&g, 0, 3, &|v| v == 1).unwrap();
        assert_eq!(p, vec![0, 2, 4, 3]);
        assert!(!p.contains(&1));
    }

    #[test]
    fn endpoints_are_never_banned() {
        // A ban predicate matching everything must still connect neighbours.
        let g = graph(2, &[(0, 1)]);
        let mut pf = PathFinder::new(g.len());
        assert_eq!(pf.shortest_path(&g, 0, 1, &|_| true), Some(vec![0, 1]));
    }

    #[test]
    fn reused_finder_does_not_leak_state() {
        let g = graph(5, &[(0, 1), (1, 4), (0, 2), (2, 3), (3, 4)]);
        let mut pf = PathFinder::new(g.len());
        let first = pf.shortest_path(&g, 0, 4, &|_| false);
        let second = pf.shortest_path(&g, 0, 4, &|_| false);
        assert_eq!(first, second, "scratch space must reset between queries");
        // A different query in between must not corrupt the next one.
        pf.shortest_path(&g, 2, 4, &|_| false);
        assert_eq!(pf.shortest_path(&g, 0, 4, &|_| false), first);
    }
}

#[cfg(test)]
mod counting_tests {
    use super::tests_support::*;
    use super::*;

    /// The original single-direction counter, kept verbatim as the oracle:
    /// a full forward BFS carrying counts, expanding whole levels until the
    /// goal's level completes. Slow and simple — exactly what a reference
    /// should be.
    fn count_reference(
        g: &Graph,
        start: u32,
        goal: u32,
        banned: &dyn Fn(u32) -> bool,
        max_depth: u8,
    ) -> Option<(usize, u64)> {
        if start == goal {
            return Some((0, 1));
        }
        let n = g.len();
        let mut dist = vec![u8::MAX; n];
        let mut sigma = vec![0u64; n];
        sigma[start as usize] = 1;
        dist[start as usize] = 0;
        let mut frontier = vec![start];
        let mut depth = 0u8;
        while !frontier.is_empty() && depth < max_depth {
            depth += 1;
            let mut next = Vec::new();
            for &v in &frontier {
                let sv = sigma[v as usize];
                for &w in g.forward.neighbors(v) {
                    if w != goal && banned(w) {
                        continue;
                    }
                    let d = dist[w as usize];
                    if d == u8::MAX {
                        dist[w as usize] = depth;
                        sigma[w as usize] = sv;
                        next.push(w);
                    } else if d == depth {
                        sigma[w as usize] = sigma[w as usize].saturating_add(sv);
                    }
                }
            }
            if dist[goal as usize] == depth {
                return Some((depth as usize, sigma[goal as usize]));
            }
            frontier = next;
        }
        None
    }

    /// The truncated level BFS must agree with the full distance map on
    /// every level it emits, and may only omit whole levels past the cap —
    /// never individual articles, which would show a close link as "far".
    #[test]
    fn near_goal_levels_match_the_full_distance_map() {
        let mut seed = 0x0FED_CBA9_8765_4321u64;
        let mut rand = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };
        for trial in 0..40 {
            let n = 8 + (rand() % 20);
            let m = n * (1 + rand() % 4);
            let edges: Vec<(u32, u32)> = (0..m)
                .map(|_| (rand() % n, rand() % n))
                .filter(|(a, b)| a != b)
                .collect();
            let g = graph(n, &edges);
            let mut pf = PathFinder::new(g.len());
            for goal in 0..n {
                let dist = pf.distances_to(&g, goal, 6);
                for cap in [2usize, 10, 100_000] {
                    let levels = near_goal_levels(&g, goal, 6, cap);
                    for (i, lvl) in levels.iter().enumerate() {
                        let want: Vec<u32> =
                            (0..n).filter(|&v| dist[v as usize] == (i + 1) as u8).collect();
                        let mut got = lvl.clone();
                        got.sort_unstable();
                        assert_eq!(got, want, "trial {trial} goal {goal} cap {cap} level {}", i + 1);
                    }
                    let total: usize = levels.iter().map(|l| l.len()).sum();
                    let l1 = levels.first().map_or(0, |l| l.len());
                    assert!(total <= cap.max(l1), "trial {trial} goal {goal} cap {cap} total {total}");
                }
            }
        }
    }

    /// The meet-in-the-middle counter must agree with the oracle on every
    /// pair of every graph, banned and unbanned, at every depth cap. Random
    /// graphs from a fixed LCG so a failure reproduces.
    #[test]
    fn bidirectional_count_matches_the_oracle() {
        let mut seed = 0x0123_4567_89AB_CDEFu64;
        let mut rand = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };
        for trial in 0..40 {
            let n = 8 + (rand() % 20);
            let m = n * (1 + rand() % 4);
            let edges: Vec<(u32, u32)> = (0..m)
                .map(|_| (rand() % n, rand() % n))
                .filter(|(a, b)| a != b)
                .collect();
            let g = graph(n, &edges);
            let mut pf = PathFinder::new(g.len());
            for ban_limit in [None, Some(2usize)] {
                let banned = |v: u32| ban_limit.is_some_and(|l| g.reverse.degree(v) > l);
                for s in 0..n {
                    for t in 0..n {
                        for cap in [3u8, 5, 8] {
                            let got = pf.count_shortest_paths(&g, s, t, &banned, cap);
                            let want = count_reference(&g, s, t, &banned, cap);
                            assert_eq!(
                                got, want,
                                "trial {trial} n={n} {s}->{t} cap={cap} ban={ban_limit:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    fn count(g: &Graph, s: u32, t: u32) -> Option<(usize, u64)> {
        PathFinder::new(g.len()).count_shortest_paths(g, s, t, &|_| false, 12)
    }

    #[test]
    fn counts_a_diamond() {
        //   0 -> 1 -> 3
        //   0 -> 2 -> 3      two shortest routes, both length 2
        let g = graph(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]);
        assert_eq!(count(&g, 0, 3), Some((2, 2)));
    }

    #[test]
    fn counts_parallel_diamonds_multiplicatively() {
        // Two diamonds in series: 2 ways through the first times 2 through the
        // second is 4 distinct shortest routes of length 4.
        let g = graph(
            7,
            &[(0, 1), (0, 2), (1, 3), (2, 3), (3, 4), (3, 5), (4, 6), (5, 6)],
        );
        assert_eq!(count(&g, 0, 6), Some((4, 4)));
    }

    #[test]
    fn ignores_longer_routes() {
        // 0->1->4 is the only shortest; 0->2->3->4 is longer and must not be
        // counted even though it also arrives.
        let g = graph(5, &[(0, 1), (1, 4), (0, 2), (2, 3), (3, 4)]);
        assert_eq!(count(&g, 0, 4), Some((2, 1)));
    }

    #[test]
    fn unreachable_and_self() {
        let g = graph(3, &[(0, 1)]);
        assert_eq!(count(&g, 0, 2), None);
        assert_eq!(count(&g, 1, 1), Some((0, 1)));
    }

    #[test]
    fn respects_the_hub_ban() {
        // Every route runs through 1; banning it leaves nothing.
        let g = graph(3, &[(0, 1), (1, 2)]);
        let mut pf = PathFinder::new(g.len());
        assert_eq!(pf.count_shortest_paths(&g, 0, 2, &|v| v == 1, 12), None);
    }

    #[test]
    fn depth_cap_gives_up_rather_than_lying() {
        let g = graph(5, &[(0, 1), (1, 2), (2, 3), (3, 4)]);
        let mut pf = PathFinder::new(g.len());
        assert_eq!(pf.count_shortest_paths(&g, 0, 4, &|_| false, 2), None);
        assert_eq!(pf.count_shortest_paths(&g, 0, 4, &|_| false, 4), Some((4, 1)));
    }

    #[test]
    fn distances_to_measures_toward_the_goal() {
        // 0 -> 1 -> 2, and 3 is a dead end that reaches nothing.
        let g = graph(4, &[(0, 1), (1, 2)]);
        let d = PathFinder::new(g.len()).distances_to(&g, 2, 6);
        assert_eq!(d[2], 0, "the goal is zero from itself");
        assert_eq!(d[1], 1, "one click away");
        assert_eq!(d[0], 2, "two clicks away");
        assert_eq!(d[3], UNSEEN, "cannot reach the goal at all");
    }

    #[test]
    fn distances_to_honours_the_cap() {
        let g = graph(4, &[(0, 1), (1, 2), (2, 3)]);
        let d = PathFinder::new(g.len()).distances_to(&g, 3, 1);
        assert_eq!(d[2], 1);
        assert_eq!(d[1], UNSEEN, "past the cap is indistinguishable from absent");
    }
}
