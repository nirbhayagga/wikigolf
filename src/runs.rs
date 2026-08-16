//! Run issuing, validation and the leaderboard.
//!
//! The security model in one line: **never trust a submitted score, verify a
//! submitted path.** A client that can POST `{"clicks": 1, "time": 3}` will,
//! so the server issues a run, remembers what it issued, and on submission
//! re-walks the claimed route across its own copy of the graph. Every hop must
//! be a real edge, no banned hub may be used, and the clock is the server's.
//!
//! This is not anti-cheat in the strong sense — someone can still solve the
//! puzzle with their own copy of the graph and submit a genuine optimal path.
//! It does make forged scores require actually solving the problem, which is
//! all a leaderboard needs. It is also something only this project can do,
//! because verification needs the graph.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::graph::Graph;

/// Accepted runs, one JSON object per line.
pub const LOG_FILE: &str = "leaderboard.jsonl";

/// No human clicks a link, reads the next page and picks again this fast.
const MIN_MS_PER_CLICK: u64 = 250;
/// Slack for the first page load before the first click.
const MIN_MS_BASE: u64 = 400;
/// Beyond this a run is stale rather than impressive.
const MAX_RUN: Duration = Duration::from_secs(6 * 3600);
/// A legitimate race is nowhere near this long; the cap bounds validation work.
const MAX_PATH: usize = 500;
/// Issued runs are dropped after this long even if never submitted.
const RUN_TTL: Duration = Duration::from_secs(6 * 3600 + 600);
/// Hard cap on live runs, so an attacker cannot grow the map without bound.
const MAX_LIVE_RUNS: usize = 20_000;

#[derive(Clone)]
pub struct Run {
    pub start: u32,
    pub goal: u32,
    pub ban_degree: Option<usize>,
    pub par: usize,
    pub difficulty: String,
    pub number: Option<u64>,
    issued: Instant,
    submitted: bool,
    /// Compass charges left. Tracked on the server because the client cannot
    /// be trusted with a limit that is the whole point of the mechanic.
    compass_left: u8,
}

/// Compass charges for a race of this par.
///
/// A flat allowance does not work, because par is not flat: most races come
/// out at 3, and three charges on a three-click race lets you measure every
/// move you will ever make. The compass stops being a decision and becomes
/// the solution.
///
/// Scaling with par keeps the ratio honest — you can always check roughly
/// half your moves, never all of them:
///
///   par 3 -> 1    par 4 -> 2    par 5 -> 3    par 6+ -> 4
pub fn compass_charges(par: usize) -> u8 {
    par.saturating_sub(2).clamp(1, 4) as u8
}

#[derive(Debug, PartialEq)]
pub enum RunError {
    UnknownRun,
    AlreadySubmitted,
    WrongStart,
    WrongGoal,
    PathTooLong,
    /// `from` does not actually link to `to`.
    BrokenLink(u32, u32),
    /// Routed through a hub banned at this difficulty.
    UsedBannedHub(u32),
    TooFast,
    Expired,
    NoCompassLeft,
}

