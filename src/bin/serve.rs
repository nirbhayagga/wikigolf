//! serve — the wiki-race HTTP service.
//!
//!   serve --data data/simple --port 8080
//!
//! Holds the whole graph in memory and answers path queries from it. Every
//! response is derived from the parser's Parquet; nothing is fetched from
//! Wikipedia at request time, so the game is self-consistent: the optimal path
//! we report is optimal *in the world the player is playing in*.

use anyhow::{Context, Result};
use axum::extract::{ConnectInfo, Path as AxPath, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use wiki_parser::game::{Difficulty, Game, Rng};
use wiki_parser::graph::PathFinder;
use wiki_parser::identity::Identity;
use wiki_parser::ratelimit::RateLimiter;
use wiki_parser::runs::{Registry, RunSpec};

#[derive(Parser, Debug)]
#[command(name = "serve", about = "WikiGolf HTTP service")]
struct Args {
    /// Directory holding titles.parquet / edges.parquet (and optionally nodes.parquet)
    #[arg(short, long, default_value = "data")]
    data: PathBuf,

    /// Where to keep writable state: the leaderboard log and the HMAC secret
    /// backing the identity cookie. Defaults to --data, which is right for
    /// local use but wrong for a deployment: the parquet is large, immutable,
    /// and normally mounted read-only, and the process refuses to start if it
    /// cannot open the leaderboard there.
    #[arg(long)]
    state: Option<PathBuf>,

    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Read the client IP from X-Forwarded-For. Only enable behind a reverse
    /// proxy you control — a client can otherwise forge the header and defeat
    /// per-IP rate limiting entirely.
    #[arg(long)]
    trust_proxy: bool,

    /// Mark the identity cookie Secure. Enable when served over HTTPS; a
    /// Secure cookie is never sent over plain http, so setting it during
    /// local development silently breaks identity.
    #[arg(long)]
    secure_cookies: bool,

    /// Leave redirect aliases out of the search index, saving ~450 MB of
    /// resident memory (measured total drops from ~7.1 GB to ~6.6 GB at
    /// enwiki scale). Search still covers every title; "NYC" just stops
    /// finding New York City. For squeezing onto an 8 GB box.
    #[arg(long)]
    no_alias_search: bool,
}

/// Pathfinder scratch space is ~72 MB per instance at enwiki scale, so it is
/// pooled rather than allocated per request. Searches are CPU-bound too, so
/// every handler that touches the graph runs on the blocking pool — holding an
/// async worker for 40 ms would stall unrelated requests.
struct App {
    game: Game,
    finders: Mutex<Vec<PathFinder>>,
    runs: Registry,
    /// Cheap reads: article pages, map tiles, meta.
    rl_read: RateLimiter,
    /// Everything that costs real CPU — a title scan or a BFS — plus anything
    /// that mutates the leaderboard.
    rl_heavy: RateLimiter,
    trust_proxy: bool,
    identity: Identity,
    secure_cookies: bool,
    /// Usage counters, flushed hourly to analytics.jsonl in the state dir.
    analytics: Analytics,
    /// Distance-to-goal maps, keyed by goal. One is ~7 MB at enwiki scale and
    /// costs a full reverse BFS, so they are kept rather than recomputed —
    /// and everyone racing the daily shares a single entry.
    compass: Mutex<Vec<(u32, Arc<Vec<u8>>)>>,
}

/// How many goal maps to keep. Each is one byte per article, so this bounds
/// the cache at roughly 8 x 7 MB.
const COMPASS_CACHE: usize = 8;
/// Nothing in this game is a sensible race at more than a handful of clicks,
/// and the cap is what stops a BFS from walking the entire graph.
const COMPASS_DEPTH: u8 = 6;

/// The player id resolved from a request's cookie, attached by middleware.
#[derive(Clone)]
struct Player(String);

/// Best-effort usage counters — the number you need to know before deciding
/// whether hosting is worth paying for. One JSON line per hour is appended to
/// <state>/analytics.jsonl with the deltas since the previous line, plus the
/// count of distinct client IPs seen so far that UTC day (hashed before
/// storing; the raw address is never kept). Lost lines on a crash are
/// accepted — this is capacity planning, not accounting.
#[derive(Default)]
struct Analytics {
    pages: std::sync::atomic::AtomicU64,
    searches: std::sync::atomic::AtomicU64,
    articles: std::sync::atomic::AtomicU64,
    puzzles: std::sync::atomic::AtomicU64,
    submits: std::sync::atomic::AtomicU64,
    compass: std::sync::atomic::AtomicU64,
    other: std::sync::atomic::AtomicU64,
    /// (unix day number, hashed IPs seen that day)
    uniques: Mutex<(u64, std::collections::HashSet<u64>)>,
}

impl Analytics {
    fn hit(&self, path: &str, ip: IpAddr) {
        use std::sync::atomic::Ordering::Relaxed;
        match path {
            "/" => &self.pages,
            "/api/search" => &self.searches,
            p if p.starts_with("/api/article/") => &self.articles,
            "/api/puzzle" | "/api/daily" => &self.puzzles,
            "/api/submit" => &self.submits,
            "/api/compass" => &self.compass,
            _ => &self.other,
        }
        .fetch_add(1, Relaxed);

        let day = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() / 86_400)
            .unwrap_or(0);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&ip, &mut h);
        let mut u = self.uniques.lock().unwrap();
        if u.0 != day {
            *u = (day, Default::default());
        }
        u.1.insert(std::hash::Hasher::finish(&h));
    }

    /// Snapshot-and-reset the counters; uniques stay (they are a running
    /// per-day count, not a delta).
    fn flush_line(&self) -> Option<String> {
        use std::sync::atomic::Ordering::Relaxed;
        let take = |a: &std::sync::atomic::AtomicU64| a.swap(0, Relaxed);
        let (pages, searches, articles, puzzles, submits, compass, other) = (
            take(&self.pages),
            take(&self.searches),
            take(&self.articles),
            take(&self.puzzles),
            take(&self.submits),
            take(&self.compass),
            take(&self.other),
        );
        let (day, n_unique) = {
            let u = self.uniques.lock().unwrap();
            (u.0, u.1.len())
        };
        if pages + searches + articles + puzzles + submits + compass + other == 0 {
            return None;
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Some(
            serde_json::json!({
                "ts": ts, "day": day, "pages": pages, "searches": searches,
                "articles": articles, "puzzles": puzzles, "submits": submits,
                "compass": compass, "other": other, "uniques_today": n_unique,
            })
            .to_string(),
        )
    }
}

