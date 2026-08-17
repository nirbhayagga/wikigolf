//! static_export — emit the game as a tree of static files.
//!
//! The static build serves the whole ritual (daily, round, archive, free
//! navigation, search, the map) from any dumb file host — Cloudflare Pages'
//! free tier being the target — at the cost of everything that needs a live
//! graph: custom-pair par, the compass, leaderboards. See the runbook's
//! "static build" section for the tradeoff table.
//!
//!   cargo run --release --bin static_export -- --data data --out static_site --days 45
//!
//! Layout produced:
//!   meta.json                 counts, shard scheme, day numbering
//!   regions.json              community id -> name
//!   landmarks.json / map.json the background map
//!   shards/{i}.json           articles [i*S, (i+1)*S): metadata + links
//!                             with titles denormalized, one fetch per click
//!   search/{a}-{b}.json       top-50 titles per two-char lowercase prefix
//!   daily/{n}.json            per-difficulty dailies + the 3-hole round,
//!                             pars and route counts baked in

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::json;

use wiki_parser::game::{
    course_seed, daily_seed, today_day, Difficulty, Game, Rng, COURSE_3, DAILY_EPOCH_DAY,
};
use wiki_parser::graph::PathFinder;

/// The page itself and its share card ride along, so the out directory IS
/// the deploy — nothing else to gather. data-static on <html> is what flips
/// the page's fetch layer from /api to the file tree.
const PAGE: &str = include_str!("../../static/index.html");
const OG: &[u8] = include_bytes!("../../static/og.jpg");

/// Articles per shard. ~880 keeps a shard around 250 KB once the CDN
/// compresses it — one link-list click, one fetch.
const SHARD_SIZE: usize = 896;
const SEARCH_TOP: usize = 50;

#[derive(Parser, Debug)]
#[command(name = "static_export", about = "Emit the WikiGolf static site tree")]
struct Args {
    #[arg(short, long, default_value = "data")]
    data: PathBuf,

    #[arg(short, long, default_value = "static_site")]
    out: PathBuf,

    /// Future dailies/rounds to pre-generate. The site goes stale after
    /// this many days without a re-export — size it to your republish habit.
    #[arg(long, default_value_t = 45)]
    days: u64,

    /// Also emit the archive back to daily #1.
    #[arg(long, default_value_t = true)]
    archive: bool,
}

