//! Generate `pools.parquet`: the precomputed puzzle pools with route counts.
//!
//! Runs on the PC (BFS from every candidate endpoint plus route counting
//! wants cores; expect ~30-60 min at enwiki scale on an i9), reads the same
//! parser output the server reads, and writes one small parquet next to it.
//! Ship that file to the server's data directory and restart — the server
//! picks it up at startup, and refuses it loudly if it was computed for a
//! different dump.
//!
//!   cargo build --release --bin pools
//!   ./target/release/pools --data data
//!   scp data/pools.parquet user@VM:~/wiki-graph/data/  && restart the game

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;

use wiki_parser::game::{load_article_flags, load_endpoint_deny, playable_pool, Difficulty};
use wiki_parser::graph::Graph;
use wiki_parser::pools::{self, Pools, POOL_FILE};

#[derive(Parser, Debug)]
#[command(name = "pools", about = "Precompute wiki-race puzzle pools with route counts")]
struct Args {
    /// Directory holding titles.parquet / edges.parquet
    #[arg(short, long, default_value = "data")]
    data: PathBuf,

    /// Output file; defaults to <data>/pools.parquet
    #[arg(long)]
    out: Option<PathBuf>,

    /// How many of the most-linked playable articles serve as endpoints.
    /// More buys variety, not better difficulty — and endpoints beyond the
    /// head get obscure fast.
    #[arg(long, default_value_t = 20_000)]
    sources: usize,

    /// Kept pairs per (difficulty, par) bucket. Route counting runs on every
    /// kept pair at 31-142 ms each, so this cap is what bounds the wall
    /// clock.
    #[arg(long, default_value_t = 25_000)]
    per_bucket: usize,

    /// Worker threads; 0 = all cores
    #[arg(long, default_value_t = 0)]
    threads: usize,
}

fn main() -> Result<()> {
    let a = Args::parse();
    let threads = if a.threads == 0 {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8)
    } else {
        a.threads
    };

    eprintln!("loading graph from {}...", a.data.display());
    let t0 = Instant::now();
    let graph = Graph::load(&a.data)?;
    eprintln!(
        "  {} articles / {} edges in {:.1?}",
        graph.len(),
        graph.forward.edges(),
        t0.elapsed()
    );

    // Same curation the server applies — manual deny list plus the disambig
    // flag — so an excluded endpoint cannot hide in the precomputed pairs.
    let flags = load_article_flags(&a.data, graph.len())?;
    let playable = playable_pool(&graph, &load_endpoint_deny(&a.data, &graph), &flags);
    let mut out_pools = Pools::empty();

    for d in [Difficulty::Easy, Difficulty::Medium, Difficulty::Hard] {
        let (ban, min_len) = d.rules();

        // Endpoints must be legal under this difficulty's hub ban — a banned
        // endpoint could never appear in a race the server would serve.
        let rev = &graph.reverse;
        let mut cand: Vec<u32> = playable
            .iter()
            .copied()
            .filter(|&v| ban.is_none_or(|limit| rev.degree(v) <= limit))
            .collect();
        cand.sort_unstable_by_key(|&v| std::cmp::Reverse(rev.degree(v)));
        cand.truncate(a.sources);

        eprintln!("{d:?}: {} endpoints, BFS pass on {threads} threads...", cand.len());
        let t = Instant::now();
        let buckets = pools::generate(&graph, &cand, min_len, ban, a.per_bucket, threads);
        let kept: usize = buckets.iter().map(|b| b.len()).sum();
        eprintln!("  kept {kept} pairs in {:.1?}; counting routes...", t.elapsed());

        let t = Instant::now();
        let all: Vec<(u32, u32)> = buckets.into_iter().flatten().collect();
        let counted = pools::attach_routes(&graph, &all, min_len, ban, threads);
        for (off, bucket) in counted.into_iter().enumerate() {
            out_pools.set(d, off, bucket);
        }
        eprintln!("  routes counted in {:.1?}", t.elapsed());
    }

    let out = a.out.unwrap_or_else(|| a.data.join(POOL_FILE));
    let rows = out_pools.write(&out, graph.len(), graph.forward.edges())?;
    eprintln!("wrote {rows} rows to {}", out.display());
    eprintln!("  {}", out_pools.summary());
    Ok(())
}
