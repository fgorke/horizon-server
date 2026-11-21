//! What a player sees, for the bot: rays from their eyes through the loaded world, a few hundred
//! a tick, and per pixel the block hit, its face, where on the face and how far. The bot's
//! program paints the picture from that with the block textures.
use std::collections::HashMap;

pub const WIDTH: u16 = 160;
pub const HEIGHT: u16 = 90;
/// Minecraft's default field of view (vertical), degrees.
const FOV: f64 = 70.0;
const MAX_DISTANCE: f64 = 96.0;
pub const RAYS_PER_TICK: usize = 500;
const EYE_HEIGHT: f64 = 1.62;
/// What a ray hit nowhere (sky, or a chunk the server hasn't loaded).
pub const NOTHING: u16 = u16::MAX;
/// The palette's size at most; rarer blocks after that show as nothing.
const MAX_PALETTE: usize = 2000;

/// Blocks rays pass through: air, and small things that would hide what is behind them.
fn see_through(state: &str) -> bool {
    let name = state.split('[').next().unwrap_or(state);
    let name = name.strip_prefix("minecraft:").unwrap_or(name);
    matches!(
        name,
        "air"
            | "cave_air"
            | "void_air"
            | "light"
            | "barrier"
            | "structure_void"
            | "short_grass"
            | "tall_grass"
            | "fern"
            | "large_fern"
            | "dead_bush"
            | "torch"
            | "wall_torch"
            | "soul_torch"
            | "soul_wall_torch"
            | "redstone_torch"
            | "redstone_wall_torch"
            | "redstone_wire"
            | "tripwire"
            | "rail"
            | "powered_rail"
            | "detector_rail"
            | "activator_rail"
            | "glass_pane"
            | "short_dry_grass"
            | "tall_dry_grass"
    )
}

/// One pixel: the palette index of the block hit (`NOTHING` for none), the face (0 down, 1 up,
/// 2 north, 3 south, 4 west, 5 east), where on the face (0-255 each way) and the distance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pixel {
    pub block: u16,
    pub face: u8,
    pub u: u8,
    pub v: u8,
    pub distance: u8,
}

impl Pixel {
    const SKY: Pixel = Pixel {
        block: NOTHING,
        face: 0,
        u: 0,
        v: 0,
        distance: 255,
    };
}

pub struct View {
    dimension: String,
    eye: [f64; 3],
    forward: [f64; 3],
    right: [f64; 3],
    up: [f64; 3],
    next: usize,
    pub pixels: Vec<Pixel>,
    pub palette: Vec<String>,
    index: HashMap<String, u16>,
    cache: HashMap<[i32; 3], Cell>,
}

#[derive(Clone, Copy, PartialEq)]
enum Cell {
    Block(u16),
    Clear,
    Unloaded,
}

