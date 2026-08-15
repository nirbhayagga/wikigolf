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
#[command(name = "serve", about = "Wiki-race HTTP service")]
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
}

/// The player id resolved from a request's cookie, attached by middleware.
#[derive(Clone)]
struct Player(String);

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
        "/api/search" | "/api/path" | "/api/puzzle" | "/api/daily" | "/api/submit"
    );
    let ok = if heavy { app.rl_heavy.allow(ip) } else { app.rl_read.allow(ip) };
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
        x,
        y,
        community,
        in_degree: g.graph.reverse.degree(id),
        banned,
    }
}

#[derive(Serialize)]
struct Meta {
    articles: usize,
    edges: usize,
    has_map: bool,
    bounds: Option<[f32; 4]>,
}

#[derive(Serialize)]
struct ArticleDetail {
    #[serde(flatten)]
    article: ArticleRef,
    links: Vec<ArticleRef>,
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

async fn index() -> Html<&'static str> {
    Html(include_str!("../../static/index.html"))
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
        ArticleDetail { article: article_ref(&s.game, id), links }
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
    let from = q.get("from").and_then(|v| v.parse::<u32>().ok());
    let to = q.get("to").and_then(|v| v.parse::<u32>().ok());
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
        let out = tokio::task::spawn_blocking(move || {
            let limit = ban_top.and_then(|n| s.game.hub_cut(n, 0).0);
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

async fn leaderboard(
    State(s): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let number = q.get("number").and_then(|v| v.parse::<u64>().ok());
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

    let day = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|t| t.as_secs() / 86_400)
        .unwrap_or(DAILY_EPOCH_DAY);
    let number = day.saturating_sub(DAILY_EPOCH_DAY) + 1;

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

async fn map_points(
    State(s): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let n = q
        .get("n")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000usize)
        .min(120_000);
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
        [(header::CACHE_CONTROL, "public, max-age=3600")],
        Json(serde_json::json!({ "x": xs, "y": ys, "c": cs })),
    )
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    eprintln!("loading graph from {}...", args.data.display());
    let t = std::time::Instant::now();
    let game = Game::load(&args.data)?;
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
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/meta", get(meta))
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
        // Layers run outermost-last, so rate limiting is checked before we
        // bother minting an identity for a request we are about to refuse.
        .layer(middleware::from_fn_with_state(state.clone(), identify))
        .layer(middleware::from_fn_with_state(state.clone(), rate_limit))
        .with_state(state);

    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("\n  wiki-race listening on http://{addr}");
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