impl App {
    fn with_finder<T>(&self, f: impl FnOnce(&Game, &mut PathFinder) -> T) -> T {
        let mut pf = self
            .finders
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| PathFinder::new(self.game.graph.len()));
        let out = f(&self.game, &mut pf);
        let mut pool = self.finders.lock().unwrap();
        if pool.len() < 4 {
            pool.push(pf);
        }
        out
    }
}

type Shared = Arc<App>;

/// Resolve the client address for rate-limiting purposes.
///
/// X-Forwarded-For is client-controlled unless a proxy is guaranteed to
/// overwrite it, so it is consulted only when explicitly trusted. The
/// left-most entry is the original client.
fn client_ip(app: &App, req: &Request, peer: IpAddr) -> IpAddr {
    if app.trust_proxy {
        if let Some(fwd) = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(first) = fwd.split(',').next() {
                if let Ok(ip) = first.trim().parse::<IpAddr>() {
                    return ip;
                }
            }
        }
    }
    peer
}

/// Resolve or mint the player identity, then hand it to the handlers.
///
/// A cookie that fails verification is treated as absent and replaced, so a
/// tampered or stale value cannot impersonate anyone — it just becomes a new
/// anonymous player.
async fn identify(State(app): State<Shared>, mut req: Request, next: Next) -> Response {
    let existing = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(Identity::from_header)
        .and_then(|c| app.identity.verify(c));

    let (player, issue) = match existing {
        Some(id) => (id, None),
        None => match app.identity.issue() {
            Ok(cookie) => {
                let id = app.identity.verify(&cookie).unwrap_or_default();
                (id, Some(cookie))
            }
            Err(_) => (String::new(), None),
        },
    };

    req.extensions_mut().insert(Player(player));
    let mut res = next.run(req).await;
    if let Some(cookie) = issue {
        if let Ok(v) = Identity::cookie_header(&cookie, app.secure_cookies).parse() {
            res.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    res
}

async fn rate_limit(
    State(app): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    let ip = client_ip(&app, &req, peer.ip());
    let heavy = matches!(
        path.as_str(),
        "/api/search"
            | "/api/path"
            | "/api/puzzle"
            | "/api/daily"
            | "/api/course"
            | "/api/submit"
            | "/api/compass"
            | "/api/routes"
    );
    let ok = if heavy { app.rl_heavy.allow(ip) } else { app.rl_read.allow(ip) };
    if ok {
        // Count what was served, not what was throttled — the question these
        // counters answer is "how much real use is there".
        app.analytics.hit(&path, ip);
    }
    if !ok {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "5")],
            Json(serde_json::json!({ "error": "rate limited, slow down" })),
        )
            .into_response();
    }
    next.run(req).await
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ArticleRef {
    id: u32,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    x: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    y: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    community: Option<i32>,
    in_degree: usize,
    /// Set on link lists when a hub ban is in force. The link is still shown —
    /// hiding "United States" from "Bank of America" makes the page look
    /// broken rather than constrained — but it cannot be taken, which is what
    /// keeps par honest.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    banned: bool,
    /// What the article is about, from its own [[Category:...]] links. This is
    /// the context a bare link list otherwise throws away — in a real article
    /// the surrounding sentence tells you what a link is, and here nothing
    /// does. Empty for a parse that predates category extraction.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cats: Vec<String>,
    /// Monthly pageviews — what people read, where in_degree is what editors
    /// link. Absent until 09_pageviews.py has produced pageviews.parquet.
    #[serde(skip_serializing_if = "Option::is_none")]
    views: Option<u32>,
    /// {{Short description}} — the context line a bare link list lacks.
    #[serde(skip_serializing_if = "Option::is_none")]
    desc: Option<String>,
    /// First infobox kind ("person", "film"), for the type glyph.
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    /// Editor-vetted quality ({{Featured article}} / {{Good article}}).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    featured: bool,
    /// A fork, not a destination. Excluded from generated endpoints; tagged
    /// in the link list so the player knows what they are clicking into.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    disambig: bool,
}

fn article_ref(g: &Game, id: u32) -> ArticleRef {
    article_ref_banned(g, id, false)
}

fn article_ref_banned(g: &Game, id: u32, banned: bool) -> ArticleRef {
    let (x, y, community) = match g.coords(id) {
        Some((x, y, c)) => (Some(x), Some(y), Some(c)),
        None => (None, None, None),
    };
    ArticleRef {
        id,
        title: g.graph.title(id).to_string(),
        cats: g.categories.get(id).to_vec(),
        x,
        y,
        community,
        in_degree: g.graph.reverse.degree(id),
        banned,
        views: g.views.get(id as usize).copied(),
        desc: g.descs.get(id).first().cloned(),
        kind: g.kinds.get(id).first().cloned(),
        featured: g
            .flags
            .get(id as usize)
            .is_some_and(|f| f & (wiki_parser::extras::FLAG_FEATURED | wiki_parser::extras::FLAG_GOOD) != 0),
        disambig: g
            .flags
            .get(id as usize)
            .is_some_and(|f| f & wiki_parser::extras::FLAG_DISAMBIG != 0),
    }
}

