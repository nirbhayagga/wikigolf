//! Precomputed puzzle pools: the designed difficulty curve.
//!
//! Rejection sampling cannot make hard races: 72.3% of playable pairs are
//! par 3 and par 5 is one in a thousand, so "generate until par >= 5" means
//! ~1,000 bidirectional searches per puzzle. And par alone is a blunt signal —
//! four clicks with one viable route is a different game from four clicks
//! with two hundred. This module precomputes, on the PC, a curated set of
//! pairs bucketed by (difficulty, par) with the number of distinct optimal
//! routes attached, and the server draws from the buckets with weights that
//! favour interesting races — hard from the few-routes end, easy from the
//! forgiving end.
//!
//! The full 20k x 20k distance matrix never exists anywhere: pairs are
//! bucketed with a reservoir while the BFS runs, and route counting happens
//! only for the pairs that survive the reservoir (~10^5, minutes in
//! parallel, not the hours all 4x10^8 pairs would cost). The shipped file is
//! a few MB of (difficulty, par, src, dst, routes) rows.
//!
//! Ids are parse-order dense, so a pools file is only meaningful against the
//! dump it was computed from. The parquet carries the graph's article and
//! edge counts as metadata and `load` refuses a mismatch outright — a stale
//! pool would serve races whose endpoints are silently different articles.

