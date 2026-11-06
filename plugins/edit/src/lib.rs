//! Horizon Server plugin (`docs/plugins.md`): `//pos1` and `//pos2` mark a box where the operator
//! stands, or at a position they name; `//set <block>` (or `//fill <block>`) fills it, with `air`
//! to clear it. A fill runs over the next ticks, [`BATCH`] blocks a tick, from the bottom up, and
//! air from the top down, so what stands on a block goes before the block it stands on. There is
//! no undo. Each operator's corners live in memory until they leave. Its messages are
//! [`MESSAGES`], worded in `data/messages`.
use horizon_mod_sdk::{
    messages::Messages,
    server::{Args, Argument, BlockChange, Player, Registration, ServerMod, players, world},
};
use std::collections::{BTreeMap, VecDeque};

/// The permission level the commands need: an operator's.
const OPERATOR: u8 = 2;
/// The blocks set each tick: the most one `world.set-blocks` call takes.
const BATCH: u64 = 4096;
const AIR: &str = "minecraft:air";
/// The messages and their defaults.
const MESSAGES: &[(&str, &str)] = &[
    ("not_operator", "You need to be an operator for that."),
    ("corner_set", "{corner} set: {x} {y} {z}"),
    (
        "corner_set_box",
        "{corner} set: {x} {y} {z} ({blocks} blocks)",
    ),
    ("no_box", "Set both corners first: //pos1 and //pos2."),
    ("usage", "Usage: //set <block>"),
    ("busy", "Your last fill is still running."),
    ("not_loaded", "Part of that box isn't loaded."),
    ("started", "Filling {blocks} blocks with {block}..."),
    ("finished", "Filled {blocks} blocks with {block}."),
];

/// Opposite corners, sorted: both included.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Cuboid {
    min: [i32; 3],
    max: [i32; 3],
}

impl Cuboid {
    fn new(a: [i32; 3], b: [i32; 3]) -> Self {
        Self {
            min: std::array::from_fn(|k| a[k].min(b[k])),
            max: std::array::from_fn(|k| a[k].max(b[k])),
        }
    }

    /// The blocks along each axis.
    fn size(&self) -> [u64; 3] {
        std::array::from_fn(|k| (i64::from(self.max[k]) - i64::from(self.min[k]) + 1) as u64)
    }

    /// Its blocks. Saturates: no box that large has all its chunks loaded.
    fn volume(&self) -> u64 {
        self.size().iter().fold(1, |n, s| n.saturating_mul(*s))
    }

    /// The `n`th block, layer by layer from the bottom, or from the top when `top_down`.
    fn nth(&self, n: u64, top_down: bool) -> [i32; 3] {
        let [sx, _, sz] = self.size();
        let layer = (n / (sx * sz)) as i32;
        let in_layer = n % (sx * sz);
        let y = if top_down {
            self.max[1] - layer
        } else {
            self.min[1] + layer
        };
        [
            self.min[0] + (in_layer % sx) as i32,
            y,
            self.min[2] + (in_layer / sx) as i32,
        ]
    }

    /// Whether the server holds every chunk it crosses, at its lowest layer.
    fn loaded(&self, dimension: &str) -> bool {
        let y = self.min[1];
        (self.min[0] >> 4..=self.max[0] >> 4).all(|cx| {
            (self.min[2] >> 4..=self.max[2] >> 4)
                .all(|cz| world::get_block(dimension, cx << 4, y, cz << 4).is_some())
        })
    }
}

/// What a player typed as a block, with its namespace: none when it isn't one word.
fn block_state(typed: &str) -> Option<String> {
    let typed = typed.trim();
    if typed.is_empty() || typed.contains(char::is_whitespace) {
        return None;
    }
    Some(if typed.contains(':') {
        typed.to_owned()
    } else {
        format!("minecraft:{typed}")
    })
}

/// A player's corners, in the dimension they were set in.
#[derive(Clone, Debug, Default, PartialEq)]
struct Selection {
    dimension: String,
    pos1: Option<[i32; 3]>,
    pos2: Option<[i32; 3]>,
}