#[derive(Serialize)]
struct Meta {
    articles: usize,
    edges: usize,
    has_map: bool,
    /// Whether puzzle draws come from the precomputed pools (pools.parquet)
    /// rather than rejection sampling — i.e. whether par-5+ races and the
    /// route-count difficulty split are available.
    pools: bool,
    /// Whether monthly pageview counts are loaded (pageviews.parquet).
    views: bool,
    bounds: Option<[f32; 4]>,
}

#[derive(Serialize)]
struct ArticleDetail {
    #[serde(flatten)]
    article: ArticleRef,
    links: Vec<ArticleRef>,
    /// Redirect titles pointing here — "also known as". Already in memory for
    /// search; this just stops throwing it away at the boundary.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    aliases: Vec<String>,
    /// Wikitext bytes. A rough "is this a stub or a monster" signal, and the
    /// dump gives it away for free.
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u32>,
}

#[derive(Deserialize)]
struct PathRequest {
    from: u32,
    to: u32,
    ban_degree: Option<usize>,
}

#[derive(Serialize)]
struct PathResponse {
    found: bool,
    clicks: usize,
    path: Vec<ArticleRef>,
}

#[derive(Serialize)]
struct PuzzleResponse {
    /// Compass charges this race gets, scaled to its par.
    compass: u8,
    start: ArticleRef,
    goal: ArticleRef,
    optimal: usize,
    ban_degree: Option<usize>,
    difficulty: String,
    /// Present only for the daily challenge: the puzzle's sequence number.
    #[serde(skip_serializing_if = "Option::is_none")]
    number: Option<u64>,
    /// Server-issued handle for this race. Scores are only accepted against
    /// one of these, so the clock and the puzzle terms are ours, not the
    /// client's.
    run: u64,
}

#[derive(Deserialize)]
struct SubmitRequest {
    run: u64,
    path: Vec<u32>,
    #[serde(default)]
    nickname: String,
}

#[derive(Serialize)]
struct SubmitResponse {
    accepted: bool,
    clicks: usize,
    par: usize,
    ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    rank: Option<usize>,
}

#[derive(Serialize)]
struct BoardRow {
    rank: usize,
    nickname: String,
    clicks: usize,
    ms: u64,
}

/// Day 0 of the daily challenge: 2026-01-01 UTC, as a Unix day index.
const DAILY_EPOCH_DAY: u64 = 20_454;

// Rate limits, per IP. Reads are generous because normal play issues one
// article fetch per click. Heavy endpoints each cost a full-title scan or a
// BFS, so they get a much smaller budget with a burst for ordinary bursts of
// typing in the search box.
const READ_BURST: f64 = 60.0;
const READ_PER_SEC: f64 = 10.0;
const HEAVY_BURST: f64 = 15.0;
const HEAVY_PER_SEC: f64 = 2.0;

/// Generate deterministically from a seed, retrying with a derived seed if a
/// seed happens to produce no qualifying pair. Determinism is the whole point
/// of the daily — everyone must get the same race — so the retry has to be a
/// pure function of the seed too, never of wall-clock or attempt timing.
fn puzzle_from_seed(
    app: &App,
    d: Difficulty,
    seed: u64,
    ban_top: Option<usize>,
) -> Option<wiki_parser::game::Puzzle> {
    app.with_finder(|g, pf| {
        // An explicit hub cut from the slider overrides the difficulty preset's
        // ban, but keeps its minimum route length — the two dials control
        // different things.
        let custom = ban_top.map(|n| g.hub_cut(n, 0).0);
        let mut s = seed;
        for _ in 0..6 {
            let mut rng = Rng::new(s);
            let found = match custom {
                Some(ban) => g.puzzle_with(pf, ban, 3, &mut rng),
                None => g.puzzle(pf, d, &mut rng),
            };
            if found.is_some() {
                return found;
            }
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        }
        None
    })
}

/// What a given hub-slider position actually excludes.
async fn hubs(
    State(s): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let n = q.get("n").and_then(|v| v.parse().ok()).unwrap_or(0usize);
    let (limit, names, excluded) = s.game.hub_cut(n, 12);
    Json(serde_json::json!({
        "ban_degree": limit,
        "excluded": excluded,
        "sample": names.iter().map(|&v| serde_json::json!({
            "title": s.game.graph.title(v),
            "in_degree": s.game.graph.reverse.degree(v),
        })).collect::<Vec<_>>(),
    }))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn err(msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg.into() })),
    )
}

/// Short-lived public caching so an edge proxy (Cloudflare in front of the
/// tunnel) serves the page instead of the home uplink. Five minutes is the
/// deploy-visibility price, paid knowingly: on an LTE connection the page
/// and the map are the two objects that matter, and with both edge-cached
/// the uplink carries only per-race JSON — a few KB/s per active player.
async fn index() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "public, max-age=300")],
        Html(include_str!("../../static/index.html")),
    )
}

/// Region names, so the link list can group by topic instead of showing a
/// flat wall of several hundred links. Sent once with the page rather than per
/// article: there are at most a few hundred and they never change while the
/// process runs. Empty until 03_name_clusters.py has been run, which the UI
/// handles by falling back to "Region N".
async fn regions(State(s): State<Shared>) -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "public, max-age=86400")],
        Json(s.game.region_names.clone()),
    )
        .into_response()
}

async fn meta(State(s): State<Shared>) -> Json<Meta> {
    let bounds = s.game.layout.as_ref().map(|l| {
        let fold = |v: &Vec<f32>, init: f32, f: fn(f32, f32) -> f32| {
            v.iter().fold(init, |a, &b| f(a, b))
        };
        [
            fold(&l.x, f32::MAX, f32::min),
            fold(&l.y, f32::MAX, f32::min),
            fold(&l.x, f32::MIN, f32::max),
            fold(&l.y, f32::MIN, f32::max),
        ]
    });
    Json(Meta {
        articles: s.game.graph.len(),
        edges: s.game.graph.forward.edges(),
        has_map: s.game.layout.is_some(),
        pools: s.game.has_pools(),
        views: !s.game.views.is_empty(),
        bounds,
    })
}