use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use arrow::array::{ArrayRef, UInt32Array, UInt8Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;

use crate::game::{Difficulty, Rng};
use crate::graph::{Graph, PathFinder};

/// Accepted pars per difficulty span `min_len ..= min_len + PAR_SPAN - 1`,
/// mirroring the `optimal <= min_len + 3` cap in puzzle generation.
pub const PAR_SPAN: usize = 4;

pub const POOL_FILE: &str = "pools.parquet";

/// (start, goal, number of distinct optimal routes, saturated at u32::MAX).
pub type Pair = (u32, u32, u32);

/// How the server weights the par buckets when drawing. Rejection sampling
/// gave the natural distribution (nearly all par 3); these are the designed
/// one. Empty buckets drop out and the rest renormalize, so a wiki with no
/// par-6 pairs at all still serves puzzles.
const WEIGHTS: [[u64; PAR_SPAN]; 3] = [
    [60, 30, 8, 2],  // easy:   mostly the classic 3-click race
    [40, 40, 15, 5], // medium: even split of 3s and 4s, real chance of long
    [50, 35, 10, 5], // hard:   par 4 floor already; half the draws go higher
];

fn diff_index(d: Difficulty) -> usize {
    match d {
        Difficulty::Easy => 0,
        Difficulty::Medium => 1,
        Difficulty::Hard => 2,
    }
}

fn diff_from_index(i: usize) -> Difficulty {
    match i {
        0 => Difficulty::Easy,
        1 => Difficulty::Medium,
        _ => Difficulty::Hard,
    }
}

pub struct Pools {
    /// `buckets[difficulty][par - min_len]` -> qualifying pairs, sorted by
    /// route count ascending so `pick` can address the precise or the
    /// forgiving end of a bucket by index range.
    buckets: [[Vec<Pair>; PAR_SPAN]; 3],
}

impl Pools {
    pub fn empty() -> Pools {
        Pools { buckets: Default::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.buckets.iter().flatten().all(|b| b.is_empty())
    }

    pub fn set(&mut self, d: Difficulty, par_offset: usize, mut pairs: Vec<Pair>) {
        pairs.sort_unstable_by_key(|&(s, t, r)| (r, s, t));
        self.buckets[diff_index(d)][par_offset] = pairs;
    }

    /// Draw one candidate pair. Weighted choice over the non-empty par
    /// buckets, then uniform within the difficulty's slice of the bucket:
    /// buckets are sorted by route count, and hard draws from the low
    /// (single-route, precise) half while easy draws from the high
    /// (many-routes, forgiving) half. Deterministic in the Rng, so the
    /// seeded daily stays a pure function of its seed.
    pub fn pick(&self, d: Difficulty, rng: &mut Rng) -> Option<(u32, u32)> {
        let di = diff_index(d);
        let total: u64 = (0..PAR_SPAN)
            .map(|i| if self.buckets[di][i].is_empty() { 0 } else { WEIGHTS[di][i] })
            .sum();
        if total == 0 {
            return None;
        }
        let mut r = rng.next_u64() % total;
        for (b, &weight) in self.buckets[di].iter().zip(&WEIGHTS[di]) {
            if b.is_empty() {
                continue;
            }
            if r < weight {
                let (lo, hi) = match d {
                    // Halves overlap at the midpoint so a 1-element bucket
                    // serves every difficulty.
                    Difficulty::Easy => (b.len() / 2, b.len()),
                    Difficulty::Medium => (0, b.len()),
                    Difficulty::Hard => (0, b.len().div_ceil(2)),
                };
                let k = lo + (rng.next_u64() % (hi - lo) as u64) as usize;
                let (s, t, _) = b[k];
                return Some((s, t));
            }
            r -= weight;
        }
        None
    }

    /// Like `pick`, but returning the full record — the static exporter
    /// bakes these into random/{difficulty}.json so the file-only build can
    /// serve random races with par and route count already attached.
    pub fn pick_full(&self, d: Difficulty, rng: &mut Rng) -> Option<(u32, u32, usize, u32)> {
        let di = diff_index(d);
        let (_, min_len) = d.rules();
        let total: u64 = (0..PAR_SPAN)
            .map(|i| if self.buckets[di][i].is_empty() { 0 } else { WEIGHTS[di][i] })
            .sum();
        if total == 0 {
            return None;
        }
        let mut r = rng.next_u64() % total;
        for (off, (b, &weight)) in self.buckets[di].iter().zip(&WEIGHTS[di]).enumerate() {
            if b.is_empty() {
                continue;
            }
            if r < weight {
                let (s, t, routes) = b[(rng.next_u64() % b.len() as u64) as usize];
                return Some((s, t, min_len + off, routes));
            }
            r -= weight;
        }
        None
    }

    /// Draw from one exact par bucket — how a course hole gets its designed
    /// par. Full bucket range (no route-count halving): a round mixes skill
    /// levels by construction.
    pub fn pick_par(&self, d: Difficulty, par: usize, rng: &mut Rng) -> Option<(u32, u32)> {
        let (_, min_len) = d.rules();
        let off = par.checked_sub(min_len)?;
        let b = self.buckets.get(diff_index(d))?.get(off)?;
        if b.is_empty() {
            return None;
        }
        let (s, t, _) = b[(rng.next_u64() % b.len() as u64) as usize];
        Some((s, t))
    }

    /// One line per difficulty for startup logs and the generator.
    pub fn summary(&self) -> String {
        (0..3)
            .map(|di| {
                let (_, min_len) = diff_from_index(di).rules();
                let per: Vec<String> = (0..PAR_SPAN)
                    .map(|i| format!("par{}:{}", min_len + i, self.buckets[di][i].len()))
                    .collect();
                format!("{:?} [{}]", diff_from_index(di), per.join(" "))
            })
            .collect::<Vec<_>>()
            .join("  ")
    }

    pub fn write(&self, path: &Path, n_articles: usize, n_edges: usize) -> Result<u64> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("difficulty", DataType::UInt8, false),
            Field::new("par", DataType::UInt8, false),
            Field::new("src", DataType::UInt32, false),
            Field::new("dst", DataType::UInt32, false),
            Field::new("routes", DataType::UInt32, false),
        ]));
        let props = WriterProperties::builder()
            .set_key_value_metadata(Some(vec![
                parquet::file::metadata::KeyValue::new(
                    "wiki_articles".to_string(),
                    n_articles.to_string(),
                ),
                parquet::file::metadata::KeyValue::new(
                    "wiki_edges".to_string(),
                    n_edges.to_string(),
                ),
            ]))
            .build();
        let mut writer = ArrowWriter::try_new(File::create(path)?, schema.clone(), Some(props))?;

        let (mut cd, mut cp, mut cs, mut ct, mut cr) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for di in 0..3 {
            let (_, min_len) = diff_from_index(di).rules();
            for (off, bucket) in self.buckets[di].iter().enumerate() {
                for &(s, t, routes) in bucket {
                    cd.push(di as u8);
                    cp.push((min_len + off) as u8);
                    cs.push(s);
                    ct.push(t);
                    cr.push(routes);
                }
            }
        }
        let rows = cd.len() as u64;
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(UInt8Array::from(cd)) as ArrayRef,
                Arc::new(UInt8Array::from(cp)) as ArrayRef,
                Arc::new(UInt32Array::from(cs)) as ArrayRef,
                Arc::new(UInt32Array::from(ct)) as ArrayRef,
                Arc::new(UInt32Array::from(cr)) as ArrayRef,
            ],
        )?;
        writer.write(&batch)?;
        writer.close()?;
        Ok(rows)
    }

    /// Missing file is not an error — pools are an enhancement. A file whose
    /// fingerprint disagrees with the loaded graph IS an error, and a loud
    /// one: after a new dump every id means a different article.
    pub fn load(path: &Path, n_articles: usize, n_edges: usize) -> Result<Option<Pools>> {
        if !path.exists() {
            return Ok(None);
        }
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;

        let kv = builder
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .cloned()
            .unwrap_or_default();
        let get = |k: &str| -> Option<u64> {
            kv.iter()
                .find(|e| e.key == k)
                .and_then(|e| e.value.as_ref())
                .and_then(|v| v.parse().ok())
        };
        let (fa, fe) = match (get("wiki_articles"), get("wiki_edges")) {
            (Some(a), Some(e)) => (a, e),
            _ => bail!(
                "{} has no graph fingerprint — not a pools file",
                path.display()
            ),
        };
        if fa != n_articles as u64 || fe != n_edges as u64 {
            bail!(
                "{} was computed for a different dump ({} articles / {} edges, \
                 graph has {} / {}). Regenerate it with the pools binary — \
                 stale pools would race between silently different articles.",
                path.display(),
                fa,
                fe,
                n_articles,
                n_edges
            );
        }

        let mut raw: [[Vec<Pair>; PAR_SPAN]; 3] = Default::default();
        for batch in builder.build()? {
            let batch = batch?;
            let d = batch
                .column(0)
                .as_any()
                .downcast_ref::<UInt8Array>()
                .context("pools column 0 is not uint8")?;
            let p = batch
                .column(1)
                .as_any()
                .downcast_ref::<UInt8Array>()
                .context("pools column 1 is not uint8")?;
            let s = batch
                .column(2)
                .as_any()
                .downcast_ref::<UInt32Array>()
                .context("pools column 2 is not uint32")?;
            let t = batch
                .column(3)
                .as_any()
                .downcast_ref::<UInt32Array>()
                .context("pools column 3 is not uint32")?;
            let r = batch
                .column(4)
                .as_any()
                .downcast_ref::<UInt32Array>()
                .context("pools column 4 is not uint32")?;
            for i in 0..batch.num_rows() {
                let di = d.value(i) as usize;
                if di > 2 {
                    bail!("pools row has difficulty {di}");
                }
                let (_, min_len) = diff_from_index(di).rules();
                let par = p.value(i) as usize;
                if par < min_len || par >= min_len + PAR_SPAN {
                    bail!("pools row has par {par} outside {:?}'s range", diff_from_index(di));
                }
                let (a, b) = (s.value(i), t.value(i));
                if a as usize >= n_articles || b as usize >= n_articles {
                    bail!("pools row references article id beyond the graph");
                }
                raw[di][par - min_len].push((a, b, r.value(i)));
            }
        }
        let mut pools = Pools::empty();
        for (di, row) in raw.iter_mut().enumerate() {
            for (off, bucket) in row.iter_mut().enumerate() {
                pools.set(diff_from_index(di), off, std::mem::take(bucket));
            }
        }
        Ok(Some(pools))
    }
}