impl RunError {
    pub fn message(&self, g: &Graph) -> String {
        match self {
            RunError::NoCompassLeft => "no compass charges left".into(),
            RunError::UnknownRun => "unknown or expired run".into(),
            RunError::AlreadySubmitted => "this run was already submitted".into(),
            RunError::WrongStart => "path does not begin at the start article".into(),
            RunError::WrongGoal => "path does not end at the goal article".into(),
            RunError::PathTooLong => format!("path longer than {MAX_PATH} steps"),
            RunError::BrokenLink(a, b) => format!(
                "{:?} does not link to {:?}",
                g.title(*a),
                g.title(*b)
            ),
            RunError::UsedBannedHub(v) => {
                format!("{:?} is banned at this difficulty", g.title(*v))
            }
            RunError::TooFast => "run completed implausibly fast".into(),
            RunError::Expired => "run expired".into(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub nickname: String,
    pub clicks: usize,
    pub ms: u64,
    /// Signed anonymous player id. One entry per player per board, so a single
    /// person cannot occupy the whole leaderboard under many nicknames.
    #[serde(default)]
    pub player: String,
}

#[derive(Serialize, Deserialize)]
struct Record {
    board: String,
    #[serde(flatten)]
    entry: Entry,
}

#[derive(Default)]
pub struct Registry {
    runs: Mutex<HashMap<u64, Run>>,
    board: Mutex<HashMap<String, Vec<Entry>>>,
    next: Mutex<u64>,
    /// Append-only log of accepted runs. Appends are crash-safe and need no
    /// rewrite, unlike dumping the whole board on every submission; the
    /// in-memory board is just a replay of this file.
    log: Mutex<Option<std::fs::File>>,
}

/// Trim a submitted nickname to something safe to store and render.
///
/// Rendering already escapes, so this is about storage hygiene: bounded
/// length, no control characters, no leading/trailing whitespace games.
pub fn clean_nickname(raw: &str) -> String {
    let s: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(20)
        .collect();
    let s = s.trim().to_string();
    if s.is_empty() { "anonymous".into() } else { s }
}

/// Insert into a board, keeping one entry per player and the best order.
fn place(list: &mut Vec<Entry>, entry: Entry) {
    if let Some(existing) = list.iter_mut().find(|e| !e.player.is_empty() && e.player == entry.player)
    {
        // Same player, so keep only their better run rather than letting them
        // stack the board with repeated attempts.
        if (entry.clicks, entry.ms) < (existing.clicks, existing.ms) {
            *existing = entry;
        }
    } else {
        list.push(entry);
    }
    list.sort_by(|a, b| a.clicks.cmp(&b.clicks).then(a.ms.cmp(&b.ms)));
    list.truncate(100);
}

impl Registry {
    /// Replay the log so the board survives a restart.
    pub fn open(dir: &Path) -> Result<Registry> {
        let path = dir.join(LOG_FILE);
        let reg = Registry::default();
        if let Ok(f) = std::fs::File::open(&path) {
            let mut n = 0usize;
            let mut board = reg.board.lock().unwrap();
            for line in BufReader::new(f).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                // A partially written final line (torn by a crash) is skipped
                // rather than aborting the whole restore.
                match serde_json::from_str::<Record>(&line) {
                    Ok(r) => {
                        place(board.entry(r.board).or_default(), r.entry);
                        n += 1;
                    }
                    Err(e) => eprintln!("   skipping malformed leaderboard line: {e}"),
                }
            }
            drop(board);
            eprintln!("   restored {n} leaderboard entries from {}", path.display());
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        *reg.log.lock().unwrap() = Some(f);
        Ok(reg)
    }

    fn append(&self, board: &str, entry: &Entry) {
        let mut guard = self.log.lock().unwrap();
        let Some(f) = guard.as_mut() else { return };
        let rec = Record { board: board.to_string(), entry: entry.clone() };
        if let Ok(mut line) = serde_json::to_string(&rec) {
            line.push('\n');
            // Losing a leaderboard row is not worth failing a request over.
            if let Err(e) = f.write_all(line.as_bytes()) {
                eprintln!("   leaderboard write failed: {e}");
            }
        }
    }

    pub fn issue(&self, run: RunSpec) -> u64 {
        let mut runs = self.runs.lock().unwrap();
        // Opportunistic eviction: cheaper than a timer thread and bounded by
        // the same call that would grow the map.
        if runs.len() >= MAX_LIVE_RUNS {
            runs.retain(|_, r| r.issued.elapsed() < RUN_TTL && !r.submitted);
            if runs.len() >= MAX_LIVE_RUNS {
                runs.clear();
            }
        }
        let mut n = self.next.lock().unwrap();
        *n = n.wrapping_add(1);
        // Mix the counter so ids are not guessable in sequence.
        let id = n.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5DEE_CE66_D1CE_4B9F;
        runs.insert(
            id,
            Run {
                start: run.start,
                goal: run.goal,
                ban_degree: run.ban_degree,
                par: run.par,
                difficulty: run.difficulty,
                number: run.number,
                issued: Instant::now(),
                submitted: false,
                compass_left: compass_charges(run.par),
            },
        );
        id
    }

    pub fn get(&self, id: u64) -> Option<Run> {
        self.runs.lock().unwrap().get(&id).cloned()
    }

    /// Validate a claimed path against the graph and the issued run.
    /// Spend one compass charge, returning the goal to measure toward and how
    /// many charges remain.
    ///
    /// The charge is taken before the BFS runs, so a slow or failed lookup
    /// still costs the player — otherwise a retry loop is free hints.
    pub fn spend_compass(&self, id: u64) -> Result<(u32, Option<usize>, u8), RunError> {
        let mut runs = self.runs.lock().unwrap();
        let run = runs.get_mut(&id).ok_or(RunError::UnknownRun)?;
        if run.submitted {
            return Err(RunError::AlreadySubmitted);
        }
        if run.issued.elapsed() > MAX_RUN {
            return Err(RunError::Expired);
        }
        if run.compass_left == 0 {
            return Err(RunError::NoCompassLeft);
        }
        run.compass_left -= 1;
        Ok((run.goal, run.ban_degree, run.compass_left))
    }

    pub fn submit(
        &self,
        g: &Graph,
        id: u64,
        path: &[u32],
        nickname: &str,
        player: &str,
    ) -> Result<Accepted, RunError> {
        let mut runs = self.runs.lock().unwrap();
        let run = runs.get_mut(&id).ok_or(RunError::UnknownRun)?;
        if run.submitted {
            return Err(RunError::AlreadySubmitted);
        }
        let elapsed = run.issued.elapsed();
        if elapsed > MAX_RUN {
            runs.remove(&id);
            return Err(RunError::Expired);
        }
        if path.len() > MAX_PATH {
            return Err(RunError::PathTooLong);
        }
        if path.first() != Some(&run.start) {
            return Err(RunError::WrongStart);
        }
        if path.last() != Some(&run.goal) {
            return Err(RunError::WrongGoal);
        }

        // Re-walk the route on our own graph. This is the whole point: a score
        // is only accepted if the journey that produced it actually exists.
        for pair in path.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if !g.forward.neighbors(a).contains(&b) {
                return Err(RunError::BrokenLink(a, b));
            }
        }
        if let Some(limit) = run.ban_degree {
            for &v in &path[1..path.len().saturating_sub(1)] {
                if g.reverse.degree(v) > limit {
                    return Err(RunError::UsedBannedHub(v));
                }
            }
        }

        let clicks = path.len() - 1;
        let floor = MIN_MS_BASE + MIN_MS_PER_CLICK * clicks as u64;
        let ms = elapsed.as_millis() as u64;
        if ms < floor {
            return Err(RunError::TooFast);
        }

        run.submitted = true;
        let key = board_key(run.number, &run.difficulty);
        let par = run.par;
        drop(runs);

        let entry = Entry {
            nickname: clean_nickname(nickname),
            clicks,
            ms,
            player: player.to_string(),
        };
        self.append(&key, &entry);

        let mut board = self.board.lock().unwrap();
        let list = board.entry(key).or_default();
        place(list, entry);
        let rank = list
            .iter()
            .position(|e| e.clicks == clicks && e.ms == ms)
            .map(|i| i + 1);

        Ok(Accepted { clicks, par, ms, rank })
    }

    pub fn leaderboard(&self, number: Option<u64>, difficulty: &str) -> Vec<Entry> {
        self.board
            .lock()
            .unwrap()
            .get(&board_key(number, difficulty))
            .cloned()
            .unwrap_or_default()
    }
}

fn board_key(number: Option<u64>, difficulty: &str) -> String {
    match number {
        Some(n) => format!("daily:{n}:{difficulty}"),
        None => format!("free:{difficulty}"),
    }
}

pub struct RunSpec {
    pub start: u32,
    pub goal: u32,
    pub ban_degree: Option<usize>,
    pub par: usize,
    pub difficulty: String,
    pub number: Option<u64>,
}

pub struct Accepted {
    pub clicks: usize,
    pub par: usize,
    pub ms: u64,
    pub rank: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nicknames_are_bounded_and_stripped() {
        assert_eq!(clean_nickname("  bob  "), "bob");
        assert_eq!(clean_nickname(""), "anonymous");
        assert_eq!(clean_nickname("   "), "anonymous");
        assert_eq!(clean_nickname("a\u{0}b\nc"), "abc");
        assert_eq!(clean_nickname(&"x".repeat(200)).len(), 20);
    }

    fn e(player: &str, clicks: usize, ms: u64) -> Entry {
        Entry { nickname: player.into(), clicks, ms, player: player.into() }
    }

    #[test]
    fn one_entry_per_player_keeping_their_best() {
        let mut list = Vec::new();
        place(&mut list, e("ann", 6, 9000));
        place(&mut list, e("ann", 4, 9000));   // improvement
        place(&mut list, e("ann", 9, 1000));   // worse on clicks
        assert_eq!(list.len(), 1, "a player must not stack the board");
        assert_eq!(list[0].clicks, 4);
    }

    #[test]
    fn ranks_by_clicks_then_time() {
        let mut list = Vec::new();
        place(&mut list, e("slow", 3, 90_000));
        place(&mut list, e("fast", 3, 10_000));
        place(&mut list, e("fewest", 2, 99_000));
        assert_eq!(
            list.iter().map(|x| x.nickname.as_str()).collect::<Vec<_>>(),
            ["fewest", "fast", "slow"],
            "clicks lead, time breaks ties"
        );
    }

    #[test]
    fn anonymous_entries_are_not_merged_together() {
        // An empty player id means "unknown", not "the same person".
        let mut list = Vec::new();
        place(&mut list, Entry { nickname: "a".into(), clicks: 3, ms: 1, player: String::new() });
        place(&mut list, Entry { nickname: "b".into(), clicks: 4, ms: 1, player: String::new() });
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn board_key_separates_daily_from_free_play() {
        assert_ne!(board_key(Some(1), "hard"), board_key(None, "hard"));
        assert_ne!(board_key(Some(1), "hard"), board_key(Some(1), "easy"));
        assert_ne!(board_key(Some(1), "hard"), board_key(Some(2), "hard"));
    }
}

#[cfg(test)]
mod compass_tests {
    use super::*;

    #[test]
    fn charges_scale_with_par_and_never_cover_every_move() {
        // The point of the scaling: on a three-click race you get one look,
        // not three. Anything that lets you measure every move is the answer,
        // not a hint.
        assert_eq!(compass_charges(3), 1);
        assert_eq!(compass_charges(4), 2);
        assert_eq!(compass_charges(5), 3);
        for par in 2..12 {
            assert!(
                (compass_charges(par) as usize) < par.max(2),
                "par {par} would let the compass cover the whole route"
            );
        }
    }

    #[test]
    fn always_at_least_one_and_never_runaway() {
        assert_eq!(compass_charges(0), 1);
        assert_eq!(compass_charges(1), 1);
        assert_eq!(compass_charges(2), 1);
        assert_eq!(compass_charges(50), 4);
    }
}