fn main() -> Result<()> {
    let a = Args::parse();
    let t0 = std::time::Instant::now();
    eprintln!("loading game state from {}...", a.data.display());
    let game = Game::load(&a.data)?;
    let g = &game.graph;
    let n = g.len();
    let mut pf = PathFinder::new(n);

    fs::create_dir_all(a.out.join("shards"))?;
    fs::create_dir_all(a.out.join("search"))?;
    fs::create_dir_all(a.out.join("daily"))?;

    // ---- the page, the card, meta / regions / landmarks / map -------------
    fs::write(
        a.out.join("index.html"),
        PAGE.replacen("<html lang=\"en\">", "<html lang=\"en\" data-static>", 1),
    )?;
    fs::write(a.out.join("og.jpg"), OG)?;

    let today = today_day();
    let today_number = today.saturating_sub(DAILY_EPOCH_DAY) + 1;
    let bounds = game.layout.as_ref().map(|l| {
        let fold = |v: &Vec<f32>, init: f32, f: fn(f32, f32) -> f32| {
            v.iter().copied().filter(|x| x.is_finite()).fold(init, f)
        };
        json!([
            fold(&l.x, f32::MAX, f32::min), fold(&l.y, f32::MAX, f32::min),
            fold(&l.x, f32::MIN, f32::max), fold(&l.y, f32::MIN, f32::max),
        ])
    });
    write_json(
        &a.out.join("meta.json"),
        &json!({
            "articles": n,
            "edges": g.forward.edges(),
            "shard_size": SHARD_SIZE,
            "n_shards": n.div_ceil(SHARD_SIZE),
            "day0": DAILY_EPOCH_DAY,
            "today_number": today_number,
            "days_ahead": a.days,
            "has_map": game.layout.is_some(),
            "bounds": bounds,
            "views": !game.views.is_empty(),
            "pools": game.has_pools(),
            "static": true,
        }),
    )?;
    write_json(&a.out.join("regions.json"), &json!(game.region_names))?;
    write_json(
        &a.out.join("landmarks.json"),
        &json!(game
            .landmarks(45)
            .into_iter()
            .map(|(t, x, y, c)| json!({"title": t, "x": x, "y": y, "c": c}))
            .collect::<Vec<_>>()),
    )?;
    // Columnar, exactly the live /api/map shape — the page draws it as-is.
    {
        let pts = game.map_sample(45_000);
        let (mut xs, mut ys, mut cs) = (Vec::new(), Vec::new(), Vec::new());
        for (x, y, c) in pts {
            xs.push(x);
            ys.push(y);
            cs.push(c);
        }
        write_json(&a.out.join("map.json"), &json!({"x": xs, "y": ys, "c": cs}))?;
    }
    eprintln!("  page + meta + map written");

    // ---- article shards ---------------------------------------------------
    let coord = |id: u32| -> (Option<f32>, Option<f32>, Option<i32>) {
        match game.coords(id) {
            Some((x, y, c)) => (Some(x), Some(y), Some(c)),
            None => (None, None, None),
        }
    };
    let mut shard_bytes = 0u64;
    let n_shards = n.div_ceil(SHARD_SIZE);
    for s in 0..n_shards {
        let lo = s * SHARD_SIZE;
        let hi = ((s + 1) * SHARD_SIZE).min(n);
        let mut entries = Vec::with_capacity(hi - lo);
        for id in lo as u32..hi as u32 {
            let (x, y, c) = coord(id);
            let links: Vec<_> = g
                .forward
                .neighbors(id)
                .iter()
                .map(|&w| json!([w, g.title(w), g.reverse.degree(w)]))
                .collect();
            entries.push(json!([
                id,
                g.title(id),
                game.descs.get(id).first(),
                game.kinds.get(id).first(),
                game.flags.get(id as usize).copied().unwrap_or(0),
                game.views.get(id as usize).copied().unwrap_or(0),
                c,
                x,
                y,
                g.reverse.degree(id),
                links,
            ]));
        }
        shard_bytes += write_json(&a.out.join(format!("shards/{s}.json")), &json!(entries))?;
        if s % 500 == 0 {
            eprintln!("  shard {s}/{n_shards}…");
        }
    }
    eprintln!("  {n_shards} shards, {:.2} GB raw", shard_bytes as f64 / 1e9);

    // ---- search prefix buckets --------------------------------------------
    // Two-char prefixes over Unicode titles produce an unbounded key space,
    // and Cloudflare Pages caps a deploy at 20k files — so prefixes hash
    // into a fixed 4096 buckets, each holding a map of prefix -> top titles.
    // The client computes the same (c1*31+c2)%4096 and looks its prefix up
    // inside the fetched file.
    let mut buckets: HashMap<u64, HashMap<String, Vec<(usize, u32)>>> = HashMap::new();
    for id in 0..n as u32 {
        let t = g.title(id);
        let mut chars = t.chars().flat_map(|c| c.to_lowercase());
        let (Some(c1), c2) = (chars.next(), chars.next()) else { continue };
        let c2v = c2.map_or(0, |c| c as u64);
        let file = (c1 as u64 * 31 + c2v) % 4096;
        let prefix = match c2 {
            Some(c2) => format!("{c1}{c2}"),
            None => c1.to_string(),
        };
        let v = buckets.entry(file).or_default().entry(prefix).or_default();
        v.push((g.reverse.degree(id), id));
        // Keep lists bounded as we go; exact ordering is finalized below.
        if v.len() > SEARCH_TOP * 4 {
            v.sort_unstable_by(|x, y| y.0.cmp(&x.0));
            v.truncate(SEARCH_TOP);
        }
    }
    let n_files = buckets.len();
    let mut n_prefixes = 0usize;
    for (file, prefixes) in buckets {
        let mut obj = serde_json::Map::new();
        n_prefixes += prefixes.len();
        for (prefix, mut v) in prefixes {
            v.sort_unstable_by(|x, y| y.0.cmp(&x.0));
            v.truncate(SEARCH_TOP);
            let rows: Vec<_> = v
                .into_iter()
                .map(|(deg, id)| json!([id, g.title(id), deg]))
                .collect();
            obj.insert(prefix, json!(rows));
        }
        write_json(&a.out.join(format!("search/{file}.json")), &json!(obj))?;
    }
    eprintln!("  {n_prefixes} search prefixes in {n_files} bucket files");

    // ---- random race pools ------------------------------------------------
    // 1,500 pre-drawn races per difficulty, par and routes attached, so the
    // static build's Random button works. Deterministic seed: the file is
    // reproducible from the same pools.
    if game.has_pools() {
        fs::create_dir_all(a.out.join("random"))?;
        for d in [Difficulty::Easy, Difficulty::Medium, Difficulty::Hard] {
            let mut rng = Rng::new(0x57A7_1C00 ^ d as u64);
            let ban = d.rules().0;
            let mut rows = Vec::with_capacity(1500);
            for _ in 0..1500 {
                if let Some((src, dst, par, routes)) = game.pools_pick_full(d, &mut rng) {
                    rows.push(json!([
                        src, g.title(src), dst, g.title(dst), par, routes
                    ]));
                }
            }
            write_json(
                &a.out.join(format!("random/{}.json", format!("{d:?}").to_lowercase())),
                &json!({"ban_degree": ban, "races": rows}),
            )?;
        }
        eprintln!("  random race files written");
    }

    // ---- dailies + rounds, archive and future -----------------------------
    let first = if a.archive { 1 } else { today_number };
    let mut n_daily = 0u64;
    for number in first..=today_number + a.days {
        let day = DAILY_EPOCH_DAY + number - 1;
        let mut difficulties = serde_json::Map::new();
        for d in [Difficulty::Easy, Difficulty::Medium, Difficulty::Hard] {
            let name = format!("{d:?}").to_lowercase();
            if let Some(p) = game.seeded_puzzle(&mut pf, d, daily_seed(day, d)) {
                difficulties.insert(name, puzzle_json(&game, &mut pf, &p));
            }
        }
        let mut rng = Rng::new(course_seed(day, COURSE_3.len()));
        let mut holes = Vec::with_capacity(COURSE_3.len());
        for &par in &COURSE_3 {
            if let Some(p) = game.course_hole(&mut pf, par, &mut rng) {
                holes.push(puzzle_json(&game, &mut pf, &p));
            }
        }
        let round_par: u64 = holes.iter().filter_map(|h| h["par"].as_u64()).sum();
        write_json(
            &a.out.join(format!("daily/{number}.json")),
            &json!({
                "number": number,
                "difficulties": difficulties,
                "round": { "par": round_par, "holes": holes },
            }),
        )?;
        n_daily += 1;
    }
    eprintln!(
        "  {n_daily} daily files (archive from #{first}, {} days ahead)",
        a.days
    );
    eprintln!("done in {:.1?} → {}", t0.elapsed(), a.out.display());
    Ok(())
}