/// One difficulty's candidate pairs, found by parallel forward BFS from each
/// candidate. `candidates` must already respect the difficulty's hub ban
/// (banned endpoints can never appear in a legal race) and should be the
/// most-linked playable articles — they double as the target set.
///
/// Reservoir-sampled per par bucket, so memory stays bounded no matter how
/// many hundred million par-3 pairs exist. Route counts are NOT computed
/// here — that is `attach_routes`, run only on the survivors.
pub fn generate(
    graph: &Graph,
    candidates: &[u32],
    min_len: usize,
    ban_degree: Option<usize>,
    per_bucket: usize,
    threads: usize,
) -> [Vec<(u32, u32)>; PAR_SPAN] {
    let n = graph.len();
    let max_depth = (min_len + PAR_SPAN - 1) as u8;

    let threads = threads.max(1);
    let cursor = AtomicUsize::new(0);
    let mut per_thread: Vec<[ThreadBucket; PAR_SPAN]> = Vec::new();

    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|ti| {
                let cursor = &cursor;
                scope.spawn(move || {
                    let mut dist = vec![u8::MAX; n];
                    let mut queue: Vec<u32> = Vec::new();
                    let mut rng = Rng::new(0x9E37_79B9 ^ ((ti as u64) << 32));
                    let mut buckets: [ThreadBucket; PAR_SPAN] = Default::default();

                    loop {
                        let i = cursor.fetch_add(1, Ordering::Relaxed);
                        if i >= candidates.len() {
                            break;
                        }
                        let src = candidates[i];
                        bfs_forward(graph, src, ban_degree, max_depth, &mut dist, &mut queue);
                        for &t in candidates {
                            let d = dist[t as usize] as usize;
                            if t != src && d >= min_len && d < min_len + PAR_SPAN {
                                buckets[d - min_len].offer((src, t), per_bucket, &mut rng);
                            }
                        }
                    }
                    buckets
                })
            })
            .collect();
        for h in handles {
            per_thread.push(h.join().expect("pool worker panicked"));
        }
    });

    // Merge: concatenate the per-thread reservoirs, then shuffle-truncate any
    // bucket that overflows the cap. Uniformity is approximate across threads,
    // which is fine — these are game puzzles, not statistics.
    let mut rng = Rng::new(0xDEAD_BEEF);
    let mut out: [Vec<(u32, u32)>; PAR_SPAN] = Default::default();
    for off in 0..PAR_SPAN {
        let mut all: Vec<(u32, u32)> = per_thread
            .iter_mut()
            .flat_map(|b| std::mem::take(&mut b[off].pairs))
            .collect();
        if all.len() > per_bucket {
            // Partial Fisher-Yates: the first per_bucket slots become a
            // uniform sample of the whole vector.
            for i in 0..per_bucket {
                let j = i + (rng.next_u64() % (all.len() - i) as u64) as usize;
                all.swap(i, j);
            }
            all.truncate(per_bucket);
        }
        all.sort_unstable(); // deterministic output order
        out[off] = all;
    }
    out
}

