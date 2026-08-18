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
//!   shards/{i}.json           {a: articles [i*S,(i+1)*S), d: target dict}
//!                             — links denormalized, one fetch per click;
//!                             d carries desc+flags per distinct target
//!   search/{a}-{b}.json       top-50 titles per two-char lowercase prefix
//!   daily/{n}.json            per-difficulty dailies + the 3-hole round,
//!                             pars and route counts baked in
//!   compass/{n}.json          compass-lite: per goal, complete BFS levels
//!                             of the ~250k nearest articles, delta-encoded

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

#[derive(Parser, Debug)]
#[command(name = "static_export", about = "Emit the WikiGolf static site tree")]
struct Args {
    #[arg(short, long, default_value = "data")]
    data: PathBuf,

    #[arg(short, long, default_value = "static_site")]
    out: PathBuf,

    /// Future dailies/rounds to pre-generate. Each daily file costs ~2 KB,
    /// so a whole year is ~700 KB — the default makes staleness an annual
    /// concern, not a monthly one. Past the horizon the page tells visitors
    /// plainly that the build needs a re-export.
    #[arg(long, default_value_t = 365)]
    days: u64,

    /// Also emit the archive back to daily #1.
    #[arg(long, default_value_t = true)]
    archive: bool,

    /// Pre-rolled random races per difficulty. Every race's goal also gets a
    /// compass file (~1.3 MB raw each at enwiki, deduped across pools), which
    /// is what actually prices this dial: 800 races/difficulty is ~2-3 GB of
    /// compass tree and still months of no-repeat feel for a heavy player.
    #[arg(long, default_value_t = 800)]
    random_per_diff: usize,

    /// Days ahead (past dailies always included) that get compass-lite
    /// files: per goal, the ~250k articles nearest the goal as complete BFS
    /// levels, delta-encoded. ~1-4 MB per day for its six goals. 0 disables.
    /// A full decade would double the tree, which is why this window is a
    /// year while the dailies run ten — re-exports refresh it.
    #[arg(long, default_value_t = 365)]
    compass_days: u64,
}

/// Budget per compass goal, in article ids. At enwiki fan-out a 50k cap
/// left typical dailies at depth 1 — "1 to go" and nothing else — because
/// level 2 alone often exceeds it. 250k restores depth 2-3 on most goals
/// for ~300 KB gzipped per press, a price only paid on the press.
const COMPASS_CAP: usize = 250_000;
const COMPASS_DEPTH: u8 = 6;

/// Sorted ids, delta-encoded: first id absolute, the rest gaps. Halves the
/// JSON against absolute ids at this density.
fn delta_encode(mut ids: Vec<u32>) -> Vec<u32> {
    ids.sort_unstable();
    let mut prev = 0u32;
    for x in ids.iter_mut() {
        let v = *x;
        *x = v - prev;
        prev = v;
    }
    ids
}

