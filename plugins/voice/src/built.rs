//! The blocks the bot's body placed: breaking one gives nothing (its drops and experience are
//! taken away), so nobody gets items from what the bot built. Kept in `storage` key `built`,
//! 13 bytes a block (dimension, x, y, z), the oldest forgotten past `MAX`.
use std::collections::{HashSet, VecDeque};

const KEY: &str = "built";
pub const MAX: usize = 100_000;
pub const DIMENSIONS: [&str; 3] = [
    "minecraft:overworld",
    "minecraft:the_nether",
    "minecraft:the_end",
];

type Spot = (u8, [i32; 3]);

#[derive(Default)]
pub struct Built {
    spots: HashSet<Spot>,
    order: VecDeque<Spot>,
    pub dirty: bool,
}

/// The dimension's number here; none for others (the body builds only in these).
pub fn dimension(name: &str) -> Option<u8> {
    DIMENSIONS.iter().position(|d| *d == name).map(|i| i as u8)
}

impl Built {
    pub fn load(raw: Option<Vec<u8>>) -> Self {
        let mut built = Built::default();
        let raw = raw.unwrap_or_default();
        for chunk in raw.as_chunks::<13>().0 {
            let n = |i: usize| i32::from_be_bytes(chunk[i..i + 4].try_into().unwrap_or_default());
            built.insert((chunk[0], [n(1), n(5), n(9)]));
        }
        built.dirty = false;
        built
    }

    pub fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.order.len() * 13);
        for (d, [x, y, z]) in &self.order {
            out.push(*d);
            for v in [x, y, z] {
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
        out
    }

    fn insert(&mut self, spot: Spot) {
        if self.spots.insert(spot) {
            self.order.push_back(spot);
            self.dirty = true;
        }
        while self.order.len() > MAX {
            if let Some(old) = self.order.pop_front() {
                self.spots.remove(&old);
            }
        }
    }

    pub fn add(&mut self, dimension: u8, at: [i32; 3]) {
        self.insert((dimension, at));
    }

    /// Whether the bot built the block there; it is forgotten (broken now).
    pub fn take(&mut self, dimension: u8, at: [i32; 3]) -> bool {
        if !self.spots.remove(&(dimension, at)) {
            return false;
        }
        self.order.retain(|s| *s != (dimension, at));
        self.dirty = true;
        true
    }

    /// Up to `count` of the blocks, oldest first, forgotten (to be taken away).
    pub fn drain(&mut self, count: usize) -> Vec<(u8, [i32; 3])> {
        let n = count.min(self.order.len());
        let out: Vec<Spot> = self.order.drain(..n).collect();
        for spot in &out {
            self.spots.remove(spot);
        }
        if !out.is_empty() {
            self.dirty = true;
        }
        out
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn save(&mut self) {
        if self.dirty {
            self.dirty = false;
            if let Err(err) = horizon_mod_sdk::storage::set(KEY, &self.bytes()) {
                horizon_mod_sdk::info(&format!("voice: couldn't keep the bot's blocks: {err}"));
            }
        }
    }
}

/// The commands that take away what breaking the block at `at` dropped.
pub fn sweep(dimension: &str, at: [i32; 3]) -> Vec<String> {
    let [x, y, z] = at.map(|v| f64::from(v) + 0.5);
    ["item", "experience_orb"]
        .iter()
        .map(|kind| {
            format!(
                "execute in {dimension} positioned {x:.1} {y:.1} {z:.1} run kill \
                 @e[type=minecraft:{kind},distance=..1.5]"
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_are_kept_taken_once_and_survive_a_restart() {
        let mut built = Built::default();
        built.add(0, [1, 64, -2]);
        built.add(1, [1, 64, -2]);
        built.add(0, [1, 64, -2]);
        let again = Built::load(Some(built.bytes()));
        assert_eq!(again.order.len(), 2);
        let mut again = again;
        assert!(again.take(0, [1, 64, -2]));
        assert!(!again.take(0, [1, 64, -2]));
        assert!(again.take(1, [1, 64, -2]));
        again.add(0, [0, 70, 0]);
        again.add(2, [5, 70, 5]);
        assert_eq!(again.drain(1), vec![(0, [0, 70, 0])]);
        assert_eq!(again.len(), 1);
        assert!(!again.take(0, [0, 70, 0]));
        assert_eq!(dimension("minecraft:the_nether"), Some(1));
        assert_eq!(dimension("custom:moon"), None);
        assert!(sweep("minecraft:overworld", [1, 64, -2])[0].contains("positioned 1.5 64.5 -1.5"));
    }
}