/// Count the distinct optimal routes for each surviving pair, in parallel,
/// and re-bucket by the counting pass's own optimal length. The two passes
/// use the same ban, so the pars agree; trusting the second pass anyway
/// means a disagreement drops the pair instead of shipping a lie. This is
/// the expensive step — 31-142 ms per pair at enwiki scale — which is
/// exactly why it runs on reservoir survivors and not on all 4x10^8 pairs.
pub fn attach_routes(
    graph: &Graph,
    pairs: &[(u32, u32)],
    min_len: usize,
    ban_degree: Option<usize>,
    threads: usize,
) -> [Vec<Pair>; PAR_SPAN] {
    let threads = threads.max(1);
    let cursor = AtomicUsize::new(0);
    let max_depth = (min_len + PAR_SPAN) as u8;
    let mut per_thread: Vec<[Vec<Pair>; PAR_SPAN]> = Vec::new();

    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let cursor = &cursor;
                scope.spawn(move || {
                    let mut pf = PathFinder::new(graph.len());
                    let rev = &graph.reverse;
                    let banned = move |v: u32| {
                        ban_degree.is_some_and(|limit| rev.degree(v) > limit)
                    };
                    let mut out: [Vec<Pair>; PAR_SPAN] = Default::default();
                    loop {
                        let i = cursor.fetch_add(1, Ordering::Relaxed);
                        if i >= pairs.len() {
                            break;
                        }
                        let (s, t) = pairs[i];
                        let Some((optimal, routes)) =
                            pf.count_shortest_paths(graph, s, t, &banned, max_depth)
                        else {
                            continue;
                        };
                        if optimal >= min_len && optimal < min_len + PAR_SPAN {
                            let routes = routes.min(u32::MAX as u64) as u32;
                            out[optimal - min_len].push((s, t, routes));
                        }
                    }
                    out
                })
            })
            .collect();
        for h in handles {
            per_thread.push(h.join().expect("route counter panicked"));
        }
    });

    let mut out: [Vec<Pair>; PAR_SPAN] = Default::default();
    for buckets in per_thread.iter_mut() {
        for off in 0..PAR_SPAN {
            out[off].append(&mut buckets[off]);
        }
    }
    out
}