/// Truncate on a char boundary — the extras.rs lesson, not repeated.
fn trunc(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
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
            "compass_days": a.compass_days,
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
        // One dictionary per shard for what the link rows need about their
        // targets — description and flags — deduped across the shard's
        // sources. Denormalizing onto every link row was measured at nearly
        // double the tree; the dict pays each target once per shard.
        let mut dict: std::collections::BTreeMap<u32, serde_json::Value> = Default::default();
        for id in lo as u32..hi as u32 {
            let (x, y, c) = coord(id);
            let links: Vec<_> = g
                .forward
                .neighbors(id)
                .iter()
                .map(|&w| {
                    if !dict.contains_key(&w) {
                        let desc = game.descs.get(w).first().map(|d| trunc(d, 72));
                        let flags = game.flags.get(w as usize).copied().unwrap_or(0);
                        if desc.is_some() || flags != 0 {
                            dict.insert(w, json!([desc, flags]));
                        }
                    }
                    json!([w, g.title(w), g.reverse.degree(w)])
                })
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
        let dict: serde_json::Map<String, serde_json::Value> =
            dict.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        shard_bytes += write_json(
            &a.out.join(format!("shards/{s}.json")),
            &json!({"a": entries, "d": dict}),
        )?;
        if s % 500 == 0 {
            eprintln!("  shard {s}/{n_shards}…");
        }
    }
    eprintln!("  {n_shards} shards, {:.2} GB raw", shard_bytes as f64 / 1e9);

    // No search buckets: the jump search bar is hidden on static (its only
    // function was mid-race teleporting, which voids the score and confused
    // playtesters), so the 4096-bucket prefix index it queried — ~2,400
    // files against Pages' 20k-file deploy cap — is no longer written.

    // ---- random race pools ------------------------------------------------
    // Pre-drawn races per difficulty, par and routes attached, so the static
    // build's Random button works. Deterministic seed: the file is
    // reproducible from the same pools. Start/goal are full article objects
    // (not id/title tuples) — the goal card and the map's red dot need
    // desc, in-degree, views and x/y, exactly like a daily's.
    //
    // Every pool race also gets a per-goal compass file (compass/g{id}.json,
    // deduped across pools), which is why the default pool is 800 rather
    // than 5,000: all-compassed beats a lottery where 10% of Random presses
    // have a working compass and the rest silently don't.
    if game.has_pools() {
        fs::create_dir_all(a.out.join("random"))?;
        let mut pool_goals: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        let with_compass = a.compass_days > 0;
        for d in [Difficulty::Easy, Difficulty::Medium, Difficulty::Hard] {
            let mut rng = Rng::new(0x57A7_1C00 ^ d as u64);
            let ban = d.rules().0;
            let mut rows = Vec::with_capacity(a.random_per_diff);
            for _ in 0..a.random_per_diff {
                if let Some((src, dst, par, routes)) = game.pools_pick_full(d, &mut rng) {
                    pool_goals.insert(dst);
                    rows.push(json!({
                        "start": art_json(&game, src),
                        "goal": art_json(&game, dst),
                        "par": par,
                        "routes": routes,
                        "route": route_json(&game, &mut pf, src, dst, ban),
                    }));
                }
            }
            write_json(
                &a.out.join(format!("random/{}.json", format!("{d:?}").to_lowercase())),
                &json!({"ban_degree": ban, "compass": with_compass, "races": rows}),
            )?;
        }
        if with_compass {
            fs::create_dir_all(a.out.join("compass"))?;
            let mut bytes = 0u64;
            let n_goals = pool_goals.len();
            for goal in pool_goals {
                let levels =
                    wiki_parser::graph::near_goal_levels(g, goal, COMPASS_DEPTH, COMPASS_CAP);
                let d = levels.len();
                let l: Vec<_> =
                    levels.into_iter().map(|lvl| json!(delta_encode(lvl))).collect();
                bytes += write_json(
                    &a.out.join(format!("compass/g{goal}.json")),
                    &json!({"d": d, "l": l}),
                )?;
            }
            eprintln!(
                "  random race files written; {n_goals} pool-goal compass files, {:.2} GB raw",
                bytes as f64 / 1e9
            );
        } else {
            eprintln!("  random race files written (no compass: --compass-days 0)");
        }
    }

    // ---- dailies + rounds, archive and future -----------------------------
    let first = if a.archive { 1 } else { today_number };
    let compass_last = if a.compass_days > 0 { today_number + a.compass_days } else { 0 };
    if compass_last >= first {
        fs::create_dir_all(a.out.join("compass"))?;
    }
    let mut n_daily = 0u64;
    let (mut n_compass, mut compass_bytes) = (0u64, 0u64);
    for number in first..=today_number + a.days {
        let day = DAILY_EPOCH_DAY + number - 1;
        // The six goals a day owns: one per difficulty, one per hole. Their
        // compass files are keyed the same way the client asks for them.
        let mut goals: Vec<(String, u32)> = Vec::with_capacity(6);
        let mut difficulties = serde_json::Map::new();
        for d in [Difficulty::Easy, Difficulty::Medium, Difficulty::Hard] {
            let name = format!("{d:?}").to_lowercase();
            if let Some(p) = game.seeded_puzzle(&mut pf, d, daily_seed(day, d)) {
                goals.push((name.clone(), p.goal));
                difficulties.insert(name, puzzle_json(&game, &mut pf, &p));
            }
        }
        let mut rng = Rng::new(course_seed(day, COURSE_3.len()));
        let mut holes = Vec::with_capacity(COURSE_3.len());
        for (i, &par) in COURSE_3.iter().enumerate() {
            if let Some(p) = game.course_hole(&mut pf, par, &mut rng) {
                goals.push((format!("h{}", i + 1), p.goal));
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
        if number <= compass_last {
            let mut obj = serde_json::Map::new();
            for (key, goal) in goals {
                let levels =
                    wiki_parser::graph::near_goal_levels(g, goal, COMPASS_DEPTH, COMPASS_CAP);
                let d = levels.len();
                let l: Vec<_> =
                    levels.into_iter().map(|lvl| json!(delta_encode(lvl))).collect();
                obj.insert(key, json!({"d": d, "l": l}));
            }
            compass_bytes +=
                write_json(&a.out.join(format!("compass/{number}.json")), &json!(obj))?;
            n_compass += 1;
        }
    }
    eprintln!(
        "  {n_daily} daily files (archive from #{first}, {} days ahead)",
        a.days
    );
    if n_compass > 0 {
        eprintln!(
            "  {n_compass} compass files, {:.2} GB raw",
            compass_bytes as f64 / 1e9
        );
    }
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
    let art = |id: u32| art_json(game, id);
    json!({
        "start": art(p.start),
        "goal": art(p.goal),
        "par": p.optimal,
        "ban_degree": p.ban_degree,
        "routes": routes,
        "route": route_json(game, pf, p.start, p.goal, p.ban_degree),
    })
}

/// One shortest route, baked in at export: [[id, title, x, y], ...]. The
/// live server computes this on demand post-race; a static build has no
/// pathfinder, and reconstructing from the truncated compass levels only
/// reached ~1/3 of races (depth 2-3 vs pars up to 6). A few hundred bytes
/// per race buys 100% coverage. It sits in a file the client fetches at
/// race start — "hidden" was never on the table for a static site, and
/// there is no posted leaderboard to protect.
fn route_json(
    game: &Game,
    pf: &mut PathFinder,
    start: u32,
    goal: u32,
    ban: Option<usize>,
) -> serde_json::Value {
    let path = match ban {
        Some(limit) => {
            let rev = &game.graph.reverse;
            let b = move |v: u32| rev.degree(v) > limit;
            pf.shortest_path(&game.graph, start, goal, &b)
        }
        None => pf.shortest_path(&game.graph, start, goal, &|_| false),
    }
    .unwrap_or_default();
    let nodes: Vec<_> = path
        .iter()
        .map(|&id| {
            let (x, y) = match game.coords(id) {
                Some((x, y, _)) => (Some(x), Some(y)),
                None => (None, None),
            };
            json!([id, game.graph.title(id), x, y])
        })
        .collect();
    json!(nodes)
}

/// A puzzle endpoint's article object, matching the fields the client reads
/// from the live server's ArticleRef: the goal card shows desc, in-degree and
/// reads/mo, and the map's red goal dot needs x/y (null off-layout articles
/// simply draw no dot, same as live).
fn art_json(game: &Game, id: u32) -> serde_json::Value {
    let (x, y) = match game.coords(id) {
        Some((x, y, _)) => (Some(x), Some(y)),
        None => (None, None),
    };
    json!({
        "id": id,
        "title": game.graph.title(id),
        "desc": game.descs.get(id).first(),
        "in_degree": game.graph.reverse.degree(id),
        "views": game.views.get(id as usize),
        "x": x,
        "y": y,
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
