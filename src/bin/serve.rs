//! serve — the wiki-race HTTP service.
//!
//!   serve --data data/simple --port 8080
//!
//! Holds the whole graph in memory and answers path queries from it. Every
//! response is derived from the parser's Parquet; nothing is fetched from
//! Wikipedia at request time, so the game is self-consistent: the optimal path
//! we report is optimal *in the world the player is playing in*.

use anyhow::Result;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use wiki_parser::game::{Difficulty, Game, Rng};
use wiki_parser::graph::PathFinder;

#[derive(Parser, Debug)]
#[command(name = "serve", about = "Wiki-race HTTP service")]
struct Args {
    /// Directory holding titles.parquet / edges.parquet (and optionally nodes.parquet)
    #[arg(short, long, default_value = "data")]
    data: PathBuf,

    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    #[arg(long, default_value = "127.0.0.1")]
    host: String,
}

/// Pathfinder scratch space is ~72 MB per instance at enwiki scale, so it is
/// pooled rather than allocated per request. Searches are CPU-bound too, so
/// every handler that touches the graph runs on the blocking pool — holding an
/// async worker for 40 ms would stall unrelated requests.
struct App {
    game: Game,
    finders: Mutex<Vec<PathFinder>>,
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
}

fn article_ref(g: &Game, id: u32) -> ArticleRef {
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

async fn article(State(s): State<Shared>, AxPath(id): AxPath<u32>) -> impl IntoResponse {
    if id as usize >= s.game.graph.len() {
        return err("no such article id").into_response();
    }
    let out = tokio::task::spawn_blocking(move || {
        let links: Vec<ArticleRef> = s
            .game
            .graph
            .forward
            .neighbors(id)
            .iter()
            .map(|&v| article_ref(&s.game, v))
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

    let out = tokio::task::spawn_blocking(move || {
        let mut rng = Rng::new(seed);
        s.with_finder(|g, pf| {
            g.puzzle(pf, d, &mut rng).map(|p| PuzzleResponse {
                start: article_ref(g, p.start),
                goal: article_ref(g, p.goal),
                optimal: p.optimal,
                ban_degree: p.ban_degree,
                difficulty: name.clone(),
            })
        })
    })
    .await;

    match out {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => err("could not generate a puzzle at that difficulty").into_response(),
        Err(_) => err("puzzle generation failed").into_response(),
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

    let app = Router::new()
        .route("/", get(index))
        .route("/api/meta", get(meta))
        .route("/api/search", get(search))
        .route("/api/article/{id}", get(article))
        .route("/api/path", post(path))
        .route("/api/puzzle", get(puzzle))
        .route("/api/map", get(map_points))
        .with_state(Arc::new(App { game, finders: Mutex::new(Vec::new()) }));

    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("\n  wiki-race listening on http://{addr}\n");
    axum::serve(listener, app).await?;
    Ok(())
}