#[derive(Default)]
struct ThreadBucket {
    pairs: Vec<(u32, u32)>,
    seen: u64,
}

impl ThreadBucket {
    fn offer(&mut self, pair: (u32, u32), cap: usize, rng: &mut Rng) {
        self.seen += 1;
        if self.pairs.len() < cap {
            self.pairs.push(pair);
        } else {
            let r = rng.next_u64() % self.seen;
            if (r as usize) < cap {
                self.pairs[r as usize] = pair;
            }
        }
    }
}

/// Plain forward BFS with a depth cap, skipping banned vertices. `dist` is
/// reset wholesale — at 7.2M entries that is a ~7 MB memset, trivial next to
/// the traversal it precedes.
fn bfs_forward(
    graph: &Graph,
    src: u32,
    ban_degree: Option<usize>,
    max_depth: u8,
    dist: &mut [u8],
    queue: &mut Vec<u32>,
) {
    dist.fill(u8::MAX);
    dist[src as usize] = 0;
    queue.clear();
    queue.push(src);
    let mut head = 0;
    while head < queue.len() {
        let v = queue[head];
        head += 1;
        let dv = dist[v as usize];
        if dv >= max_depth {
            continue;
        }
        for &w in graph.forward.neighbors(v) {
            if dist[w as usize] != u8::MAX {
                continue;
            }
            if let Some(limit) = ban_degree {
                if graph.reverse.degree(w) > limit {
                    continue;
                }
            }
            dist[w as usize] = dv + 1;
            queue.push(w);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::tests_support::graph;

    /// 0 -> 1 -> 2 -> 3 -> 4, plus a hub 5 that shortcuts 0 -> 5 -> 4.
    fn line_with_hub() -> Graph {
        graph(
            6,
            &[(0, 1), (1, 2), (2, 3), (3, 4), (0, 5), (5, 4), (1, 5), (2, 5), (3, 5)],
        )
    }

    #[test]
    fn bfs_respects_ban_and_depth() {
        let g = line_with_hub();
        let mut dist = vec![u8::MAX; 6];
        let mut q = Vec::new();
        // Unbanned: the hub makes 0 -> 4 two clicks.
        bfs_forward(&g, 0, None, 8, &mut dist, &mut q);
        assert_eq!(dist[4], 2);
        // Ban the hub (in-degree 4 > 2): the long way round is 4 clicks.
        bfs_forward(&g, 0, Some(2), 8, &mut dist, &mut q);
        assert_eq!(dist[4], 4);
        // A depth cap below that leaves 4 unreached.
        bfs_forward(&g, 0, Some(2), 3, &mut dist, &mut q);
        assert_eq!(dist[4], u8::MAX);
    }

    #[test]
    fn generate_buckets_by_par() {
        let g = line_with_hub();
        let candidates = [0u32, 1, 2, 3, 4];
        // min_len 2, hub banned: qualifying pars are 2..=5.
        let buckets = generate(&g, &candidates, 2, Some(2), 100, 2);
        // 0->2, 1->3, 2->4 are par 2; 0->3, 1->4 par 3; 0->4 par 4.
        assert_eq!(buckets[0], vec![(0, 2), (1, 3), (2, 4)]);
        assert_eq!(buckets[1], vec![(0, 3), (1, 4)]);
        assert_eq!(buckets[2], vec![(0, 4)]);
        assert!(buckets[3].is_empty());
    }

    #[test]
    fn reservoir_caps_a_bucket() {
        let g = line_with_hub();
        let candidates = [0u32, 1, 2, 3, 4];
        let buckets = generate(&g, &candidates, 2, Some(2), 2, 1);
        assert_eq!(buckets[0].len(), 2, "par-2 bucket must be capped at 2");
    }

    #[test]
    fn attach_routes_counts_the_dag() {
        // Diamond: two distinct optimal routes 0 -> 3.
        let g = graph(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]);
        let buckets = attach_routes(&g, &[(0, 3)], 2, None, 2);
        assert_eq!(buckets[0], vec![(0, 3, 2)]);

        // The banned line has exactly one route 0 -> 4 at par 4.
        let g = line_with_hub();
        let buckets = attach_routes(&g, &[(0, 4)], 4, Some(2), 1);
        assert_eq!(buckets[0], vec![(0, 4, 1)]);
    }

    #[test]
    fn write_load_round_trip_and_fingerprint() {
        let dir = std::env::temp_dir().join(format!("wp-pools-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pools.parquet");

        let mut p = Pools::empty();
        p.set(Difficulty::Easy, 0, vec![(1, 2, 9), (3, 4, 1)]);
        p.set(Difficulty::Hard, 1, vec![(5, 6, 3)]);
        p.write(&path, 100, 999).unwrap();

        let loaded = Pools::load(&path, 100, 999).unwrap().unwrap();
        // set() sorts by route count, and load re-sorts the same way.
        assert_eq!(loaded.buckets[0][0], vec![(3, 4, 1), (1, 2, 9)]);
        assert_eq!(loaded.buckets[2][1], vec![(5, 6, 3)]);

        // Same file against a different graph must refuse, not degrade.
        assert!(Pools::load(&path, 101, 999).is_err());
        assert!(Pools::load(&path, 100, 1000).is_err());
        // A missing file is simply absent.
        assert!(Pools::load(&dir.join("nope.parquet"), 1, 1).unwrap().is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pick_is_deterministic_and_respects_route_halves() {
        let mut p = Pools::empty();
        // Two pairs in one bucket: routes 1 (precise) and 50 (forgiving).
        p.set(Difficulty::Easy, 0, vec![(1, 2, 50), (3, 4, 1)]);
        p.set(Difficulty::Hard, 0, vec![(1, 2, 50), (3, 4, 1)]);
        let mut rng = Rng::new(9);
        for _ in 0..20 {
            // Easy must draw from the many-routes half, hard from the
            // few-routes half.
            assert_eq!(p.pick(Difficulty::Easy, &mut rng), Some((1, 2)));
            assert_eq!(p.pick(Difficulty::Hard, &mut rng), Some((3, 4)));
        }
        // Determinism: same seed, same draw.
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        assert_eq!(p.pick(Difficulty::Medium, &mut a), p.pick(Difficulty::Medium, &mut b));
        assert!(Pools::empty().pick(Difficulty::Easy, &mut a).is_none());
    }
}