fn puzzle_json(
    game: &Game,
    pf: &mut PathFinder,
    p: &wiki_parser::game::Puzzle,
) -> serde_json::Value {
    let banned = |_: u32| false; // dailies and holes are generated unbanned here
    let routes = match p.ban_degree {
        // A banned daily (medium/hard) needs the ban honoured in the count.
        Some(limit) => {
            let g = &game.graph;
            let rev = &g.reverse;
            let b = move |v: u32| rev.degree(v) > limit;
            pf.count_shortest_paths(g, p.start, p.goal, &b, p.optimal as u8 + 1)
        }
        None => pf.count_shortest_paths(&game.graph, p.start, p.goal, &banned, p.optimal as u8 + 1),
    }
    .map(|(_, c)| c)
    .unwrap_or(0);
    let art = |id: u32| {
        json!({
            "id": id,
            "title": game.graph.title(id),
            "desc": game.descs.get(id).first(),
            "in_degree": game.graph.reverse.degree(id),
        })
    };
    json!({
        "start": art(p.start),
        "goal": art(p.goal),
        "par": p.optimal,
        "ban_degree": p.ban_degree,
        "routes": routes,
    })
}

fn write_json(path: &std::path::Path, v: &serde_json::Value) -> Result<u64> {
    let mut f = std::io::BufWriter::new(
        fs::File::create(path).with_context(|| format!("create {}", path.display()))?,
    );
    serde_json::to_writer(&mut f, v)?;
    f.flush()?;
    Ok(fs::metadata(path)?.len())
}
