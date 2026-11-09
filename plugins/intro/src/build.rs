//! `/intro build <lift>`: the schematic's island into the world, as the intro leaves it. Its
//! ground layer goes where the operator stands, its middle under them; the piece is cut out of
//! the ground there (a crater, and the air over it cleared) and stands `lift` blocks higher.
//! Then the island is read back, as `capture` would, and saved.
//!
//! A plugin's call has a CPU budget, and an island has thousands of blocks, so every part of
//! the work runs over the ticks in slices: reading the schematic, planning, carving, placing,
//! checking, reading back.
use crate::scene::{Block, Island, IslandWriter, Schematic, is_air};
use horizon_mod_sdk::server::world::{self, BlockChange};
use std::collections::HashMap;

/// `world.set-blocks`' limit per call.
const PER_TICK: usize = 4096;
/// Schematic lines read, blocks checked or read back, per tick.
const LINES_PER_TICK: usize = 2000;
const CHECKS_PER_TICK: usize = 2048;
const AIR: &str = "minecraft:air";
/// The world's build limit.
const TOP: i32 = 319;

#[derive(Debug, PartialEq)]
enum Phase {
    /// The schematic's lines, from this byte on.
    Parse(usize),
    /// Where the island goes.
    Plan,
    /// What the crater takes.
    Crater,
    /// The crater, from the top down so nothing is left standing on air.
    Carve,
    /// The island, from the bottom up so each block has what it stands on.
    Place,
    /// Blocks that broke off while their neighbours were missing, again.
    Fix,
    /// The island as it now stands, and the ground under it.
    Read,
    /// The island saved, as `capture` saves it.
    Save,
    Done,
}

pub struct Build {
    pub player: String,
    pub dimension: String,
    pub lift: i32,
    feet: [i32; 3],
    text: String,
    schematic: Schematic,
    /// Where the schematic's corner goes in the ground.
    offset: [i32; 3],
    carve: Vec<[i32; 3]>,
    /// The island in the sky, bottom up.
    sky: Vec<Block>,
    island: Island,
    writer: IslandWriter,
    phase: Phase,
    next: usize,
}

impl Build {
    /// A build of the schematic `text` for an operator standing on `feet`: an error when its
    /// first line isn't `ground <y>`.
    pub fn new(
        text: String,
        player: &str,
        dimension: &str,
        feet: [i32; 3],
        lift: i32,
    ) -> Result<Build, String> {
        let first = text.lines().next().unwrap_or_default();
        let ground = Schematic::header(first).ok_or("its first line isn't ground <y>")?;
        let start = first.len() + 1;
        Ok(Build {
            player: player.to_owned(),
            dimension: dimension.to_owned(),
            lift,
            feet,
            text,
            schematic: Schematic {
                ground,
                blocks: Vec::new(),
            },
            offset: [0; 3],
            carve: Vec::new(),
            sky: Vec::new(),
            island: Island {
                dimension: dimension.to_owned(),
                drop: lift,
                ..Island::default()
            },
            writer: IslandWriter::default(),
            phase: Phase::Parse(start),
            next: 0,
        })
    }

    /// Blocks set so far and in all (0 of 0 while the schematic is read).
    #[cfg(test)]
    fn progress(&self) -> (usize, usize) {
        let all = self.carve.len() + self.sky.len() * 3;
        let done = match self.phase {
            Phase::Parse(_) | Phase::Plan | Phase::Crater => 0,
            Phase::Carve => self.next,
            Phase::Place => self.carve.len() + self.next,
            Phase::Fix => self.carve.len() + self.sky.len() + self.next,
            Phase::Read => self.carve.len() + self.sky.len() * 2 + self.next,
            Phase::Save | Phase::Done => all,
        };
        (done, all)
    }

