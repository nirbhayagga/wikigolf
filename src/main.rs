//! wiki-parser — MediaWiki XML dump -> integer article link graph.
//!
//! Two passes over the dump:
//!   1. Build the article identity table (titles + resolved redirects).
//!   2. Emit `(src, dst)` int32 edges, dropping red links and namespace links.
//!
//! Output is Parquet, ready for cuGraph/pandas with no further preprocessing.
//! Peak memory is ~2 GB on full English Wikipedia, so this runs on a laptop.

use wiki_parser::{edges, index, output, titles, wikitext};

use anyhow::{bail, Context, Result};
use clap::Parser;
use mimalloc::MiMalloc;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const READ_BUF: usize = 8 << 20;

#[derive(Parser, Debug)]
#[command(name = "wiki-parser", version, about = "Wikipedia XML dump -> integer link graph")]
struct Args {
    /// Path to *-pages-articles*.xml.bz2 (or an already-decompressed .xml)
    dump: PathBuf,

    /// Directory for titles.parquet / redirects.parquet / edges.parquet
    #[arg(short, long, default_value = "data")]
    out: PathBuf,

    /// Decompressor for .bz2 input (default: lbzip2, then pbzip2, then bzip2)
    #[arg(long)]
    decompressor: Option<String>,

    /// Drop {{template}} contents before extracting links (excludes infobox links)
    #[arg(long)]
    strip_templates: bool,

    /// Drop <ref>...</ref> citation bodies before extracting links
    #[arg(long)]
    strip_refs: bool,

    /// Keep the References / External links / Further reading sections
    #[arg(long)]
    keep_citation_sections: bool,

    /// Stop after pass 1 (writes titles.parquet and redirects.parquet only)
    #[arg(long)]
    titles_only: bool,
}

/// Wait for the decompressor and surface a non-zero exit. Without this a
/// truncated `.bz2` would look like a clean end-of-dump: the XML parser sees
/// EOF, reports success, and you get a silently partial graph.
fn reap(child: Option<Child>) -> Result<()> {
    if let Some(mut c) = child {
        let status = c.wait()?;
        if !status.success() {
            bail!("decompressor exited with {status} — the dump is likely truncated");
        }
    }
    Ok(())
}

fn pick_decompressor(explicit: &Option<String>) -> Result<String> {
    if let Some(d) = explicit {
        return Ok(d.clone());
    }
    // lbzip2 first: it parallel-decompresses arbitrary multi-stream bz2, which
    // is exactly what Wikimedia's -multistream dumps are. pbzip2 only fully
    // supports files it compressed itself and was observed to die with SIGSEGV
    // 4.2M pages into the full enwiki dump (sha1-verified, so the fault was
    // pbzip2's, not the data's). bzip2 is the single-threaded last resort.
    for cand in ["lbzip2", "pbzip2", "bzip2"] {
        let found = Command::new(cand)
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
        if found {
            return Ok(cand.to_string());
        }
    }
    bail!("no bzip2 decompressor found; install pbzip2 or pass --decompressor")
}