impl Selection {
    /// With `corner` at `at`: corners of another dimension don't make a box with this one.
    fn with_corner(&self, corner: &str, dimension: &str, at: [i32; 3]) -> Self {
        let kept = if self.dimension == dimension {
            self.clone()
        } else {
            Self {
                dimension: dimension.to_owned(),
                ..Self::default()
            }
        };
        if corner == "pos1" {
            Self {
                pos1: Some(at),
                ..kept
            }
        } else {
            Self {
                pos2: Some(at),
                ..kept
            }
        }
    }

    fn cuboid(&self) -> Option<Cuboid> {
        Some(Cuboid::new(self.pos1?, self.pos2?))
    }
}

/// A fill under way.
struct Job {
    player: String,
    dimension: String,
    state: String,
    cuboid: Cuboid,
    next: u64,
}

impl Job {
    /// Sets the next batch: true when none is left. An error sets nothing.
    fn step(&mut self) -> Result<bool, String> {
        let top_down = self.state == AIR;
        let end = (self.next + BATCH).min(self.cuboid.volume());
        let blocks: Vec<BlockChange> = (self.next..end)
            .map(|n| {
                let [x, y, z] = self.cuboid.nth(n, top_down);
                BlockChange {
                    x,
                    y,
                    z,
                    state: self.state.clone(),
                }
            })
            .collect();
        world::set_blocks(&self.dimension, &blocks)?;
        self.next = end;
        Ok(end == self.cuboid.volume())
    }
}

struct Edit {
    /// By player id.
    selections: BTreeMap<String, Selection>,
    /// One runs at a time, the oldest first.
    jobs: VecDeque<Job>,
    messages: Messages,
}

impl Edit {
    fn corner(&mut self, player: &Player, corner: &str, args: &Args) -> String {
        let at = match args.block_pos("pos") {
            Some(p) => [p.x, p.y, p.z],
            None => [player.x, player.y, player.z].map(|c| c.floor() as i32),
        };
        let selection = self
            .selections
            .get(&player.id)
            .cloned()
            .unwrap_or_default()
            .with_corner(corner, &player.dimension, at);
        let [x, y, z] = at;
        let label = corner.replace("pos", "Pos");
        let text = match selection.cuboid() {
            Some(cuboid) => {
                let blocks = cuboid.volume();
                let values: [(&str, &dyn std::fmt::Display); 5] = [
                    ("corner", &label),
                    ("x", &x),
                    ("y", &y),
                    ("z", &z),
                    ("blocks", &blocks),
                ];
                self.messages.get("corner_set_box", &values)
            }
            None => {
                let values: [(&str, &dyn std::fmt::Display); 4] =
                    [("corner", &label), ("x", &x), ("y", &y), ("z", &z)];
                self.messages.get("corner_set", &values)
            }
        };
        self.selections.insert(player.id.clone(), selection);
        text
    }

    fn fill(&mut self, player: &Player, typed: &str) -> Result<String, String> {
        let state = block_state(typed).ok_or_else(|| self.messages.get("usage", &[]))?;
        if self.jobs.iter().any(|j| j.player == player.id) {
            return Err(self.messages.get("busy", &[]));
        }
        let selection = self.selections.get(&player.id);
        let cuboid = selection
            .filter(|s| s.dimension == player.dimension)
            .and_then(Selection::cuboid)
            .ok_or_else(|| self.messages.get("no_box", &[]))?;
        let blocks = cuboid.volume();
        if !cuboid.loaded(&player.dimension) {
            return Err(self.messages.get("not_loaded", &[]));
        }
        let mut job = Job {
            player: player.id.clone(),
            dimension: player.dimension.clone(),
            state: state.clone(),
            cuboid,
            next: 0,
        };
        // The first batch now: a state that isn't one fails here, in the player's chat.
        let done = job.step()?;
        let values: [(&str, &dyn std::fmt::Display); 2] = [("blocks", &blocks), ("block", &state)];
        if done {
            return Ok(self.messages.get("finished", &values));
        }
        self.jobs.push_back(job);
        Ok(self.messages.get("started", &values))
    }
}