async fn search(
    State(s): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let query = q.get("q").cloned().unwrap_or_default();
    let limit = q
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10usize)
        .min(50);
    let out = tokio::task::spawn_blocking(move || {
        s.game
            .search(&query, limit)
            .into_iter()
            .map(|id| article_ref(&s.game, id))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    Json(out)
}

async fn article(
    State(s): State<Shared>,
    AxPath(id): AxPath<u32>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if id as usize >= s.game.graph.len() {
        return err("no such article id").into_response();
    }
    let ban: Option<usize> = q.get("ban_degree").and_then(|v| v.parse().ok());
    let goal: Option<u32> = q.get("goal").and_then(|v| v.parse().ok());
    let out = tokio::task::spawn_blocking(move || {
        let links: Vec<ArticleRef> = s
            .game
            .graph
            .forward
            .neighbors(id)
            .iter()
            .map(|&v| {
                // The goal itself is never banned, matching the pathfinder —
                // otherwise a high-degree goal would be unreachable.
                let blocked = ban.is_some_and(|limit| {
                    Some(v) != goal && s.game.graph.reverse.degree(v) > limit
                });
                article_ref_banned(&s.game, v, blocked)
            })
            .collect();
        ArticleDetail {
            article: article_ref(&s.game, id),
            links,
            aliases: s.game.aliases.get(id).to_vec(),
            bytes: s.game.sizes.get(id as usize).copied().filter(|&b| b > 0),
        }
    })
    .await;
    match out {
        Ok(detail) => Json(detail).into_response(),
        Err(_) => err("article lookup failed").into_response(),
    }
}

async fn path(State(s): State<Shared>, Json(req): Json<PathRequest>) -> impl IntoResponse {
    let n = s.game.graph.len();
    if req.from as usize >= n || req.to as usize >= n {
        return err("article id out of range").into_response();
    }
    let out = tokio::task::spawn_blocking(move || {
        s.with_finder(|g, pf| {
            let rev = &g.graph.reverse;
            let banned: Box<dyn Fn(u32) -> bool + '_> = match req.ban_degree {
                Some(limit) => Box::new(move |v: u32| rev.degree(v) > limit),
                None => Box::new(|_| false),
            };
            match pf.shortest_path(&g.graph, req.from, req.to, &banned) {
                Some(p) => PathResponse {
                    found: true,
                    clicks: p.len() - 1,
                    path: p.into_iter().map(|v| article_ref(g, v)).collect(),
                },
                None => PathResponse { found: false, clicks: 0, path: Vec::new() },
            }
        })
    })
    .await;
    match out {
        Ok(resp) => Json(resp).into_response(),
        Err(_) => err("path search failed").into_response(),
    }
}

async fn puzzle(
    State(s): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let name = q.get("difficulty").cloned().unwrap_or_else(|| "easy".into());
    let d = Difficulty::parse(&name);

    // Player-chosen endpoints. Both must be given; one alone is ambiguous
    // enough that failing loudly beats guessing which half to generate.
    //
    // Titles get their own parameter names rather than being accepted by
    // `from`/`to`. Overloading one parameter would be genuinely ambiguous:
    // articles are titled "1941", so a bare `from=1941` could mean the year or
    // the article with that id, and guessing wrong sends the player somewhere
    // plausible and wrong. Titles are what share links carry, since they
    // survive a dump refresh and dense ids do not.
    let by_title = |key: &str| {
        q.get(key)
            .map(|t| s.game.graph.resolve(t).ok_or_else(|| t.clone()))
    };
    let (from_t, to_t) = (by_title("from_title"), by_title("to_title"));
    for r in [&from_t, &to_t] {
        if let Some(Err(name)) = r {
            return err(format!("no article called \"{name}\"")).into_response();
        }
    }
    let from = from_t
        .and_then(|r| r.ok())
        .or_else(|| q.get("from").and_then(|v| v.parse::<u32>().ok()));
    let to = to_t
        .and_then(|r| r.ok())
        .or_else(|| q.get("to").and_then(|v| v.parse::<u32>().ok()));
    if from.is_some() || to.is_some() {
        let (Some(a), Some(b)) = (from, to) else {
            return err("a custom race needs both from and to").into_response();
        };
        // Distinguished from the no-route case below: puzzle_between rejects
        // both, but "no route" is actively misleading for a == b.
        if a == b {
            return err("pick two different articles").into_response();
        }
        let ban_top = q.get("ban_top").and_then(|v| v.parse::<usize>().ok());
        // An exact degree limit, used by the client to re-issue a run with
        // identical terms after a server restart orphaned the old id. Runs
        // are in-memory, so a restart mid-race otherwise strands the open
        // tab: the compass and route count would fail for a race that is
        // still perfectly playable. ban_top cannot express a difficulty
        // preset's ban (it is a hub count, not a degree), hence the second
        // parameter rather than a lossy round-trip.
        let ban_degree = q.get("ban_degree").and_then(|v| v.parse::<usize>().ok());
        let out = tokio::task::spawn_blocking(move || {
            let limit = ban_degree.or_else(|| ban_top.and_then(|n| s.game.hub_cut(n, 0).0));
            s.with_finder(|g, pf| g.puzzle_between(pf, a, b, limit))
                .map(|p| issue(&s, p, "custom".into(), None))
        })
        .await;
        return match out {
            Ok(Some(p)) => Json(p).into_response(),
            Ok(None) => err("no route between those articles at this hub level").into_response(),
            Err(_) => err("puzzle generation failed").into_response(),
        };
    }

    // A topic race: both endpoints from one map region. Uses the layout's
    // communities, so it needs nodes.parquet — absent that, the error says
    // so instead of pretending the region is empty.
    if let Some(region) = q.get("region").and_then(|v| v.parse::<i32>().ok()) {
        if s.game.layout.is_none() {
            return err("topic races need the map data (nodes.parquet)").into_response();
        }
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1);
        let label = format!("topic-{}", name);
        let out = tokio::task::spawn_blocking(move || {
            s.with_finder(|g, pf| {
                let mut rng = Rng::new(seed);
                g.puzzle_in_region(pf, region, d, &mut rng)
            })
            .map(|p| issue(&s, p, label, None))
        })
        .await;
        return match out {
            Ok(Some(p)) => Json(p).into_response(),
            Ok(None) => {
                err("that region has no qualifying race at this difficulty").into_response()
            }
            Err(_) => err("puzzle generation failed").into_response(),
        };
    }

    // An explicit seed makes a puzzle reproducible, which is what a shared
    // daily challenge needs: same seed, same race, for everyone.
    let seed = q
        .get("seed")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(1)
        });

    let ban_top = q.get("ban_top").and_then(|v| v.parse::<usize>().ok());
    let out = tokio::task::spawn_blocking(move || {
        puzzle_from_seed(&s, d, seed, ban_top).map(|p| issue(&s, p, name.clone(), None))
    })
    .await;

    match out {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => err("could not generate a puzzle at that difficulty").into_response(),
        Err(_) => err("puzzle generation failed").into_response(),
    }
}

