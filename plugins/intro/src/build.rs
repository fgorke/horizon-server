//! `/intro build <lift>`: the schematic's island into the world, as the intro leaves it. Its
//! ground layer goes where the operator stands, its middle under them; the piece is cut out of
//! the ground there (a crater, and the air over it cleared) and stands `lift` blocks higher.
//! The work runs over the ticks, [`PER_TICK`] blocks a tick; then the island is read back, as
//! `capture` would.
use crate::scene::{Block, Island, Schematic};
use horizon_mod_sdk::server::world::{self, BlockChange};
use std::collections::BTreeSet;

/// `world.set-blocks`' limit per call.
pub const PER_TICK: usize = 4096;
const AIR: &str = "minecraft:air";
/// The world's build limit.
const TOP: i32 = 319;

#[derive(Debug, PartialEq)]
enum Phase {
    /// The crater, from the top down so nothing is left standing on air.
    Carve,
    /// The island, from the bottom up so each block has what it stands on.
    Place,
    /// Blocks that broke off while their neighbours were missing, again.
    Fix,
    Done,
}

pub struct Build {
    pub player: String,
    pub dimension: String,
    pub lift: i32,
    carve: Vec<[i32; 3]>,
    /// The island in the sky, bottom up.
    sky: Vec<Block>,
    phase: Phase,
    next: usize,
}

impl Build {
    /// Where the schematic goes for an operator standing on `feet`: an error says why it
    /// doesn't fit.
    pub fn plan(
        schematic: &Schematic,
        player: &str,
        dimension: &str,
        feet: [i32; 3],
        lift: i32,
    ) -> Result<Build, String> {
        let [cx, cz] = schematic.centre();
        // The schematic's ground layer is the block under the operator's feet.
        let offset = [feet[0] - cx, feet[1] - 1 - schematic.ground, feet[2] - cz];
        let at = |[x, y, z]: [i32; 3], up: i32| [x + offset[0], y + offset[1] + up, z + offset[2]];
        let mut sky: Vec<Block> = schematic
            .blocks
            .iter()
            .map(|(p, s)| (at(*p, lift), s.clone()))
            .collect();
        sky.sort_by_key(|([x, y, z], _)| (*y, *z, *x));
        let top = sky.iter().map(|([_, y, _], _)| *y).max().unwrap_or(0);
        if top > TOP {
            return Err(format!("its top would be at y {top}, over {TOP}"));
        }
        // The crater: where the island stood, and the air over the ground where it stood.
        let ground_top = top - lift;
        let mut carve: BTreeSet<(i32, i32, i32)> = BTreeSet::new();
        for (p, _) in &schematic.blocks {
            let [x, y, z] = at(*p, 0);
            carve.insert((-y, z, x));
            if y < feet[1] {
                for above in feet[1]..=ground_top {
                    carve.insert((-above, z, x));
                }
            }
        }
        let carve = carve.into_iter().map(|(y, z, x)| [x, -y, z]).collect();
        Ok(Build {
            player: player.to_owned(),
            dimension: dimension.to_owned(),
            lift,
            carve,
            sky,
            phase: Phase::Carve,
            next: 0,
        })
    }

    /// Whether every chunk the work touches is loaded.
    pub fn loaded(&self) -> bool {
        let chunks: BTreeSet<(i32, i32)> = self
            .sky
            .iter()
            .map(|([x, _, z], _)| (x >> 4, z >> 4))
            .collect();
        let y = self.sky.first().map_or(0, |([_, y, _], _)| *y);
        chunks
            .iter()
            .all(|(cx, cz)| world::get_block(&self.dimension, cx * 16, y, cz * 16).is_some())
    }

    /// Blocks set so far and in all.
    pub fn progress(&self) -> (usize, usize) {
        let all = self.carve.len() + self.sky.len() * 2;
        let done = match self.phase {
            Phase::Carve => self.next,
            Phase::Place => self.carve.len() + self.next,
            Phase::Fix => self.carve.len() + self.sky.len() + self.next,
            Phase::Done => all,
        };
        (done, all)
    }

