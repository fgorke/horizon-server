//! What an intro is made of, and how it is stored: poses tick by tick (the camera's path, each
//! figure's walk), the island and the ground under it, the schematic it is built from, and the
//! settings.
use std::collections::{HashMap, HashSet};

/// Where something is and where it looks, one tick of a recording.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pose {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
}

impl Pose {
    pub fn line(&self) -> String {
        format!(
            "{:.3} {:.3} {:.3} {:.1} {:.1}",
            self.x, self.y, self.z, self.yaw, self.pitch
        )
    }

    pub fn parse(line: &str) -> Option<Pose> {
        let mut parts = line.split_whitespace();
        let mut next = || parts.next()?.parse::<f64>().ok().filter(|v| v.is_finite());
        Some(Pose {
            x: next()?,
            y: next()?,
            z: next()?,
            yaw: next()? as f32,
            pitch: next()? as f32,
        })
    }

    /// The same pose `dy` blocks higher.
    pub fn up(self, dy: f64) -> Pose {
        Pose {
            y: self.y + dy,
            ..self
        }
    }
}

/// A recording: one pose a tick.
pub fn track_text(frames: &[Pose]) -> String {
    let lines: Vec<String> = frames.iter().map(Pose::line).collect();
    lines.join("\n")
}

pub fn parse_track(text: &str) -> Vec<Pose> {
    text.lines().filter_map(Pose::parse).collect()
}

/// A block: where, and its state (`minecraft:oak_log[axis=y]`).
pub type Block = ([i32; 3], String);

/// The floating island, as it stands in the sky: its blocks, and the real blocks where it
/// stood in the ground (`drop` blocks lower, in the same order), which the ripped-out piece
/// leaves as a crater.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Island {
    pub dimension: String,
    pub drop: i32,
    pub blocks: Vec<Block>,
    pub ground: Vec<String>,
}

/// The x and z the island covers, inclusive: `[min x, max x, min z, max z]`.
pub type Footprint = [i32; 4];