/// Register a generated race and shape the response.
fn issue(
    s: &App,
    p: wiki_parser::game::Puzzle,
    difficulty: String,
    number: Option<u64>,
) -> PuzzleResponse {
    let run = s.runs.issue(RunSpec {
        start: p.start,
        goal: p.goal,
        ban_degree: p.ban_degree,
        par: p.optimal,
        difficulty: difficulty.clone(),
        number,
    });
    PuzzleResponse {
        compass: wiki_parser::runs::compass_charges(p.optimal),
        start: article_ref(&s.game, p.start),
        goal: article_ref(&s.game, p.goal),
        optimal: p.optimal,
        ban_degree: p.ban_degree,
        difficulty,
        number,
        run,
    }
}

async fn submit(
    State(s): State<Shared>,
    axum::Extension(Player(player)): axum::Extension<Player>,
    Json(req): Json<SubmitRequest>,
) -> impl IntoResponse {
    let out = tokio::task::spawn_blocking(move || {
        s.runs
            .submit(&s.game.graph, req.run, &req.path, &req.nickname, &player)
            .map_err(|e| e.message(&s.game.graph))
    })
    .await;

    match out {
        Ok(Ok(a)) => Json(SubmitResponse {
            accepted: true,
            clicks: a.clicks,
            par: a.par,
            ms: a.ms,
            rank: a.rank,
        })
        .into_response(),
        // A rejected run is the client's fault, and the message says which
        // check failed — useful for honest clients, useless to a forger.
        Ok(Err(msg)) => err(msg).into_response(),
        Err(_) => err("submission failed").into_response(),
    }
}

/// How many distinct shortest routes the race had.
///
/// Deliberately answered *after* the race, not at generation. Counting is a
/// full forward BFS carrying path counts — seconds, against 41 ms to generate
/// a puzzle — so putting it in the generation path would make every race slow
/// to start for a number nobody had looked at yet. Asked once at the end, the
/// player is already finished and the wait costs nothing.
///
/// It is also the better difficulty signal: four clicks with one viable route
/// is a far harder puzzle than four clicks with two hundred, and length alone
/// cannot tell those apart.
async fn route_count(
    State(s): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(run) = q.get("run").and_then(|v| v.parse::<u64>().ok()) else {
        return err("run is required").into_response();
    };
    let Some((start, goal, ban, par)) = s.runs.terms(run) else {
        return err("unknown or expired run").into_response();
    };

    let out = tokio::task::spawn_blocking(move || {
        let rev = &s.game.graph.reverse;
        let banned: Box<dyn Fn(u32) -> bool + '_> = match ban {
            Some(limit) => {
                Box::new(move |v: u32| v != start && v != goal && rev.degree(v) > limit)
            }
            None => Box::new(|_| false),
        };
        // +1 on the cap: par came from the same graph, so anything deeper is a
        // disagreement worth surfacing as "unknown" rather than as a number.
        s.with_finder(|g, pf| {
            pf.count_shortest_paths(&g.graph, start, goal, &banned, (par + 1) as u8)
        })
    })
    .await;

    match out {
        Ok(Some((len, count))) => Json(serde_json::json!({
            "clicks": len,
            "routes": count,
            // Saturating means "at least this many", and presenting a clamped
            // value as exact would be a lie about a number nobody can check.
            "saturated": count == u64::MAX,
        }))
        .into_response(),
        Ok(None) => err("no route within par").into_response(),
        Err(_) => err("counting failed").into_response(),
    }
}

#[derive(Deserialize)]
struct CompassRequest {
    run: u64,
    /// The article the player is standing on. Only its links are measured —
    /// handing back the whole distance map would let a client spend one charge
    /// and keep the answer for the rest of the race.
    from: u32,
}

/// Exact distance from each of `from`'s links to the goal.
///
/// This replaces the map-distance arrows, which were measured at a 1.18x lift
/// over guessing. These numbers are the real graph distance, which is why they
/// are rationed: shown for free on every article they would solve the game.
async fn compass(
    State(s): State<Shared>,
    Json(req): Json<CompassRequest>,
) -> impl IntoResponse {
    let (goal, ban, left) = match s.runs.spend_compass(req.run) {
        Ok(v) => v,
        Err(e) => return err(e.message(&s.game.graph)).into_response(),
    };

    let out = tokio::task::spawn_blocking(move || {
        let cached = {
            let cache = s.compass.lock().unwrap();
            cache.iter().find(|(g, _)| *g == goal).map(|(_, d)| Arc::clone(d))
        };
        let dist = match cached {
            Some(d) => d,
            None => {
                let d = Arc::new(
                    s.with_finder(|g, pf| pf.distances_to(&g.graph, goal, COMPASS_DEPTH)),
                );
                let mut cache = s.compass.lock().unwrap();
                if !cache.iter().any(|(g, _)| *g == goal) {
                    cache.push((goal, Arc::clone(&d)));
                    if cache.len() > COMPASS_CACHE {
                        cache.remove(0);
                    }
                }
                d
            }
        };

        let rev = &s.game.graph.reverse;
        let links: Vec<serde_json::Value> = s
            .game
            .graph
            .forward
            .neighbors(req.from)
            .iter()
            .map(|&w| {
                let d = dist[w as usize];
                serde_json::json!({
                    "id": w,
                    // null rather than a number when the goal is further than
                    // the cap or unreachable: "I do not know" is honest, and a
                    // large sentinel would read as a real distance.
                    "dist": if d == u8::MAX { serde_json::Value::Null }
                            else { serde_json::json!(d) },
                    "banned": ban.is_some_and(|l| rev.degree(w) > l && w != goal),
                })
            })
            .collect();
        serde_json::json!({ "charges_left": left, "links": links })
    })
    .await;

    match out {
        Ok(v) => Json(v).into_response(),
        Err(_) => err("compass failed").into_response(),
    }
}