    /// One tick's work. `Ok(Some(island))` when the island stands and is read back.
    pub fn step(&mut self) -> Result<Option<Island>, String> {
        match self.phase {
            Phase::Parse(at) => self.parse(at)?,
            Phase::Plan => self.plan()?,
            Phase::Crater => self.crater(),
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
                let end = (self.next + CHECKS_PER_TICK).min(self.sky.len());
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
                self.advance(end, self.sky.len(), Phase::Read);
            }
            Phase::Read => {
                let end = (self.next + CHECKS_PER_TICK).min(self.sky.len());
                self.read(end)?;
                self.advance(end, self.sky.len(), Phase::Save);
            }
            Phase::Save => {
                if self.writer.write(&self.island, LINES_PER_TICK) {
                    let text = std::mem::take(&mut self.writer.out);
                    horizon_mod_sdk::storage::set(crate::ISLAND, text.as_bytes())?;
                    self.phase = Phase::Done;
                }
            }
            Phase::Done => return Ok(Some(std::mem::take(&mut self.island))),
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

    fn parse(&mut self, at: usize) -> Result<(), String> {
        let rest = self.text.get(at..).unwrap_or_default();
        let mut used = 0;
        for line in rest.split_inclusive('\n').take(LINES_PER_TICK) {
            used += line.len();
            let block = Schematic::line(line.trim_end()).ok_or_else(|| {
                format!("a schematic line isn't x y z state: {}", line.trim_end())
            })?;
            self.schematic.blocks.extend(block);
            if self.schematic.blocks.len() > crate::MAX_ISLAND {
                return Err(format!("it has over {} blocks", crate::MAX_ISLAND));
            }
        }
        self.phase = if at + used >= self.text.len() {
            self.text = String::new();
            Phase::Plan
        } else {
            Phase::Parse(at + used)
        };
        Ok(())
    }

    /// Where each block goes: the schematic's ground layer is the block under the feet.
    fn plan(&mut self) -> Result<(), String> {
        let s = &self.schematic;
        if s.blocks.is_empty() {
            return Err("the schematic has no blocks".into());
        }
        let [cx, cz] = s.centre();
        let feet = self.feet;
        self.offset = [feet[0] - cx, feet[1] - 1 - s.ground, feet[2] - cz];
        let [ox, oy, oz] = self.offset;
        let lift = self.lift;
        let mut sky: Vec<Block> = std::mem::take(&mut self.schematic.blocks)
            .into_iter()
            .map(|([x, y, z], s)| ([x + ox, y + oy + lift, z + oz], s))
            .collect();
        sky.sort_unstable_by_key(|([x, y, z], _)| (*y, *z, *x));
        let top = sky.last().map_or(0, |([_, y, _], _)| *y);
        if top > TOP {
            return Err(format!("its top would be at y {top}, over {TOP}"));
        }
        // Every chunk the work touches is loaded.
        let mut chunks: Vec<(i32, i32)> =
            sky.iter().map(|([x, _, z], _)| (x >> 4, z >> 4)).collect();
        chunks.sort_unstable();
        chunks.dedup();
        if !chunks
            .iter()
            .all(|(cx, cz)| world::get_block(&self.dimension, cx * 16, feet[1], cz * 16).is_some())
        {
            return Err("part of it isn't loaded: stand nearer".into());
        }
        self.sky = sky;
        self.phase = Phase::Crater;
        Ok(())
    }

    /// The crater: where the island stood, and the air over the ground in each column it
    /// went down into; from the top down.
    fn crater(&mut self) {
        let ground = self.feet[1] - 1;
        let top = self.sky.last().map_or(0, |([_, y, _], _)| *y) - self.lift;
        // Each column that reaches under the ground: cleared from the feet up.
        let mut columns: HashMap<(i32, i32), ()> = HashMap::new();
        for ([x, y, z], _) in &self.sky {
            if y - self.lift <= ground {
                columns.insert((*x, *z), ());
            }
        }
        let mut carve: Vec<[i32; 3]> = Vec::with_capacity(self.sky.len() + columns.len() * 8);
        for ([x, y, z], _) in &self.sky {
            let y = y - self.lift;
            // Over the ground those columns are cleared whole below.
            if y > ground && columns.contains_key(&(*x, *z)) {
                continue;
            }
            carve.push([*x, y, *z]);
        }
        for &(x, z) in columns.keys() {
            for y in ground + 1..=top {
                carve.push([x, y, z]);
            }
        }
        carve.sort_unstable_by_key(|[x, y, z]| (-y, *z, *x));
        self.carve = carve;
        self.phase = Phase::Carve;
    }

    fn read(&mut self, end: usize) -> Result<(), String> {
        let get = |[x, y, z]: [i32; 3]| {
            world::get_block(&self.dimension, x, y, z)
                .ok_or_else(|| format!("{x} {y} {z} isn't loaded"))
        };
        for ([x, y, z], _) in &self.sky[self.next..end] {
            let state = get([*x, *y, *z])?;
            if is_air(&state) {
                continue;
            }
            let ground = get([*x, *y - self.lift, *z])?;
            self.island.blocks.push(([*x, *y, *z], state));
            self.island.ground.push(ground);
        }
        Ok(())
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

    /// A pillar of 3 under the ground layer (y 2) and a block on top (y 3).
    const PILLAR: &str = "ground 2\n1 0 1 minecraft:stone\n1 1 1 minecraft:stone\n1 2 1 minecraft:dirt\n1 3 1 minecraft:oak_log[axis=y]\n";

    #[test]
    fn the_island_goes_up_and_the_crater_down() {
        let mut b =
            Build::new(PILLAR.into(), "p", "minecraft:overworld", [10, 64, 20], 50).unwrap();
        b.parse(9).unwrap();
        assert_eq!(b.phase, Phase::Plan);
        assert_eq!(b.schematic.blocks.len(), 4);
        // Plan reads the world (none in a unit test): its arithmetic, by hand.
        let [cx, cz] = b.schematic.centre();
        assert_eq!([cx, cz], [1, 1]);
        b.sky = b
            .schematic
            .blocks
            .iter()
            .map(|([x, y, z], s)| ([x + 9, y + 61 + 50, z + 19], s.clone()))
            .collect();
        b.crater();
        // The ground layer under the feet; the island 50 higher, bottom up.
        let ys: Vec<i32> = b.sky.iter().map(|([_, y, _], _)| *y).collect();
        assert_eq!(ys, [111, 112, 113, 114]);
        // The crater from the top down: the pillar and the block over the ground.
        let carved: Vec<i32> = b.carve.iter().map(|[_, y, _]| *y).collect();
        assert_eq!(carved, [64, 63, 62, 61]);
        assert!(b.carve.iter().all(|[x, _, z]| (*x, *z) == (10, 20)));
        assert_eq!(b.phase, Phase::Carve);
        assert_eq!(b.progress(), (0, 16));
    }

    #[test]
    fn a_schematic_is_read_a_slice_a_tick() {
        let mut text = String::from("ground 0\n");
        for i in 0..(LINES_PER_TICK + 10) {
            text.push_str(&format!("{i} 0 0 minecraft:stone\n"));
        }
        let mut b = Build::new(text, "p", "minecraft:overworld", [0, 64, 0], 50).unwrap();
        b.step().unwrap();
        assert!(matches!(b.phase, Phase::Parse(_)));
        assert_eq!(b.schematic.blocks.len(), LINES_PER_TICK);
        b.step().unwrap();
        assert_eq!(b.phase, Phase::Plan);
        assert_eq!(b.schematic.blocks.len(), LINES_PER_TICK + 10);
    }

    #[test]
    fn bad_schematics_are_refused() {
        assert!(Build::new("1 2 3 minecraft:stone".into(), "p", "d", [0; 3], 50).is_err());
        let mut b = Build::new("ground 0\nnonsense".into(), "p", "d", [0; 3], 50).unwrap();
        assert!(b.step().is_err());
    }
}