impl ServerMod for Edit {
    fn init() -> Result<(Self, Registration), String> {
        let corner = || [Argument::block_pos("pos").optional()];
        let block = || [Argument::text("block")];
        Ok((
            Edit {
                selections: BTreeMap::new(),
                jobs: VecDeque::new(),
                messages: Messages::load(MESSAGES),
            },
            Registration::new()
                .command_with("/pos1", corner())
                .command_with("/pos2", corner())
                .command_with("/set", block())
                .command_with("/fill", block())
                .on_tick()
                .on_joins(),
        ))
    }

    fn command(
        &mut self,
        player: &Player,
        name: &str,
        args: &Args,
    ) -> Result<Option<String>, String> {
        if players::permission(&player.id).unwrap_or(0) < OPERATOR {
            return Err(self.messages.get("not_operator", &[]));
        }
        match name {
            "/pos1" => Ok(Some(self.corner(player, "pos1", args))),
            "/pos2" => Ok(Some(self.corner(player, "pos2", args))),
            _ => self.fill(player, args.text()).map(Some),
        }
    }

    fn tick(&mut self, _tick: u64) {
        let Some(job) = self.jobs.front_mut() else {
            return;
        };
        let done = match job.step() {
            Ok(done) => done,
            Err(e) => {
                horizon_mod_sdk::warn(&format!("A fill stopped: {e}"));
                true
            }
        };
        if !done {
            return;
        }
        if let Some(job) = self.jobs.pop_front() {
            let (blocks, state) = (job.next, job.state);
            let values: [(&str, &dyn std::fmt::Display); 2] =
                [("blocks", &blocks), ("block", &state)];
            players::tell(&job.player, &self.messages.get("finished", &values));
        }
    }

    fn player_left(&mut self, player: &Player) {
        self.selections.remove(&player.id);
    }
}

horizon_mod_sdk::export_server_mod!(Edit);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_sort_and_count_both_ends() {
        let c = Cuboid::new([2, 70, -1], [0, 64, 1]);
        assert_eq!((c.min, c.max), ([0, 64, -1], [2, 70, 1]));
        assert_eq!(c.size(), [3, 7, 3]);
        assert_eq!(c.volume(), 63);
    }

    #[test]
    fn every_block_comes_once_layer_by_layer() {
        let c = Cuboid::new([-1, 5, -1], [1, 6, 0]);
        let up: Vec<_> = (0..c.volume()).map(|n| c.nth(n, false)).collect();
        assert_eq!(up[0], [-1, 5, -1]);
        assert_eq!(up[1], [0, 5, -1]);
        assert_eq!(up[3], [-1, 5, 0]);
        assert_eq!(up[6], [-1, 6, -1]);
        let mut unique = up.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 12);
        assert!(
            up.iter()
                .all(|p| (-1..=1).contains(&p[0]) && (-1..=0).contains(&p[2]))
        );
    }

    #[test]
    fn air_goes_from_the_top() {
        let c = Cuboid::new([0, 5, 0], [0, 6, 0]);
        assert_eq!(c.nth(0, true), [0, 6, 0]);
        assert_eq!(c.nth(1, true), [0, 5, 0]);
    }

    #[test]
    fn a_huge_box_saturates_instead_of_wrapping() {
        let c = Cuboid::new(
            [i32::MIN, i32::MIN, i32::MIN],
            [i32::MAX, i32::MAX, i32::MAX],
        );
        assert_eq!(c.volume(), u64::MAX);
    }

    #[test]
    fn a_typed_block_gets_its_namespace() {
        assert_eq!(block_state(" air ").as_deref(), Some(AIR));
        assert_eq!(
            block_state("oak_log[axis=x]").as_deref(),
            Some("minecraft:oak_log[axis=x]")
        );
        assert_eq!(block_state("mod:thing").as_deref(), Some("mod:thing"));
        assert_eq!(block_state(""), None);
        assert_eq!(block_state("stone keep"), None);
    }

    #[test]
    fn another_dimension_starts_a_new_box() {
        let s = Selection::default()
            .with_corner("pos1", "a", [1, 1, 1])
            .with_corner("pos2", "a", [2, 2, 2]);
        assert!(s.cuboid().is_some());
        let moved = s.with_corner("pos1", "b", [3, 3, 3]);
        assert_eq!(moved.dimension, "b");
        assert_eq!(moved.pos2, None);
    }
}