async fn leaderboard(
    State(s): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let number = q.get("number").and_then(|v| v.parse::<u64>().ok());
    // "medium" is a board normal play never writes to — free play sends
    // "hard", custom races "custom", the daily whatever it was asked for — so
    // a caller that omits this gets an empty list rather than a wrong one.
    // The page always sends its own race's values.
    let difficulty = q.get("difficulty").cloned().unwrap_or_else(|| "medium".into());
    let rows: Vec<BoardRow> = s
        .runs
        .leaderboard(number, &difficulty)
        .into_iter()
        .enumerate()
        .take(25)
        .map(|(i, e)| BoardRow {
            rank: i + 1,
            nickname: e.nickname,
            clicks: e.clicks,
            ms: e.ms,
        })
        .collect();
    Json(rows)
}

/// Today's challenge — same race for everyone, derived from the UTC date.
async fn daily(
    State(s): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let name = q.get("difficulty").cloned().unwrap_or_else(|| "medium".into());
    let d = Difficulty::parse(&name);

    let today_day = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|t| t.as_secs() / 86_400)
        .unwrap_or(DAILY_EPOCH_DAY);
    let today_number = today_day.saturating_sub(DAILY_EPOCH_DAY) + 1;

    // The archive: any past daily by number. The seed is a pure function of
    // the day, so old dailies replay exactly. Future numbers stay sealed —
    // tomorrow's puzzle is tomorrow's.
    let number = match q.get("number").and_then(|v| v.parse::<u64>().ok()) {
        None => today_number,
        Some(n) if n >= 1 && n <= today_number => n,
        Some(_) => return err("that daily does not exist yet").into_response(),
    };
    let day = DAILY_EPOCH_DAY + number - 1;

    // Mix the difficulty into the seed so each difficulty has its own daily
    // rather than three names for one race.
    let seed = day
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (d as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);

    let label = name.clone();
    let out = tokio::task::spawn_blocking(move || {
        puzzle_from_seed(&s, d, seed, None).map(|p| issue(&s, p, label, Some(number)))
    })
    .await;

    match out {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => err("could not generate today's puzzle").into_response(),
        Err(_) => err("daily generation failed").into_response(),
    }
}

/// Histogram of posted scores for one daily board — the "38% made par"
/// line. Submissions only, so it is the same opt-in sample Wordle's stats
/// were; the client, which knows par, does the relative math.
async fn dist(
    State(s): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(number) = q.get("number").and_then(|v| v.parse::<u64>().ok()) else {
        return err("number is required").into_response();
    };
    let difficulty = q.get("difficulty").cloned().unwrap_or_else(|| "medium".into());
    let entries = s.runs.leaderboard(Some(number), &difficulty);
    let mut hist: std::collections::BTreeMap<usize, usize> = Default::default();
    for e in &entries {
        *hist.entry(e.clicks).or_default() += 1;
    }
    Json(serde_json::json!({ "total": entries.len(), "hist": hist })).into_response()
}

/// The daily round: nine holes at a designed par profile, the same course
/// for everyone, seeded by the day like the daily but salted apart from it.
/// Holes come from the pools at their exact par (fallback: rejection
/// sampling, which flattens the profile toward par 3 but still plays).
/// Each hole is a normal issued run — compass, routes and submits all work
/// per hole with no special cases.
/// 3 holes is the daily-ritual length (~5-8 min at observed race pace);
/// 9 is the session round. Playtests said single races already run minutes,
/// so the short round is the default — Wordle fits in a coffee break and
/// the default mode here has to as well.
const COURSE_3: [usize; 3] = [3, 3, 4];
const COURSE_9: [usize; 9] = [3, 3, 4, 3, 3, 5, 3, 4, 3];

async fn course(
    State(s): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let today_day = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|t| t.as_secs() / 86_400)
        .unwrap_or(DAILY_EPOCH_DAY);
    let today_number = today_day.saturating_sub(DAILY_EPOCH_DAY) + 1;
    let number = match q.get("number").and_then(|v| v.parse::<u64>().ok()) {
        None => today_number,
        Some(n) if n >= 1 && n <= today_number => n,
        Some(_) => return err("that round does not exist yet").into_response(),
    };
    let day = DAILY_EPOCH_DAY + number - 1;
    let pars: &'static [usize] = match q.get("holes").map(|v| v.as_str()) {
        Some("9") => &COURSE_9,
        _ => &COURSE_3,
    };
    // A different salt than the daily, or the round's first hole IS the
    // daily; the hole count folds in so the 3- and 9-hole rounds differ too.
    let seed = day.wrapping_mul(0xD1B5_4A32_D192_ED03)
        ^ 0xC0FF_EE00_C0FF_EE00
        ^ (pars.len() as u64) << 56;

    let out = tokio::task::spawn_blocking(move || {
        s.with_finder(|g, pf| {
            let mut rng = Rng::new(seed);
            let mut holes = Vec::with_capacity(pars.len());
            for (i, &par) in pars.iter().enumerate() {
                let p = g.course_hole(pf, par, &mut rng)?;
                holes.push((i, p));
            }
            Some(holes)
        })
        .map(|holes| {
            let issued: Vec<PuzzleResponse> = holes
                .into_iter()
                .map(|(i, p)| issue(&s, p, format!("round-h{}", i + 1), Some(number)))
                .collect();
            let par: usize = issued.iter().map(|h| h.optimal).sum();
            serde_json::json!({ "number": number, "par": par, "holes": issued })
        })
    })
    .await;

    match out {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => err("could not build today's round").into_response(),
        Err(_) => err("round generation failed").into_response(),
    }
}