impl Island {
    /// `dimension drop`, then the states (`P state`), then a line a block (`B x y z state#`),
    /// then the ground's states in the blocks' order (`G state#`).
    pub fn text(&self) -> String {
        fn index<'a>(
            palette: &mut HashMap<&'a str, usize>,
            states: &mut Vec<&'a str>,
            s: &'a str,
        ) -> usize {
            let n = palette.len();
            *palette.entry(s).or_insert_with(|| {
                states.push(s);
                n
            })
        }
        let mut palette: HashMap<&str, usize> = HashMap::new();
        let mut states: Vec<&str> = Vec::new();
        let blocks: Vec<String> = self
            .blocks
            .iter()
            .map(|([x, y, z], s)| {
                let i = index(&mut palette, &mut states, s);
                format!("B {x} {y} {z} {i}")
            })
            .collect();
        let ground: Vec<String> = self
            .ground
            .iter()
            .map(|s| format!("G {}", index(&mut palette, &mut states, s)))
            .collect();
        let mut out = vec![format!("{} {}", self.dimension, self.drop)];
        out.extend(states.iter().map(|s| format!("P {s}")));
        out.extend(blocks);
        out.extend(ground);
        out.join("\n")
    }

    pub fn parse(text: &str) -> Option<Island> {
        let mut lines = text.lines();
        let mut head = lines.next()?.split_whitespace();
        let mut island = Island {
            dimension: head.next()?.to_owned(),
            drop: head.next()?.parse().ok()?,
            ..Island::default()
        };
        let mut states: Vec<&str> = Vec::new();
        for line in lines {
            let (kind, rest) = line.split_once(' ')?;
            let state = |i: &str| {
                states
                    .get(i.parse::<usize>().ok()?)
                    .map(|s| (*s).to_owned())
            };
            match kind {
                "P" => states.push(rest),
                "B" => {
                    let mut p = rest.split(' ');
                    let mut n = || p.next()?.parse::<i32>().ok();
                    let pos = [n()?, n()?, n()?];
                    island.blocks.push((pos, state(p.next()?)?));
                }
                "G" => island.ground.push(state(rest)?),
                _ => return None,
            }
        }
        (island.ground.len() == island.blocks.len()).then_some(island)
    }

    /// The blocks with a side open to something not of the island: the ones a rising
    /// copy needs, since the others are never seen.
    pub fn surface(&self) -> Vec<usize> {
        let cells: HashSet<[i32; 3]> = self.blocks.iter().map(|(p, _)| *p).collect();
        const SIDES: [[i32; 3]; 6] = [
            [1, 0, 0],
            [-1, 0, 0],
            [0, 1, 0],
            [0, -1, 0],
            [0, 0, 1],
            [0, 0, -1],
        ];
        self.blocks
            .iter()
            .enumerate()
            .filter(|(_, ([x, y, z], _))| {
                SIDES
                    .iter()
                    .any(|[dx, dy, dz]| !cells.contains(&[x + dx, y + dy, z + dz]))
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn footprint(&self) -> Footprint {
        let mut f = [i32::MAX, i32::MIN, i32::MAX, i32::MIN];
        for ([x, _, z], _) in &self.blocks {
            f = [f[0].min(*x), f[1].max(*x), f[2].min(*z), f[3].max(*z)];
        }
        f
    }

    /// The lowest and highest y of the island, in the sky.
    pub fn heights(&self) -> (i32, i32) {
        let ys = self.blocks.iter().map(|([_, y, _], _)| *y);
        (ys.clone().min().unwrap_or(0), ys.max().unwrap_or(0))
    }
}

/// Whether `x z` is over the island (with a block to spare): what stands there rises with it.
pub fn over(f: &Footprint, x: f64, z: f64) -> bool {
    let (x, z) = (x.floor() as i32, z.floor() as i32);
    (f[0] - 1..=f[1] + 1).contains(&x) && (f[2] - 1..=f[3] + 1).contains(&z)
}

/// A schematic to build the island from: its blocks relative to its corner, and the layer
/// that lies at ground level (`ground <y>`, then `x y z state` a line).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Schematic {
    pub ground: i32,
    pub blocks: Vec<Block>,
}

impl Schematic {
    pub fn parse(text: &str) -> Option<Schematic> {
        let mut lines = text.lines();
        let ground = lines.next()?.strip_prefix("ground ")?.trim().parse().ok()?;
        let mut blocks = Vec::new();
        for line in lines.filter(|l| !l.trim().is_empty()) {
            let mut p = line.splitn(4, ' ');
            let mut n = || p.next()?.parse::<i32>().ok();
            let pos = [n()?, n()?, n()?];
            let state = p.next()?.trim().to_owned();
            if !is_air(&state) {
                blocks.push((pos, state));
            }
        }
        Some(Schematic { ground, blocks })
    }

    /// The middle of the blocks' x and z.
    pub fn centre(&self) -> [i32; 2] {
        let xs = self.blocks.iter().map(|([x, _, _], _)| *x);
        let zs = self.blocks.iter().map(|([_, _, z], _)| *z);
        let mid = |lo: Option<i32>, hi: Option<i32>| (lo.unwrap_or(0) + hi.unwrap_or(0)) / 2;
        [
            mid(xs.clone().min(), xs.max()),
            mid(zs.clone().min(), zs.max()),
        ]
    }
}

/// Air of any kind: not part of the island.
pub fn is_air(state: &str) -> bool {
    let id = state.split('[').next().unwrap_or(state);
    matches!(
        id,
        "minecraft:air" | "minecraft:cave_air" | "minecraft:void_air"
    )
}

/// A block state as a display's `block_state` compound:
/// `minecraft:oak_log[axis=y]` is `{Name:"minecraft:oak_log",Properties:{axis:"y"}}`.
pub fn state_nbt(state: &str) -> String {
    let (id, props) = match state.split_once('[') {
        Some((id, rest)) => (id, rest.trim_end_matches(']')),
        None => (state, ""),
    };
    let props: Vec<String> = props
        .split(',')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| format!("{k}:\"{v}\""))
        .collect();
    if props.is_empty() {
        format!("{{Name:\"{id}\"}}")
    } else {
        format!("{{Name:\"{id}\",Properties:{{{}}}}}", props.join(","))
    }
}

