//! Live head-to-head rooms — built separated, shipped dark.
//!
//! The server mounts these routes only under `--enable-duels`, and no UI
//! references them yet: this is the multiplayer skeleton kept warm until
//! the analytics say two players are ever online at once. The runtime cost
//! of the module is what it looks like: a HashMap of rooms and a broadcast
//! channel per room, kilobytes each, riding the same process that already
//! holds the graph. Multiplayer here is expensive in engineering, not in
//! hosting — which is exactly why the engineering is done early and cheap.
//!
//! Protocol (all JSON text frames over one WebSocket per player):
//!   client -> server   {"type":"click","at":"<title>","clicks":n}
//!                      {"type":"finish","clicks":n,"ms":t}
//!   server -> everyone {"type":"joined"|"left","name":..,"players":[..]}
//!                      client messages relayed with "name" attached
//!
//! The server relays and referees presence; it does not re-validate duel
//! paths (duels never touch the leaderboard). Rooms die when empty or old.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

/// Kilobytes per room, so this cap is generosity, not protection.
const MAX_ROOMS: usize = 1_000;
const MAX_PLAYERS: usize = 8;
const ROOM_TTL: Duration = Duration::from_secs(2 * 3600);

pub struct Room {
    pub start: u32,
    pub goal: u32,
    pub par: usize,
    created: Instant,
    tx: broadcast::Sender<String>,
    players: Vec<String>,
}

#[derive(Default)]
pub struct Duels {
    rooms: Mutex<HashMap<String, Room>>,
}

#[derive(Debug)]
pub enum JoinError {
    NoSuchRoom,
    Full,
    NameTaken,
}

impl Duels {
    /// Create a room around an already-generated puzzle. Returns its code —
    /// six characters from an alphabet with no 0/O/1/I, because the code's
    /// whole job is being read aloud to a friend.
    pub fn create(&self, start: u32, goal: u32, par: usize, seed: u64) -> Option<String> {
        let mut rooms = self.rooms.lock().unwrap();
        rooms.retain(|_, r| r.created.elapsed() < ROOM_TTL);
        if rooms.len() >= MAX_ROOMS {
            return None;
        }
        const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let mut s = seed | 1;
        loop {
            let mut code = String::with_capacity(6);
            for _ in 0..6 {
                s ^= s >> 12;
                s ^= s << 25;
                s ^= s >> 27;
                code.push(ALPHABET[(s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 59) as usize
                    % ALPHABET.len()] as char);
            }
            if !rooms.contains_key(&code) {
                let (tx, _) = broadcast::channel(64);
                rooms.insert(
                    code.clone(),
                    Room { start, goal, par, created: Instant::now(), tx, players: Vec::new() },
                );
                return Some(code);
            }
        }
    }

    /// Join: returns the room's terms, a receiver of everyone's events, and
    /// a sender for this player's. The joined/left announcements are the
    /// server's own frames, so presence cannot be spoofed by clients.
    #[allow(clippy::type_complexity)]
    pub fn join(
        &self,
        code: &str,
        name: &str,
    ) -> Result<((u32, u32, usize), broadcast::Sender<String>, broadcast::Receiver<String>), JoinError>
    {
        let mut rooms = self.rooms.lock().unwrap();
        let room = rooms.get_mut(code).ok_or(JoinError::NoSuchRoom)?;
        if room.players.len() >= MAX_PLAYERS {
            return Err(JoinError::Full);
        }
        if room.players.iter().any(|p| p == name) {
            return Err(JoinError::NameTaken);
        }
        room.players.push(name.to_string());
        let _ = room.tx.send(
            serde_json::json!({"type":"joined","name":name,"players":room.players}).to_string(),
        );
        Ok(((room.start, room.goal, room.par), room.tx.clone(), room.tx.subscribe()))
    }

    /// Leave announces, then reaps the room if it emptied.
    pub fn leave(&self, code: &str, name: &str) {
        let mut rooms = self.rooms.lock().unwrap();
        let Some(room) = rooms.get_mut(code) else { return };
        room.players.retain(|p| p != name);
        let _ = room.tx.send(
            serde_json::json!({"type":"left","name":name,"players":room.players}).to_string(),
        );
        if room.players.is_empty() {
            rooms.remove(code);
        }
    }

    pub fn room_terms(&self, code: &str) -> Option<(u32, u32, usize)> {
        let rooms = self.rooms.lock().unwrap();
        rooms.get(code).map(|r| (r.start, r.goal, r.par))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_join_relay_leave() {
        let d = Duels::default();
        let code = d.create(1, 2, 3, 42).unwrap();
        assert_eq!(code.len(), 6);
        assert!(!code.contains('0') && !code.contains('O'));

        let (terms, tx, mut rx_a) = d.join(&code, "alice").unwrap();
        assert_eq!(terms, (1, 2, 3));
        let (_, _, mut rx_b) = d.join(&code, "bob").unwrap();
        // Alice hears bob join; the frame carries the roster.
        let joined = rx_a.try_recv().unwrap();
        assert!(joined.contains("bob") && joined.contains("alice"));

        // A relayed click reaches the other player. Bob subscribed after
        // his own announcement went out, so the click is his first frame —
        // nobody hears their own arrival, by construction.
        tx.send(r#"{"type":"click","name":"alice","at":"Cat"}"#.into()).unwrap();
        assert!(rx_b.try_recv().unwrap().contains("Cat"));

        assert!(matches!(d.join(&code, "alice"), Err(JoinError::NameTaken)));

        d.leave(&code, "alice");
        d.leave(&code, "bob");
        // Empty room is reaped.
        assert!(d.room_terms(&code).is_none());
        assert!(matches!(d.join(&code, "carol"), Err(JoinError::NoSuchRoom)));
    }

    #[test]
    fn room_caps_players() {
        let d = Duels::default();
        let code = d.create(1, 2, 3, 7).unwrap();
        for i in 0..MAX_PLAYERS {
            d.join(&code, &format!("p{i}")).unwrap();
        }
        assert!(matches!(d.join(&code, "late"), Err(JoinError::Full)));
    }
}