async fn map_points(
    State(s): State<Shared>,
    headers: header::HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let n = q
        .get("n")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000usize)
        .min(wiki_parser::game::MAP_POINTS);

    // This is the heaviest response the service sends — the client asks for
    // 45,000 points, which is ~1.8 MB — and it is byte-identical for every
    // visitor until the next dump. The graph's shape identifies that version,
    // so a revalidation costs a 304 and no body instead of another 1.8 MB.
    let etag = format!(
        "\"{}-{}-{}\"",
        s.game.graph.len(),
        s.game.graph.forward.edges(),
        n
    );
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
    {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }

    let pts = tokio::task::spawn_blocking(move || s.game.map_sample(n))
        .await
        .unwrap_or_default();
    // Flat arrays rather than objects: 20k points as JSON objects is several
    // MB of braces and key names for the same numbers.
    let mut xs = Vec::with_capacity(pts.len());
    let mut ys = Vec::with_capacity(pts.len());
    let mut cs = Vec::with_capacity(pts.len());
    for (x, y, c) in pts {
        xs.push(x);
        ys.push(y);
        cs.push(c);
    }
    (
        [
            (header::CACHE_CONTROL, "public, max-age=86400".to_string()),
            (header::ETAG, etag),
        ],
        Json(serde_json::json!({ "x": xs, "y": ys, "c": cs })),
    )
        .into_response()
}

async fn landmarks(
    State(s): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let n = q.get("n").and_then(|v| v.parse().ok()).unwrap_or(40usize).min(300);
    let out = tokio::task::spawn_blocking(move || s.game.landmarks(n))
        .await
        .unwrap_or_default();
    let items: Vec<serde_json::Value> = out
        .into_iter()
        .map(|(t, x, y, c)| serde_json::json!({ "title": t, "x": x, "y": y, "c": c }))
        .collect();
    (
        [(header::CACHE_CONTROL, "public, max-age=3600")],
        Json(items),
    )
}

/// Cap on simultaneous graph work.
///
/// Every concurrent request that touches the graph holds a PathFinder — 72 MB
/// of scratch at enwiki scale, and 130 MB while counting shortest paths. The
/// pool only caps *reuse*; `with_finder` allocates a fresh one whenever the
/// pool is empty, so the real ceiling is however many blocking tasks tokio
/// will run at once. That defaults to 512, which is 37 GB and an instant OOM
/// on any machine this is likely to run on.
///
/// Bounding it makes the failure mode a queue instead of a crash: past this
/// many simultaneous searches, requests wait a few milliseconds rather than
/// taking the process down. 8 is ~600 MB of scratch, which fits the 6 GB
/// minimum with room to spare, and is far above what 2 vCPU can actually work
/// through concurrently.
const BLOCKING_THREADS: usize = 8;

fn main() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(BLOCKING_THREADS)
        .build()?;
    rt.block_on(serve())
}