/// When the island tears loose and how long it rises, in ticks from the intro's start; how far
/// the camera shakes meanwhile, in blocks; the box `capture` reads.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub pos1: Option<[i32; 3]>,
    pub pos2: Option<[i32; 3]>,
    pub drop: i32,
    pub rip: u32,
    pub rise: u32,
    pub shake: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            pos1: None,
            pos2: None,
            drop: 60,
            rip: 80,
            rise: 100,
            shake: 0.3,
        }
    }
}

impl Settings {
    pub fn text(&self) -> String {
        let pos = |p: Option<[i32; 3]>| p.map(|[x, y, z]| format!("{x} {y} {z}"));
        let mut lines = vec![
            format!("drop {}", self.drop),
            format!("rip {}", self.rip),
            format!("rise {}", self.rise),
            format!("shake {}", self.shake),
        ];
        if let Some(p) = pos(self.pos1) {
            lines.push(format!("pos1 {p}"));
        }
        if let Some(p) = pos(self.pos2) {
            lines.push(format!("pos2 {p}"));
        }
        lines.join("\n")
    }

    pub fn parse(text: &str) -> Settings {
        let mut s = Settings::default();
        for line in text.lines() {
            let words: Vec<&str> = line.split_whitespace().collect();
            let pos = |w: &[&str]| -> Option<[i32; 3]> {
                Some([
                    w.first()?.parse().ok()?,
                    w.get(1)?.parse().ok()?,
                    w.get(2)?.parse().ok()?,
                ])
            };
            match words.as_slice() {
                ["drop", v] => s.drop = v.parse().unwrap_or(s.drop),
                ["rip", v] => s.rip = v.parse().unwrap_or(s.rip),
                ["rise", v] => s.rise = v.parse().unwrap_or(s.rise),
                ["shake", v] => s.shake = v.parse().unwrap_or(s.shake),
                ["pos1", rest @ ..] => s.pos1 = pos(rest),
                ["pos2", rest @ ..] => s.pos2 = pos(rest),
                _ => {}
            }
        }
        s
    }

    /// How far the island has risen at `tick`: nothing before the rip, all of `drop` once it
    /// is up, evenly between (as a display's interpolation moves).
    pub fn lift(&self, tick: u32, drop: i32) -> f64 {
        if tick < self.rip {
            return 0.0;
        }
        let done = f64::from(tick - self.rip) / f64::from(self.rise.max(1));
        f64::from(drop) * done.min(1.0)
    }
}

/// Ticks of rumbling before the island tears loose.
pub const RUMBLE: u32 = 20;