fn normalize(v: [f64; 3]) -> [f64; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    [v[0] / len, v[1] / len, v[2] / len]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

impl View {
    /// A view from a player's feet at `at`, looking `yaw`/`pitch` degrees as Minecraft counts
    /// them (yaw 0 south, 90 west; pitch 90 down).
    pub fn new(dimension: &str, at: [f64; 3], yaw: f32, pitch: f32) -> Self {
        let (yaw, pitch) = (f64::from(yaw).to_radians(), f64::from(pitch).to_radians());
        let forward = normalize([
            -yaw.sin() * pitch.cos(),
            -pitch.sin(),
            yaw.cos() * pitch.cos(),
        ]);
        // Straight up or down: the yaw still says which way is right.
        let right = if forward[1].abs() > 0.999 {
            [-yaw.cos(), 0.0, -yaw.sin()]
        } else {
            normalize(cross(forward, [0.0, 1.0, 0.0]))
        };
        let up = cross(right, forward);
        View {
            dimension: dimension.to_owned(),
            eye: [at[0], at[1] + EYE_HEIGHT, at[2]],
            forward,
            right,
            up,
            next: 0,
            pixels: Vec::with_capacity(usize::from(WIDTH) * usize::from(HEIGHT)),
            palette: Vec::new(),
            index: HashMap::new(),
            cache: HashMap::new(),
        }
    }

    pub fn dimension(&self) -> &str {
        &self.dimension
    }

    pub fn done(&self) -> bool {
        self.next >= usize::from(WIDTH) * usize::from(HEIGHT)
    }

    fn direction(&self, pixel: usize) -> [f64; 3] {
        let (w, h) = (f64::from(WIDTH), f64::from(HEIGHT));
        let tan_v = (FOV.to_radians() / 2.0).tan();
        let tan_h = tan_v * w / h;
        let px = (pixel % usize::from(WIDTH)) as f64;
        let py = (pixel / usize::from(WIDTH)) as f64;
        let x = (2.0 * (px + 0.5) / w - 1.0) * tan_h;
        let y = (1.0 - 2.0 * (py + 0.5) / h) * tan_v;
        normalize([
            self.forward[0] + x * self.right[0] + y * self.up[0],
            self.forward[1] + x * self.right[1] + y * self.up[1],
            self.forward[2] + x * self.right[2] + y * self.up[2],
        ])
    }

    /// What is at a cell: a block (its palette index), see-through, or not loaded.
    fn block(&mut self, at: [i32; 3], world: &mut impl FnMut([i32; 3]) -> Option<String>) -> Cell {
        if let Some(cell) = self.cache.get(&at) {
            return *cell;
        }
        let cell = match world(at) {
            None => Cell::Unloaded,
            Some(state) if see_through(&state) => Cell::Clear,
            Some(state) => match self.index.get(&state) {
                Some(i) => Cell::Block(*i),
                None if self.palette.len() >= MAX_PALETTE => Cell::Clear,
                None => {
                    let i = self.palette.len() as u16;
                    self.index.insert(state.clone(), i);
                    self.palette.push(state);
                    Cell::Block(i)
                }
            },
        };
        self.cache.insert(at, cell);
        cell
    }

    /// Casts up to `rays` more rays; `world` answers the block at a cell (none: unloaded).
    pub fn cast(&mut self, rays: usize, world: &mut impl FnMut([i32; 3]) -> Option<String>) {
        let end = (self.next + rays).min(usize::from(WIDTH) * usize::from(HEIGHT));
        while self.next < end {
            let d = self.direction(self.next);
            let pixel = self.ray(d, world);
            self.pixels.push(pixel);
            self.next += 1;
        }
    }

    /// Amanatides-Woo through the cells until a block, the distance or an unloaded chunk.
    fn ray(&mut self, d: [f64; 3], world: &mut impl FnMut([i32; 3]) -> Option<String>) -> Pixel {
        let o = self.eye;
        let mut cell = [
            o[0].floor() as i32,
            o[1].floor() as i32,
            o[2].floor() as i32,
        ];
        let mut step = [0i32; 3];
        let mut t_max = [f64::INFINITY; 3];
        let mut t_delta = [f64::INFINITY; 3];
        for k in 0..3 {
            if d[k] > 0.0 {
                step[k] = 1;
                t_max[k] = (f64::from(cell[k]) + 1.0 - o[k]) / d[k];
                t_delta[k] = 1.0 / d[k];
            } else if d[k] < 0.0 {
                step[k] = -1;
                t_max[k] = (o[k] - f64::from(cell[k])) / -d[k];
                t_delta[k] = -1.0 / d[k];
            }
        }
        let mut t = 0.0;
        let mut axis = 1;
        while t <= MAX_DISTANCE {
            let here = if t > 0.0 {
                self.block(cell, world)
            } else {
                Cell::Clear // the eye's own cell
            };
            if here == Cell::Unloaded {
                return Pixel::SKY;
            }
            if let Cell::Block(block) = here {
                let hit = [o[0] + d[0] * t, o[1] + d[1] * t, o[2] + d[2] * t];
                let frac = |v: f64| ((v - v.floor()) * 255.0) as u8;
                // The face the ray came through, and the two other axes as u/v on it.
                let (face, u, v) = match axis {
                    0 => (
                        if step[0] > 0 { 4 } else { 5 },
                        frac(hit[2]),
                        255 - frac(hit[1]),
                    ),
                    1 => (if step[1] > 0 { 0 } else { 1 }, frac(hit[0]), frac(hit[2])),
                    _ => (
                        if step[2] > 0 { 2 } else { 3 },
                        frac(hit[0]),
                        255 - frac(hit[1]),
                    ),
                };
                return Pixel {
                    block,
                    face,
                    u,
                    v,
                    distance: t.min(254.0) as u8,
                };
            }
            axis = if t_max[0] < t_max[1] {
                if t_max[0] < t_max[2] { 0 } else { 2 }
            } else if t_max[1] < t_max[2] {
                1
            } else {
                2
            };
            t = t_max[axis];
            t_max[axis] += t_delta[axis];
            cell[axis] += step[axis];
        }
        Pixel::SKY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_with(blocks: &[([i32; 3], &str)]) -> impl FnMut([i32; 3]) -> Option<String> {
        let blocks: Vec<([i32; 3], String)> =
            blocks.iter().map(|(p, s)| (*p, (*s).to_owned())).collect();
        move |at| {
            Some(
                blocks
                    .iter()
                    .find(|(p, _)| *p == at)
                    .map_or("minecraft:air".to_owned(), |(_, s)| s.clone()),
            )
        }
    }

    fn center(view: &View) -> Pixel {
        view.pixels[usize::from(HEIGHT / 2) * usize::from(WIDTH) + usize::from(WIDTH / 2)]
    }

    #[test]
    fn looking_south_the_middle_ray_hits_the_block_in_front() {
        // Feet at 0.5 64 0.5: eyes at y 65.62; a stone wall at z 5 in front, y 65.
        let mut world = world_with(&[([0, 65, 5], "minecraft:stone")]);
        let mut view = View::new("minecraft:overworld", [0.5, 64.0, 0.5], 0.0, 0.0);
        while !view.done() {
            view.cast(RAYS_PER_TICK, &mut world);
        }
        let p = center(&view);
        assert_eq!(view.palette[usize::from(p.block)], "minecraft:stone");
        assert_eq!(p.face, 2); // its north face, toward the player
        assert_eq!(p.distance, 4);
    }

    #[test]
    fn west_is_yaw_90_and_down_is_pitch_90() {
        let mut world = world_with(&[
            ([-3, 65, 0], "minecraft:oak_planks"),
            ([0, 62, 0], "minecraft:grass_block[snowy=false]"),
        ]);
        let mut view = View::new("minecraft:overworld", [0.5, 64.0, 0.5], 90.0, 0.0);
        view.cast(usize::MAX, &mut world);
        let p = center(&view);
        assert_eq!(view.palette[usize::from(p.block)], "minecraft:oak_planks");
        assert_eq!(p.face, 5);
        let mut down = View::new("minecraft:overworld", [0.5, 64.0, 0.5], 0.0, 90.0);
        down.cast(usize::MAX, &mut world);
        let p = center(&down);
        assert_eq!(
            down.palette[usize::from(p.block)],
            "minecraft:grass_block[snowy=false]"
        );
        assert_eq!(p.face, 1);
    }

    #[test]
    fn grass_and_air_are_seen_through_and_the_sky_is_nothing() {
        let mut world = world_with(&[
            ([0, 65, 2], "minecraft:short_grass"),
            ([0, 65, 4], "minecraft:glass"),
        ]);
        let mut view = View::new("minecraft:overworld", [0.5, 64.0, 0.5], 0.0, 0.0);
        view.cast(usize::MAX, &mut world);
        assert_eq!(
            view.palette[usize::from(center(&view).block)],
            "minecraft:glass"
        );
        let mut sky = View::new("minecraft:overworld", [0.5, 64.0, 0.5], 0.0, -90.0);
        sky.cast(usize::MAX, &mut world_with(&[]));
        assert!(sky.pixels.iter().all(|p| p.block == NOTHING));
        // Unloaded chunks stop a ray as nothing.
        let mut none = View::new("minecraft:overworld", [0.5, 64.0, 0.5], 0.0, 0.0);
        none.cast(10, &mut |_| None);
        assert!(none.pixels.iter().all(|p| p.block == NOTHING));
    }
}