    /// One tick's work. `Ok(Some(island))` when the island stands.
    pub fn step(&mut self) -> Result<Option<Island>, String> {
        match self.phase {
            Phase::Carve => {
                let end = (self.next + PER_TICK).min(self.carve.len());
                let blocks: Vec<BlockChange> = self.carve[self.next..end]
                    .iter()
                    .map(|p| change(*p, AIR))
                    .collect();
                world::set_blocks(&self.dimension, &blocks)?;
                self.advance(end, self.carve.len(), Phase::Place);
            }
            Phase::Place => {
                let end = (self.next + PER_TICK).min(self.sky.len());
                let blocks: Vec<BlockChange> = self.sky[self.next..end]
                    .iter()
                    .map(|(p, s)| change(*p, s))
                    .collect();
                world::set_blocks(&self.dimension, &blocks)?;
                self.advance(end, self.sky.len(), Phase::Fix);
            }
            Phase::Fix => {
                let end = (self.next + PER_TICK).min(self.sky.len());
                let wrong: Vec<BlockChange> = self.sky[self.next..end]
                    .iter()
                    .filter(|([x, y, z], s)| {
                        world::get_block(&self.dimension, *x, *y, *z).as_deref() != Some(s)
                    })
                    .map(|(p, s)| change(*p, s))
                    .collect();
                if !wrong.is_empty() {
                    world::set_blocks(&self.dimension, &wrong)?;
                }
                self.advance(end, self.sky.len(), Phase::Done);
            }
            Phase::Done => return self.read().map(Some),
        }
        Ok(None)
    }

    fn advance(&mut self, end: usize, len: usize, then: Phase) {
        self.next = end;
        if end >= len {
            self.next = 0;
            self.phase = then;
        }
    }

    /// The island as it now stands, and the ground under it.
    fn read(&self) -> Result<Island, String> {
        let get = |[x, y, z]: [i32; 3]| {
            world::get_block(&self.dimension, x, y, z)
                .ok_or_else(|| format!("{x} {y} {z} isn't loaded"))
        };
        let mut island = Island {
            dimension: self.dimension.clone(),
            drop: self.lift,
            ..Island::default()
        };
        for ([x, y, z], _) in &self.sky {
            let state = get([*x, *y, *z])?;
            if crate::scene::is_air(&state) {
                continue;
            }
            island.blocks.push(([*x, *y, *z], state));
            island.ground.push(get([*x, *y - self.lift, *z])?);
        }
        Ok(island)
    }
}

fn change([x, y, z]: [i32; 3], state: &str) -> BlockChange {
    BlockChange {
        x,
        y,
        z,
        state: state.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schematic() -> Schematic {
        // A pillar of 3 under the ground layer (y 2) and a block on top (y 3).
        Schematic::parse("ground 2\n1 0 1 minecraft:stone\n1 1 1 minecraft:stone\n1 2 1 minecraft:dirt\n1 3 1 minecraft:oak_log[axis=y]")
            .unwrap()
    }

    #[test]
    fn the_island_goes_up_and_the_crater_down() {
        let b = Build::plan(&schematic(), "p", "minecraft:overworld", [10, 64, 20], 50).unwrap();
        // The ground layer under the feet; the island 50 higher, bottom up.
        let ys: Vec<i32> = b.sky.iter().map(|([_, y, _], _)| *y).collect();
        assert_eq!(ys, [111, 112, 113, 114]);
        assert!(b.sky.iter().all(|([x, _, z], _)| (*x, *z) == (10, 20)));
        // The crater from the top down: the pillar and the block over the ground.
        let carved: Vec<i32> = b.carve.iter().map(|[_, y, _]| *y).collect();
        assert_eq!(carved, [64, 63, 62, 61]);
        assert_eq!(b.progress(), (0, 12));
    }

    #[test]
    fn too_high_is_refused() {
        assert!(Build::plan(&schematic(), "p", "minecraft:overworld", [0, 300, 0], 50).is_err());
    }
}