/// The camera's sway around the rip: a light rumble for [`RUMBLE`] ticks before, then
/// `strength` blocks at most, fading out over the rise.
pub fn shake(tick: u32, s: &Settings) -> [f64; 3] {
    let (strength, t, length) = if tick + RUMBLE >= s.rip && tick < s.rip {
        (f64::from(s.shake) * 0.3, tick + RUMBLE - s.rip, RUMBLE * 4)
    } else if tick >= s.rip && tick < s.rip + s.rise {
        (f64::from(s.shake), tick - s.rip, s.rise)
    } else {
        return [0.0; 3];
    };
    let t = f64::from(t);
    let fade = 1.0 - (t / f64::from(length.max(1))).min(1.0);
    let a = strength * fade;
    [
        a * (t * 2.3).sin(),
        a * 0.6 * (t * 3.7).sin(),
        a * (t * 1.9).cos(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poses_round_trip() {
        let p = Pose {
            x: 1.5,
            y: -2.25,
            z: 100.0,
            yaw: 90.0,
            pitch: -12.5,
        };
        assert_eq!(Pose::parse(&p.line()), Some(p));
        assert_eq!(parse_track(&track_text(&[p, p])).len(), 2);
        assert_eq!(Pose::parse("1 2 nope 4 5"), None);
        assert_eq!(Pose::parse("1 2 NaN 4 5"), None);
        assert_eq!(p.up(2.0).y, -0.25);
    }

    fn cube() -> Island {
        let mut island = Island {
            dimension: "minecraft:overworld".into(),
            drop: 30,
            ..Island::default()
        };
        // A 3 x 3 x 3 cube: all but its middle block show.
        for x in 0..3 {
            for y in 0..3 {
                for z in 0..3 {
                    island
                        .blocks
                        .push(([x, y + 100, z], "minecraft:stone".into()));
                    island.ground.push("minecraft:air".into());
                }
            }
        }
        island.blocks[0].1 = "minecraft:oak_log[axis=y]".into();
        island
    }

    #[test]
    fn islands_round_trip_and_find_their_surface() {
        let island = cube();
        assert_eq!(Island::parse(&island.text()), Some(island.clone()));
        let surface = island.surface();
        assert_eq!(surface.len(), 26);
        assert!(!surface.iter().any(|&i| island.blocks[i].0 == [1, 101, 1]));
        assert_eq!(island.footprint(), [0, 2, 0, 2]);
        assert_eq!(island.heights(), (100, 102));
        assert!(over(&island.footprint(), 3.5, -0.5));
        assert!(!over(&island.footprint(), 4.5, 0.5));
        // One state a line, not one a block.
        assert_eq!(
            island
                .text()
                .lines()
                .filter(|l| l.starts_with("P "))
                .count(),
            3
        );
        assert_eq!(Island::parse("minecraft:overworld 3\nB 1 2 3 0"), None);
    }

    #[test]
    fn schematics_skip_air_and_know_their_middle() {
        let s = Schematic::parse("ground 16\n0 0 0 minecraft:stone\n4 1 2 minecraft:air\n10 3 8 minecraft:oak_log[axis=y]\n")
            .unwrap();
        assert_eq!(s.ground, 16);
        assert_eq!(s.blocks.len(), 2);
        assert_eq!(
            s.blocks[1],
            ([10, 3, 8], "minecraft:oak_log[axis=y]".into())
        );
        assert_eq!(s.centre(), [5, 4]);
        assert_eq!(Schematic::parse("0 0 0 minecraft:stone"), None);
    }

    #[test]
    fn states_become_block_state_compounds() {
        assert_eq!(state_nbt("minecraft:stone"), "{Name:\"minecraft:stone\"}");
        assert_eq!(
            state_nbt("minecraft:oak_log[axis=y]"),
            "{Name:\"minecraft:oak_log\",Properties:{axis:\"y\"}}"
        );
        assert_eq!(
            state_nbt("minecraft:grass_block[snowy=false,x=1]"),
            "{Name:\"minecraft:grass_block\",Properties:{snowy:\"false\",x:\"1\"}}"
        );
        assert!(is_air("minecraft:cave_air"));
        assert!(!is_air("minecraft:stone"));
    }

    #[test]
    fn settings_round_trip_and_lift() {
        let s = Settings {
            pos1: Some([1, -2, 3]),
            pos2: Some([4, 5, -6]),
            drop: 33,
            rip: 10,
            rise: 100,
            shake: 0.5,
        };
        assert_eq!(Settings::parse(&s.text()), s);
        assert_eq!(Settings::parse(""), Settings::default());
        assert_eq!(s.lift(5, 60), 0.0);
        assert_eq!(s.lift(60, 60), 30.0);
        assert_eq!(s.lift(500, 60), 60.0);
    }

    #[test]
    fn the_shake_rumbles_then_fades_out() {
        let s = Settings {
            rip: 40,
            rise: 100,
            shake: 1.0,
            ..Settings::default()
        };
        let size = |v: [f64; 3]| v.iter().map(|c| c.abs()).fold(0.0, f64::max);
        assert_eq!(shake(10, &s), [0.0; 3]);
        assert!(size(shake(25, &s)) <= 0.3);
        assert!(size(shake(41, &s)) <= 1.0);
        assert!(size(shake(139, &s)) < 0.05);
        assert_eq!(shake(140, &s), [0.0; 3]);
    }
}