fn open(path: &Path, decompressor: &str) -> Result<(Box<dyn BufRead>, Option<Child>)> {
    if !path.exists() {
        bail!("dump not found: {}", path.display());
    }
    if path.extension().and_then(|e| e.to_str()) == Some("bz2") {
        let mut child = Command::new(decompressor)
            .arg("-dc")
            .arg(path)
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn {decompressor}"))?;
        let out = child.stdout.take().expect("stdout was piped");
        Ok((Box::new(BufReader::with_capacity(READ_BUF, out)), Some(child)))
    } else {
        let f = File::open(path)?;
        Ok((Box::new(BufReader::with_capacity(READ_BUF, f)), None))
    }
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * part as f64 / whole as f64
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let opts = wikitext::CleanOpts {
        strip_templates: args.strip_templates,
        strip_refs: args.strip_refs,
        cut_citation_sections: !args.keep_citation_sections,
    };

    std::fs::create_dir_all(&args.out)?;
    let decompressor = pick_decompressor(&args.decompressor)?;
    let started = std::time::Instant::now();

    eprintln!("wiki-parser {}", env!("CARGO_PKG_VERSION"));
    eprintln!("   dump:      {}", args.dump.display());
    eprintln!("   out:       {}", args.out.display());
    eprintln!(
        "   links:     templates {}, refs {}, citation sections {}",
        if opts.strip_templates { "stripped" } else { "kept" },
        if opts.strip_refs { "stripped" } else { "kept" },
        if opts.cut_citation_sections { "cut" } else { "kept" },
    );

    // ---- Pass 1: article identity -----------------------------------------
    eprintln!("\n[1/2] Building title index (articles + redirects)");
    let (reader, child) = open(&args.dump, &decompressor)?;
    let (idx, dump_stats, p1) = index::build(reader)?;
    reap(child)?;

    let ns = titles::NsPrefixes::from_dump(&dump_stats.namespaces);
    eprintln!("   pages in dump:      {:>12}", dump_stats.pages);
    eprintln!("   ns=0 pages:         {:>12}", p1.ns0_pages);
    eprintln!("   articles:           {:>12}", p1.articles);
    eprintln!(
        "   redirects:          {:>12}  ({} broken)",
        p1.redirects, p1.broken_redirects
    );
    eprintln!("   namespaces declared:{:>12}", dump_stats.namespaces.len());
    if p1.duplicate_titles > 0 {
        // MediaWiki titles are unique per namespace, so every one of these is
        // our normalization merging two distinct real pages — and when an
        // article collides with a redirect, the article is deleted.
        eprintln!(
            "   ⚠ titles colliding after normalization: {}  ({} article(s) lost)",
            p1.duplicate_titles,
            p1.articles - idx.n_articles as u64
        );
        for (raw, normalized) in p1.collisions.iter().take(10) {
            eprintln!("       {raw:?} → {normalized:?}");
        }
        if p1.collisions.len() > 10 {
            eprintln!("       ... and {} more", p1.collisions.len() - 10);
        }
    }
    if dump_stats.unresolved_entities > 0 {
        eprintln!(
            "   ⚠ unresolved entities: {} (titles may be wrong)",
            dump_stats.unresolved_entities
        );
    }

    let titles_path = args.out.join("titles.parquet");
    let n = output::write_titles(&titles_path, "id", "title", idx.articles())?;
    eprintln!("   → {} ({} rows)", titles_path.display(), n);

    let redirects_path = args.out.join("redirects.parquet");
    let n = output::write_titles(
        &redirects_path,
        "article_id",
        "alias",
        idx.redirects().map(|(t, id)| (id, t)),
    )?;
    eprintln!("   → {} ({} rows)", redirects_path.display(), n);

    if args.titles_only {
        eprintln!("\nStopped after pass 1 (--titles-only). {:.1}s", started.elapsed().as_secs_f64());
        return Ok(());
    }

    // ---- Pass 2: edges ----------------------------------------------------
    eprintln!("\n[2/2] Extracting links");
    let (reader2, child2) = open(&args.dump, &decompressor)?;
    let edges_path = args.out.join("edges.parquet");
    let cats_path = args.out.join("categories.parquet");
    let (p2, sizes) = edges::build(reader2, &idx, &ns, &opts, &edges_path, &cats_path)?;
    reap(child2)?;

    eprintln!("   pages scanned:      {:>12}", p2.pages_scanned);
    eprintln!("   raw links seen:     {:>12}", p2.links_seen);
    eprintln!(
        "   dropped: namespace  {:>12}  ({:.1}%)",
        p2.skipped_namespace,
        pct(p2.skipped_namespace, p2.links_seen)
    );
    eprintln!(
        "   dropped: red links  {:>12}  ({:.1}%)",
        p2.red_links,
        pct(p2.red_links, p2.links_seen)
    );
    eprintln!(
        "   dropped: duplicates {:>12}  ({:.1}%)",
        p2.duplicate_links,
        pct(p2.duplicate_links, p2.links_seen)
    );
    eprintln!("   dropped: self links {:>12}", p2.self_links);
    if p2.duplicate_pages > 0 {
        eprintln!("   ⚠ duplicate source pages skipped: {}", p2.duplicate_pages);
    }
    eprintln!("   → {} ({} edges)", edges_path.display(), p2.edges_written);

    // Categories and article sizes ride along on pass 2: both are gathered
    // where the wikitext is already in hand, and neither is worth its own
    // traversal of a 27 GB dump.
    eprintln!(
        "   categories kept:    {:>12}  (dropped {} maintenance)",
        p2.categories_written, p2.categories_skipped_maintenance
    );
    eprintln!("   → {} ({} rows)", cats_path.display(), p2.categories_written);

    let sizes_path = args.out.join("article_sizes.parquet");
    let n_sizes = output::write_sizes(&sizes_path, &sizes)?;
    let median = {
        let mut v: Vec<u32> = sizes.iter().copied().filter(|&s| s > 0).collect();
        v.sort_unstable();
        v.get(v.len() / 2).copied().unwrap_or(0)
    };
    eprintln!(
        "   → {} ({} rows, median article {} bytes)",
        sizes_path.display(),
        n_sizes,
        median
    );

    let secs = started.elapsed().as_secs_f64();
    eprintln!("\nGraph: {} nodes, {} edges", idx.n_articles, p2.edges_written);
    eprintln!(
        "Average out-degree: {:.1}",
        p2.edges_written as f64 / idx.n_articles.max(1) as f64
    );
    eprintln!("Done in {:.1}s", secs);
    Ok(())
}