async fn serve() -> Result<()> {
    let args = Args::parse();

    eprintln!("loading graph from {}...", args.data.display());
    let t = std::time::Instant::now();
    let game = Game::load_with(&args.data, !args.no_alias_search)?;
    eprintln!(
        "   {} articles, {} edges, map: {} — {:.1}s",
        game.graph.len(),
        game.graph.forward.edges(),
        if game.layout.is_some() { "yes" } else { "NO (run 01_graph_compute.py)" },
        t.elapsed().as_secs_f64()
    );

    let state_dir = args.state.clone().unwrap_or_else(|| args.data.clone());
    if state_dir != args.data {
        std::fs::create_dir_all(&state_dir)
            .with_context(|| format!("creating state directory {}", state_dir.display()))?;
    }
    eprintln!("state (leaderboard, cookie secret) in {}", state_dir.display());

    let state = Arc::new(App {
        game,
        finders: Mutex::new(Vec::new()),
        runs: Registry::open(&state_dir)?,
        rl_read: RateLimiter::new(READ_BURST, READ_PER_SEC),
        rl_heavy: RateLimiter::new(HEAVY_BURST, HEAVY_PER_SEC),
        trust_proxy: args.trust_proxy,
        identity: Identity::load_or_create(&state_dir)?,
        secure_cookies: args.secure_cookies,
        compass: Mutex::new(Vec::new()),
        analytics: Analytics::default(),
    });

    // Hourly usage line. Append-only, best-effort: an unwritable file is
    // reported once and the game keeps serving — analytics must never be the
    // reason the site is down.
    {
        let app = state.clone();
        let path = state_dir.join("analytics.jsonl");
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
            tick.tick().await; // the first tick fires immediately; skip it
            loop {
                tick.tick().await;
                if let Some(line) = app.analytics.flush_line() {
                    use std::io::Write;
                    let r = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .and_then(|mut f| writeln!(f, "{line}"));
                    if let Err(e) = r {
                        eprintln!("analytics: cannot write {}: {e}", path.display());
                    }
                }
            }
        });
    }

    // Prewarm the daily's compass map. The first compass press on the daily
    // otherwise pays the full reverse BFS (~1.4 s at enwiki scale) that
    // everyone after gets from the cache — and the daily is deterministic,
    // so its goal is knowable the moment the graph is up. Seed formula must
    // match daily()'s exactly.
    {
        let app = state.clone();
        tokio::task::spawn_blocking(move || {
            let day = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|t| t.as_secs() / 86_400)
                .unwrap_or(DAILY_EPOCH_DAY);
            let seed = day.wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ (Difficulty::Medium as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            if let Some(p) = puzzle_from_seed(&app, Difficulty::Medium, seed, None) {
                let dist = Arc::new(
                    app.with_finder(|g, pf| pf.distances_to(&g.graph, p.goal, COMPASS_DEPTH)),
                );
                let mut cache = app.compass.lock().unwrap();
                if !cache.iter().any(|(g, _)| *g == p.goal) {
                    cache.push((p.goal, dist));
                }
            }
        });
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/api/meta", get(meta))
        .route("/api/regions", get(regions))
        .route("/api/search", get(search))
        .route("/api/article/{id}", get(article))
        .route("/api/path", post(path))
        .route("/api/puzzle", get(puzzle))
        .route("/api/daily", get(daily))
        .route("/api/map", get(map_points))
        .route("/api/landmarks", get(landmarks))
        .route("/api/hubs", get(hubs))
        .route("/api/submit", post(submit))
        .route("/api/leaderboard", get(leaderboard))
        .route("/api/dist", get(dist))
        .route("/api/course", get(course))
        .route("/api/compass", post(compass))
        .route("/api/routes", get(route_count))
        // Layers run outermost-last, so rate limiting is checked before we
        // bother minting an identity for a request we are about to refuse.
        .layer(middleware::from_fn_with_state(state.clone(), identify))
        .layer(middleware::from_fn_with_state(state.clone(), rate_limit))
        // Outermost: gzip. The page is ~90 KB of HTML+JS and /api/map is
        // 1.7 MB of JSON; both compress 3-4x, which on mobile is the
        // difference between instant and noticeable. CPU cost is microseconds
        // against responses this compressible.
        .layer(tower_http::compression::CompressionLayer::new())
        .with_state(state);

    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("\n  WikiGolf listening on http://{addr}");
    eprintln!(
        "  rate limits: {} reads/s, {} heavy/s per IP{}\n",
        READ_PER_SEC, HEAVY_PER_SEC,
        if args.trust_proxy { " (trusting X-Forwarded-For)" } else { "" }
    );
    axum::serve(
        listener,
        // ConnectInfo is what gives the rate limiter a peer address.
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod analytics_tests {
    use super::Analytics;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn counters_classify_flush_and_reset() {
        let a = Analytics::default();
        let ip = |n| IpAddr::V4(Ipv4Addr::new(10, 0, 0, n));
        a.hit("/", ip(1));
        a.hit("/api/search", ip(1));
        a.hit("/api/article/42", ip(2));
        a.hit("/api/puzzle", ip(2));
        a.hit("/api/daily", ip(3));
        a.hit("/api/map", ip(3)); // -> other

        let line = a.flush_line().expect("counters were nonzero");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["pages"], 1);
        assert_eq!(v["searches"], 1);
        assert_eq!(v["articles"], 1);
        assert_eq!(v["puzzles"], 2);
        assert_eq!(v["other"], 1);
        assert_eq!(v["uniques_today"], 3);

        // Deltas reset on flush; a quiet hour writes nothing at all.
        assert!(a.flush_line().is_none());
        // Uniques are a running per-day count, so the same IP again still
        // yields one line (a hit happened) but no new unique.
        a.hit("/", ip(1));
        let v: serde_json::Value =
            serde_json::from_str(&a.flush_line().unwrap()).unwrap();
        assert_eq!(v["uniques_today"], 3);
    }
}

#[cfg(test)]
mod page_tests {
    /// The UI is embedded at compile time, so a bad edit to static/index.html
    /// cannot fail the build — it ships, and the page silently loses features.
    ///
    /// This is not hypothetical: a slice-based edit here once deleted the whole
    /// custom-race picker along with the link filter and sort handlers, and
    /// everything compiled, tested and deployed green. These are the handlers
    /// the page does not work without.
    #[test]
    fn page_keeps_its_handlers() {
        let page = include_str!("../../static/index.html");
        for needle in [
            "function attachPicker",
            "attachPicker('cfrom'",
            "attachPicker('cto'",
            "$('cgo').onclick",
            "function paintLinks",
            "$('lfilter').addEventListener",
            "$('lsort').addEventListener",
            "function setVeil",
            "$('veilbtn').onclick",
            "$('pause').onclick",
            "$('share').onclick",
            "$('copylink').onclick",
            "$('new').onclick",
            "$('daily').onclick",
            "function setTheme",
            "function readTheme",
            "$('theme').addEventListener",
            "$('helpbtn').onclick",
            "$('compass').onclick",
            "function updateCompass",
            "function linkRow",
            "$('lgroup').addEventListener",
            "/api/regions",
            "$('replay').onclick",
            "$('tgo').onclick",
            "$('ldetails').addEventListener",
            "$('dnum')",
            "function startCountdown",
            "async function showDist",
            "/api/dist",
            "$('round').onclick",
            "$('nexthole').onclick",
            "function startRound",
            "function roundScorecard",
            "function reissueRun",
            "async function apiRun",
            "wr-streak",
            "id=\"fromherebox\"",
            "$('helpclose').onclick",
            "async function loadBoard",
            "$('post').onclick",
        ] {
            assert!(page.contains(needle), "static/index.html lost: {needle}");
        }
    }

    /// Every id the script reaches for with $() must exist in the markup.
    #[test]
    fn every_referenced_id_exists() {
        let page = include_str!("../../static/index.html");
        let ids: Vec<&str> = page
            .match_indices("id=\"")
            .map(|(i, _)| {
                let rest = &page[i + 4..];
                &rest[..rest.find('"').unwrap_or(0)]
            })
            .collect();
        for (i, _) in page.match_indices("$('") {
            let rest = &page[i + 3..];
            let Some(end) = rest.find('\'') else { continue };
            let name = &rest[..end];
            if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            assert!(ids.contains(&name), "$('{name}') has no matching id in the markup");
        }
    }
}
