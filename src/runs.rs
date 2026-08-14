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

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::graph::Graph;

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
}

impl RunError {
    pub fn message(&self, g: &Graph) -> String {
        match self {
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

#[derive(Clone)]
pub struct Entry {
    pub nickname: String,
    pub clicks: usize,
    pub ms: u64,
}

#[derive(Default)]
pub struct Registry {
    runs: Mutex<HashMap<u64, Run>>,
    board: Mutex<HashMap<String, Vec<Entry>>>,
    next: Mutex<u64>,
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

impl Registry {
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
            },
        );
        id
    }

    pub fn get(&self, id: u64) -> Option<Run> {
        self.runs.lock().unwrap().get(&id).cloned()
    }

    /// Validate a claimed path against the graph and the issued run.
    pub fn submit(
        &self,
        g: &Graph,
        id: u64,
        path: &[u32],
        nickname: &str,
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

        let entry = Entry { nickname: clean_nickname(nickname), clicks, ms };
        let mut board = self.board.lock().unwrap();
        let list = board.entry(key).or_default();
        list.push(entry);
        // Clicks first, time as the tiebreaker.
        list.sort_by(|a, b| a.clicks.cmp(&b.clicks).then(a.ms.cmp(&b.ms)));
        list.truncate(100);
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

    #[test]
    fn board_key_separates_daily_from_free_play() {
        assert_ne!(board_key(Some(1), "hard"), board_key(None, "hard"));
        assert_ne!(board_key(Some(1), "hard"), board_key(Some(1), "easy"));
        assert_ne!(board_key(Some(1), "hard"), board_key(Some(2), "hard"));
    }
}
