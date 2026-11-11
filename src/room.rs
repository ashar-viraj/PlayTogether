//! Where each device sits around the listener, remembered across runs.
//!
//! Automatic placement spaces devices evenly with this PC at 12 o'clock, gives the front-right
//! and front-left seats to the best speakers (computers, then tablets) and puts phones at the
//! sides and back. Once someone drags a device, the layout becomes custom: nobody moves again,
//! and newcomers go in the middle of the biggest gap.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::spatial::{Layout, Member, norm};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Computer,
    Tablet,
    #[default]
    Phone,
}

impl Kind {
    /// Bigger speakers rank higher and get the front seats.
    fn rank(self) -> u8 {
        match self {
            Kind::Computer => 3,
            Kind::Tablet => 2,
            Kind::Phone => 1,
        }
    }
}

pub const PC_KEY: &str = "pc";

#[derive(Default, Serialize, Deserialize)]
pub struct Positions {
    pub custom: bool,
    /// Seat angle per device key (the receiver's stored id, or `pc`).
    angles: HashMap<String, f32>,
    /// Order devices were first seen in, so automatic placement doesn't reshuffle on reconnects.
    first_seen: HashMap<String, u64>,
}

pub struct Seat {
    pub member: Member,
    pub key: String,
    pub kind: Kind,
}

impl Positions {
    fn path() -> PathBuf {
        crate::state::state_dir().join("layout.json")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        if cfg!(test) {
            return;
        }
        let path = Self::path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Someone dragged a device: from now on the layout is theirs.
    pub fn place(&mut self, key: &str, angle: f32) {
        self.custom = true;
        self.angles.insert(key.to_owned(), norm(angle));
        self.save();
    }

    pub fn auto_arrange(&mut self) {
        self.custom = false;
        self.save();
    }

    /// Seats the PC (if it's playing) and the receivers, and returns the resulting layout.
    pub fn seat(&mut self, pc_playing: bool, mut receivers: Vec<Seat>) -> Layout {
        for r in &receivers {
            let next = self.first_seen.len() as u64;
            self.first_seen.entry(r.key.clone()).or_insert(next);
        }
        receivers.sort_by_key(|r| (Reverse(r.kind.rank()), self.first_seen[&r.key]));
        let mut seats = Vec::new();
        if pc_playing {
            seats.push(Seat { member: Member::Pc, key: PC_KEY.to_owned(), kind: Kind::Computer });
        }
        seats.extend(receivers);

        if self.custom {
            for s in &seats {
                if !self.angles.contains_key(&s.key) {
                    let taken: Vec<f32> = seats.iter().filter_map(|x| self.angles.get(&x.key).copied()).collect();
                    let angle = if s.member == Member::Pc { 0.0 } else { middle_of_largest_gap(&taken) };
                    self.angles.insert(s.key.clone(), angle);
                }
            }
        } else {
            for (s, angle) in seats.iter().zip(auto_slots(seats.len(), pc_playing)) {
                self.angles.insert(s.key.clone(), angle);
            }
        }
        self.save();
        Layout::new(seats.iter().map(|s| (s.member, self.angles[&s.key])).collect())
    }
}

/// Evenly spaced seats, in the order they should be handed out: the PC's 12 o'clock first
/// (when it plays), then from the front backwards, right before left.
fn auto_slots(n: usize, pc_playing: bool) -> Vec<f32> {
    if n == 0 {
        return Vec::new();
    }
    let step = 360.0 / n as f32;
    // Without the PC, keep the front between two devices (e.g. 45°/315° for four), like a stereo pair.
    let offset = if pc_playing || n == 1 { 0.0 } else { step / 2.0 };
    let mut slots: Vec<f32> = (0..n).map(|k| offset + k as f32 * step).collect();
    let from = usize::from(pc_playing);
    slots[from..].sort_by_key(|&a| ((a.min(360.0 - a) * 100.0).round() as i64, a > 180.0));
    slots
}

fn middle_of_largest_gap(taken: &[f32]) -> f32 {
    let mut sorted = taken.to_vec();
    sorted.sort_by(f32::total_cmp);
    match sorted.as_slice() {
        [] => 0.0,
        [only] => norm(only + 180.0),
        _ => {
            let n = sorted.len();
            let (start, gap) = (0..n)
                .map(|i| (sorted[i], norm(sorted[(i + 1) % n] - sorted[i])))
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .unwrap();
            norm(start + gap / 2.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(id: u64, kind: Kind) -> Seat {
        Seat { member: Member::Client(id), key: format!("dev{id}"), kind }
    }

    #[test]
    fn best_speakers_get_the_front_seats() {
        let mut p = Positions::default();
        let layout = p.seat(true, vec![seat(1, Kind::Phone), seat(2, Kind::Computer), seat(3, Kind::Phone), seat(4, Kind::Tablet)]);
        let at = |m| layout.angle_of(m).unwrap();
        assert_eq!(at(Member::Pc), 0.0);
        assert_eq!(at(Member::Client(2)), 72.0); // laptop: front-right
        assert_eq!(at(Member::Client(4)), 288.0); // tablet: front-left
        assert_eq!(at(Member::Client(1)), 144.0); // first phone: back-right
        assert_eq!(at(Member::Client(3)), 216.0);
    }

    #[test]
    fn custom_layouts_keep_seats_and_fill_the_biggest_gap() {
        let mut p = Positions::default();
        p.seat(true, vec![seat(1, Kind::Phone)]);
        p.place("dev1", 90.0);
        let layout = p.seat(true, vec![seat(1, Kind::Phone), seat(2, Kind::Phone)]);
        assert_eq!(layout.angle_of(Member::Client(1)), Some(90.0));
        assert_eq!(layout.angle_of(Member::Client(2)), Some(225.0));
    }
}
