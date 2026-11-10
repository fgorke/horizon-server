//! What an intro is made of, and how it is stored: poses tick by tick (the camera's path, each
//! figure's walk), the island and the ground under it, the schematic it is built from, and the
//! settings.
use std::collections::HashMap;

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

/// A point of the camera's path: where it is, and the ticks it takes to get there from the
/// point before (nothing, for the first).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub pose: Pose,
    pub ticks: u32,
}

/// The points, a line each: `ticks x y z yaw pitch`.
pub fn points_text(points: &[Point]) -> String {
    let lines: Vec<String> = points
        .iter()
        .map(|p| format!("{} {}", p.ticks, p.pose.line()))
        .collect();
    lines.join("\n")
}

pub fn parse_points(text: &str) -> Vec<Point> {
    text.lines()
        .filter_map(|line| {
            let (ticks, pose) = line.split_once(' ')?;
            Some(Point {
                pose: Pose::parse(pose)?,
                ticks: ticks.parse().ok()?,
            })
        })
        .collect()
}

/// The tick the camera reaches point `n` at.
pub fn point_tick(points: &[Point], n: usize) -> u32 {
    points.iter().take(n + 1).skip(1).map(|p| p.ticks).sum()
}

/// The camera's path through the points, a pose a tick: a curve through them all
/// (Catmull-Rom), turning the short way round. Nothing with under two points.
pub fn path(points: &[Point]) -> Vec<Pose> {
    if points.len() < 2 {
        return Vec::new();
    }
    // Yaws without the jump from 180 to -180, so the curve turns the short way.
    let mut yaws: Vec<f64> = Vec::with_capacity(points.len());
    for p in points {
        let yaw = f64::from(p.pose.yaw);
        let turned = match yaws.last() {
            Some(&last) => last + (yaw - last + 180.0).rem_euclid(360.0) - 180.0,
            None => yaw,
        };
        yaws.push(turned);
    }
    let last = points.len() - 1;
    let curve = |get: &dyn Fn(usize) -> f64, i: usize, u: f64| {
        let (p0, p1) = (get(i.saturating_sub(1)), get(i));
        let (p2, p3) = (get(i + 1), get((i + 2).min(last)));
        0.5 * (2.0 * p1
            + (p2 - p0) * u
            + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * u * u
            + (3.0 * p1 - p0 - 3.0 * p2 + p3) * u * u * u)
    };
    let mut frames = vec![points[0].pose];
    for i in 0..last {
        let ticks = points[i + 1].ticks.max(1);
        for k in 1..=ticks {
            let u = f64::from(k) / f64::from(ticks);
            let yaw = curve(&|n| yaws[n], i, u);
            frames.push(Pose {
                x: curve(&|n| points[n].pose.x, i, u),
                y: curve(&|n| points[n].pose.y, i, u),
                z: curve(&|n| points[n].pose.z, i, u),
                yaw: ((yaw + 180.0).rem_euclid(360.0) - 180.0) as f32,
                pitch: curve(&|n| f64::from(points[n].pose.pitch), i, u).clamp(-90.0, 90.0) as f32,
            });
        }
    }
    frames
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
    /// `dimension drop`, then a line a block (`B x y z state#`), then the ground's states in
    /// the blocks' order (`G state#`); each state is numbered by a `P state` line before its
    /// first use.
    pub fn text(&self) -> String {
        let mut w = IslandWriter::default();
        while !w.write(self, usize::MAX) {}
        w.out
    }

    /// An island's text read back; with no `G` lines, a list of blocks (an undo).
    pub fn parse(text: &str) -> Option<Island> {
        let mut r = IslandReader::new(text.to_owned())?;
        while !r.read(usize::MAX)? {}
        r.finish()
    }

    /// The island's blocks by where they stand: a grid over its box.
    pub fn grid(&self) -> Grid {
        if self.blocks.is_empty() {
            return Grid::default();
        }
        let f = self.footprint();
        let (low, high) = self.heights();
        let lo = [f[0], low, f[2]];
        let size = [f[1] - f[0] + 1, high - low + 1, f[3] - f[2] + 1];
        let mut grid = Grid {
            lo,
            size,
            cells: vec![NONE; (size[0] * size[1] * size[2]) as usize],
        };
        for (i, (p, _)) in self.blocks.iter().enumerate() {
            if let Some(cell) = grid.cell(*p) {
                grid.cells[cell] = i as u32;
            }
        }
        grid
    }

    /// The blocks a rising copy needs: those with a side open to air or to a block one sees
    /// past (stairs, glass, leaves...). The others are never seen.
    pub fn surface(&self, grid: &Grid) -> Vec<usize> {
        const SIDES: [[i32; 3]; 6] = [
            [1, 0, 0],
            [-1, 0, 0],
            [0, 1, 0],
            [0, -1, 0],
            [0, 0, 1],
            [0, 0, -1],
        ];
        // Asked once a kind of block: an island has thousands of blocks of a few kinds.
        let mut kinds: HashMap<&str, bool> = HashMap::new();
        let hides: Vec<bool> = self
            .blocks
            .iter()
            .map(|(_, s)| *kinds.entry(s.as_str()).or_insert_with(|| occludes(s)))
            .collect();
        self.blocks
            .iter()
            .enumerate()
            .filter(|(_, ([x, y, z], _))| {
                SIDES.iter().any(|[dx, dy, dz]| {
                    grid.get([x + dx, y + dy, z + dz]).is_none_or(|n| !hides[n])
                })
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// What changes for the eye when the island, `from` blocks up from the ground, goes to
    /// `to` blocks up: the cells that get another block, and the cells it leaves, which show
    /// the ground it was torn from again (or air, over it).
    pub fn step(&self, grid: &Grid, from: i32, to: i32) -> Vec<Block> {
        let by = to - from;
        let mut out = Vec::new();
        if by <= 0 {
            return out;
        }
        for ([x, y, z], state) in &self.blocks {
            // Where this block goes stood the island's block `by` higher.
            let was = grid.get([*x, y + by, *z]).map(|n| &self.blocks[n].1);
            if was != Some(state) {
                out.push(([*x, y - self.drop + to, *z], state.clone()));
            }
            // Where it stood comes the block `by` lower, or nothing.
            if grid.get([*x, y - by, *z]).is_none() {
                let ground = grid
                    .get([*x, y + from, *z])
                    .and_then(|n| self.ground.get(n));
                let left = ground.map_or("minecraft:air", String::as_str);
                out.push(([*x, y - self.drop + from, *z], left.to_owned()));
            }
        }
        out
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

const NONE: u32 = u32::MAX;

/// Which of an island's blocks stands where.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Grid {
    lo: [i32; 3],
    size: [i32; 3],
    cells: Vec<u32>,
}

impl Grid {
    fn cell(&self, p: [i32; 3]) -> Option<usize> {
        let d = [p[0] - self.lo[0], p[1] - self.lo[1], p[2] - self.lo[2]];
        (0..3)
            .all(|i| (0..self.size[i]).contains(&d[i]))
            .then(|| ((d[1] * self.size[2] + d[2]) * self.size[0] + d[0]) as usize)
    }

    /// The index of the island's block at `p`, in the sky.
    pub fn get(&self, p: [i32; 3]) -> Option<usize> {
        let i = self.cells[self.cell(p)?];
        (i != NONE).then_some(i as usize)
    }
}

/// Whether a block hides what is behind it: a full, opaque cube. Told by its name, and no
/// when in doubt: a block shown for nothing costs little, one missing leaves a hole.
pub fn occludes(state: &str) -> bool {
    let id = state.split('[').next().unwrap_or(state);
    let id = id.strip_prefix("minecraft:").unwrap_or(id);
    const OPEN: [&str; 12] = [
        "stairs", "slab", "wall", "fence", "pane", "door", "glass", "leaves", "path", "farmland",
        "button", "plate",
    ];
    const FULL: [&str; 30] = [
        "stone",
        "dirt",
        "grass_block",
        "andesite",
        "diorite",
        "granite",
        "gravel",
        "sand",
        "_ore",
        "planks",
        "_log",
        "_wood",
        "bricks",
        "deepslate",
        "podzol",
        "clay",
        "terracotta",
        "concrete",
        "wool",
        "hay_block",
        "barrel",
        "smoker",
        "furnace",
        "crafting_table",
        "bookshelf",
        "beehive",
        "tuff",
        "mud",
        "basalt",
        "obsidian",
    ];
    !OPEN.iter().any(|w| id.contains(w)) && FULL.iter().any(|w| id.contains(w))
}

/// An island's text, written a slice at a time: a big island's in one go costs more than a
/// tick allows.
#[derive(Default)]
pub struct IslandWriter {
    pub out: String,
    palette: HashMap<String, usize>,
    /// Lines written: the blocks', then the ground's.
    done: usize,
}

impl IslandWriter {
    /// Writes up to `lines` more lines of `island`: true once it is all written.
    pub fn write(&mut self, island: &Island, lines: usize) -> bool {
        use std::fmt::Write;
        let all = island.blocks.len() + island.ground.len();
        if self.done == 0 && self.out.is_empty() {
            let _ = write!(self.out, "{} {}", island.dimension, island.drop);
        }
        let end = self.done.saturating_add(lines).min(all);
        for i in self.done..end {
            let n = island.blocks.len();
            let state = if i < n {
                &island.blocks[i].1
            } else {
                &island.ground[i - n]
            };
            let index = match self.palette.get(state.as_str()) {
                Some(&k) => k,
                None => {
                    let k = self.palette.len();
                    self.palette.insert(state.clone(), k);
                    self.out.push_str("\nP ");
                    self.out.push_str(state);
                    k
                }
            };
            if i < n {
                let [x, y, z] = island.blocks[i].0;
                let _ = write!(self.out, "\nB {x} {y} {z} {index}");
            } else {
                let _ = write!(self.out, "\nG {index}");
            }
        }
        self.done = end;
        end == all
    }
}

/// An island's text, read a slice at a time.
pub struct IslandReader {
    text: String,
    at: usize,
    states: Vec<String>,
    pub island: Island,
}

impl IslandReader {
    /// None when the first line isn't `dimension drop`.
    pub fn new(text: String) -> Option<IslandReader> {
        let first = text.lines().next()?;
        let mut head = first.split_whitespace();
        let island = Island {
            dimension: head.next()?.to_owned(),
            drop: head.next()?.parse().ok()?,
            ..Island::default()
        };
        let at = (first.len() + 1).min(text.len());
        Some(IslandReader {
            text,
            at,
            states: Vec::new(),
            island,
        })
    }

    /// Reads up to `lines` more lines: `Some(true)` once all are read, None at a bad line.
    pub fn read(&mut self, lines: usize) -> Option<bool> {
        let rest = self.text.get(self.at..)?;
        let mut used = 0;
        for line in rest.split_inclusive('\n').take(lines) {
            used += line.len();
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            let (kind, rest) = line.split_once(' ')?;
            let state = |i: &str| self.states.get(i.parse::<usize>().ok()?).cloned();
            match kind {
                "P" => self.states.push(rest.to_owned()),
                "B" => {
                    let mut p = rest.split(' ');
                    let mut n = || p.next()?.parse::<i32>().ok();
                    let pos = [n()?, n()?, n()?];
                    let s = state(p.next()?)?;
                    self.island.blocks.push((pos, s));
                }
                "G" => {
                    let s = state(rest)?;
                    self.island.ground.push(s);
                }
                _ => return None,
            }
        }
        self.at += used;
        Some(self.at >= self.text.len())
    }

    /// The island read: None when its ground doesn't match its blocks.
    pub fn finish(self) -> Option<Island> {
        let i = self.island;
        (i.ground.is_empty() || i.ground.len() == i.blocks.len()).then_some(i)
    }
}

/// Whether `x z` is over the island (with a block to spare): what stands there rises with it.
pub fn over(f: &Footprint, x: f64, z: f64) -> bool {
    let (x, z) = (x.floor() as i32, z.floor() as i32);
    (f[0] - 1..=f[1] + 1).contains(&x) && (f[2] - 1..=f[3] + 1).contains(&z)
}

/// A schematic to build the island from: its blocks relative to its corner, the layer that
/// lies at ground level, and the crater's lining, which only the ground gets (`ground <y>`,
/// then `x y z state` a block, `L x y z state` a lining block).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Schematic {
    pub ground: i32,
    pub blocks: Vec<Block>,
    pub lining: Vec<Block>,
}

/// One line of a schematic.
#[derive(Debug, PartialEq)]
pub enum Line {
    Block(Block),
    Lining(Block),
    /// Air, or a blank line.
    Skip,
}

impl Schematic {
    #[cfg(test)]
    pub fn parse(text: &str) -> Option<Schematic> {
        let mut lines = text.lines();
        let mut s = Schematic {
            ground: Schematic::header(lines.next()?)?,
            ..Schematic::default()
        };
        for line in lines {
            s.add(Schematic::line(line)?);
        }
        Some(s)
    }

    pub fn add(&mut self, line: Line) {
        match line {
            Line::Block(b) => self.blocks.push(b),
            Line::Lining(b) => self.lining.push(b),
            Line::Skip => {}
        }
    }

    /// The first line: `ground <y>`.
    pub fn header(line: &str) -> Option<i32> {
        line.strip_prefix("ground ")?.trim().parse().ok()
    }

    /// A line, `x y z state` or `L x y z state`: None when it is malformed.
    pub fn line(line: &str) -> Option<Line> {
        let line = line.trim();
        if line.is_empty() {
            return Some(Line::Skip);
        }
        let (lining, line) = match line.strip_prefix("L ") {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        let mut p = line.splitn(4, ' ');
        let mut n = || p.next()?.parse::<i32>().ok();
        let pos = [n()?, n()?, n()?];
        let state = p.next()?.trim();
        if is_air(state) {
            return Some(Line::Skip);
        }
        let b = (pos, state.to_owned());
        Some(if lining {
            Line::Lining(b)
        } else {
            Line::Block(b)
        })
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
    /// The island rises as blocks, a step a tick, instead of as displays gliding.
    pub blocks: bool,
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
            blocks: false,
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
        if self.blocks {
            lines.push("rise blocks".into());
        }
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
                ["rise", v] if *v != "blocks" => s.rise = v.parse().unwrap_or(s.rise),
                ["shake", v] => s.shake = v.parse().unwrap_or(s.shake),
                ["rise", "blocks"] => s.blocks = true,
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
        let grid = island.grid();
        let surface = island.surface(&grid);
        assert_eq!(surface.len(), 26);
        assert_eq!(grid.get([0, 100, 0]), Some(0));
        assert_eq!(grid.get([3, 100, 0]), None);
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
            blocks: true,
        };
        assert_eq!(Settings::parse(&s.text()), s);
        assert_eq!(Settings::parse(""), Settings::default());
        assert_eq!(s.lift(5, 60), 0.0);
        assert_eq!(s.lift(60, 60), 30.0);
        assert_eq!(s.lift(500, 60), 60.0);
    }

    #[test]
    fn one_sees_past_stairs_but_not_past_stone() {
        assert!(occludes("minecraft:stone"));
        assert!(occludes("minecraft:oak_log[axis=y]"));
        assert!(occludes("minecraft:iron_ore"));
        assert!(!occludes("minecraft:spruce_stairs[facing=north]"));
        assert!(!occludes("minecraft:stone_brick_slab[type=bottom]"));
        assert!(!occludes("minecraft:glass"));
        assert!(!occludes("minecraft:oak_leaves[persistent=true]"));
        assert!(!occludes("minecraft:lantern[hanging=true]"));
        // The cube's middle shows once a side of it is glass.
        let mut island = cube();
        let side = island
            .blocks
            .iter()
            .position(|(p, _)| *p == [0, 101, 1])
            .unwrap();
        island.blocks[side].1 = "minecraft:glass".into();
        assert_eq!(island.surface(&island.grid()).len(), 27);
    }

    #[test]
    fn a_step_changes_only_what_the_eye_sees_change() {
        // A column: stone, stone, dirt, 30 over the ground it left as a, b, c.
        let island = Island {
            dimension: "d".into(),
            drop: 30,
            blocks: vec![
                ([0, 100, 0], "minecraft:stone".into()),
                ([0, 101, 0], "minecraft:stone".into()),
                ([0, 102, 0], "minecraft:dirt".into()),
            ],
            ground: vec!["a".into(), "b".into(), "c".into()],
        };
        let grid = island.grid();
        let mut one = island.step(&grid, 0, 1);
        one.sort();
        // The lower stone moves into stone: nothing to send for it.
        assert_eq!(
            one,
            [
                ([0, 70, 0], "a".to_owned()),
                ([0, 72, 0], "minecraft:stone".to_owned()),
                ([0, 73, 0], "minecraft:dirt".to_owned()),
            ]
        );
        // Out of the ground by two more: the ground's b and c show, then air.
        let mut more = island.step(&grid, 1, 3);
        more.sort();
        assert_eq!(more[0], ([0, 71, 0], "b".to_owned()));
        assert_eq!(more[1], ([0, 72, 0], "c".to_owned()));
        assert_eq!(island.step(&grid, 29, 30).len(), 3);
        assert!(island.step(&grid, 5, 5).is_empty());
        let last = island.step(&grid, 29, 30);
        assert!(last.contains(&([0, 99, 0], "minecraft:air".to_owned())));
        assert!(last.contains(&([0, 102, 0], "minecraft:dirt".to_owned())));
    }

    #[test]
    fn the_camera_curves_through_its_points() {
        let at = |x: f64, yaw: f32, ticks: u32| Point {
            pose: Pose {
                x,
                yaw,
                ..Pose::default()
            },
            ticks,
        };
        let points = [at(0.0, 170.0, 0), at(10.0, -170.0, 10), at(30.0, -90.0, 20)];
        assert_eq!(parse_points(&points_text(&points)), points);
        assert_eq!(point_tick(&points, 0), 0);
        assert_eq!(point_tick(&points, 2), 30);
        let frames = path(&points);
        assert_eq!(frames.len(), 31);
        // It passes through each point at its tick, and turns the short way (through 180).
        assert_eq!(frames[0], points[0].pose);
        assert!((frames[10].x - 10.0).abs() < 1e-9);
        assert!((frames[30].x - 30.0).abs() < 1e-9);
        assert!(frames[5].yaw.abs() > 170.0);
        assert!(frames.windows(2).all(|w| w[1].x > w[0].x));
        assert!(path(&points[..1]).is_empty());
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
