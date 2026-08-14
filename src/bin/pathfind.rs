//! pathfind — shortest wikilink path between two articles.
//!
//! The engine behind the wiki-race modes. Runs on the parser's Parquet output
//! alone, so it needs no GPU, no RAPIDS and no layout.
//!
//!   pathfind --data data/simple "Cat" "Philosophy"
//!   pathfind --data data/simple --ban-degree 5000 "Cat" "Philosophy"

use anyhow::{bail, Result};
use clap::Parser;
use std::path::PathBuf;
use std::time::Instant;

use wiki_parser::graph::{Graph, PathFinder};

#[derive(Parser, Debug)]
#[command(name = "pathfind", about = "Shortest wikilink path between two articles")]
struct Args {
    /// Start article title (redirects and aliases accepted)
    from: String,

    /// Goal article title
    to: String,

    /// Directory holding titles.parquet / edges.parquet
    #[arg(short, long, default_value = "data")]
    data: PathBuf,

    /// Hard mode: refuse to route through articles with in-degree above this.
    /// Bans the mega-hubs that otherwise connect nearly every pair in ~3 hops.
    #[arg(long)]
    ban_degree: Option<usize>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let t = Instant::now();
    let g = Graph::load(&args.data)?;
    eprintln!(
        "loaded {} articles, {} edges, {} redirect aliases in {:.1}s",
        g.len(),
        g.forward.edges(),
        g.n_redirect_aliases,
        t.elapsed().as_secs_f64()
    );

    let Some(start) = g.resolve(&args.from) else {
        bail!("no article named {:?}", args.from);
    };
    let Some(goal) = g.resolve(&args.to) else {
        bail!("no article named {:?}", args.to);
    };

    // in-degree is "how many articles link here" — the mega-hub measure.
    // Capture the CSR by reference; moving `g` into the closure would leave
    // nothing to pass to shortest_path.
    let rev = &g.reverse;
    let banned: Box<dyn Fn(u32) -> bool + '_> = match args.ban_degree {
        Some(limit) => Box::new(move |v: u32| rev.degree(v) > limit),
        None => Box::new(|_| false),
    };

    let t = Instant::now();
    let mut pf = PathFinder::new(g.len());
    let path = pf.shortest_path(&g, start, goal, &banned);
    let secs = t.elapsed().as_secs_f64();

    match path {
        Some(p) => {
            println!("\n{} -> {}   ({} clicks)", g.title(start), g.title(goal), p.len() - 1);
            for (i, &v) in p.iter().enumerate() {
                println!("  {i:>2}. {}  (in-degree {})", g.title(v), g.reverse.degree(v));
            }
            println!("\nfound in {:.3}s", secs);
        }
        None => {
            println!("\nNo path from {:?} to {:?}", g.title(start), g.title(goal));
            if args.ban_degree.is_some() {
                println!("(with --ban-degree applied; try a higher limit)");
            }
            println!("searched in {secs:.3}s");
        }
    }
    Ok(())
}
