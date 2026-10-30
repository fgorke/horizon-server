//! Horizon Server plugin (`docs/plugins.md`): spawn elytra. In the region an operator sets with
//! `/spawnelytra pos1|pos2`, a survival or adventure player who falls off (more than [`FALL`]
//! blocks) or double-jumps glides without an elytra, boosts once along their look with the
//! swap-hands key (F) and lands without fall damage; the glide goes on past the region until they
//! land. The region is kept in `storage` key `region`: a line `dimension <id>`, then `pos1 x y z`
//! and `pos2 x y z` for the corners set.
use horizon_mod_sdk::{
    server::{Args, Argument, GameMode, Player, Registration, ServerMod, movement, players},
    storage,
};
use std::collections::BTreeMap;

/// The fall, in blocks, after which a player in the region starts gliding: more than a jump's.
const FALL: f64 = 1.5;
/// The boost's speed along the look, in blocks per tick.
const BOOST: f64 = 2.0;
/// The permission level /spawnelytra needs: an operator's.
const OPERATOR: u8 = 2;
const KEY: &str = "region";

/// The region's dimension and corners, as they were set.
#[derive(Clone, Debug, Default, PartialEq)]
struct Region {
    dimension: Option<String>,
    pos1: Option<[i32; 3]>,
    pos2: Option<[i32; 3]>,
}

impl Region {
    /// Whether `at` in `dimension` is in the region, both corners' blocks included.
    fn contains(&self, dimension: &str, at: [f64; 3]) -> bool {
        let (Some(a), Some(b), Some(dim)) = (self.pos1, self.pos2, self.dimension.as_deref())
        else {
            return false;
        };
        dim == dimension
            && (0..3).all(|k| {
                let (lo, hi) = (a[k].min(b[k]), a[k].max(b[k]));
                at[k] >= f64::from(lo) && at[k] < f64::from(hi) + 1.0
            })
    }

    fn parse(text: &str) -> Region {
        let mut region = Region::default();
        for line in text.lines() {
            let mut words = line.split_whitespace();
            let corner = |words: &mut std::str::SplitWhitespace| -> Option<[i32; 3]> {
                let mut n = || words.next()?.parse::<i32>().ok();
                Some([n()?, n()?, n()?])
            };
            match words.next() {
                Some("dimension") => region.dimension = words.next().map(str::to_owned),
                Some("pos1") => region.pos1 = corner(&mut words),
                Some("pos2") => region.pos2 = corner(&mut words),
                _ => {}
            }
        }
        region
    }

    fn text(&self) -> String {
        let mut out = String::new();
        if let Some(d) = &self.dimension {
            out += &format!("dimension {d}\n");
        }
        for (name, p) in [("pos1", self.pos1), ("pos2", self.pos2)] {
            if let Some([x, y, z]) = p {
                out += &format!("{name} {x} {y} {z}\n");
            }
        }
        out
    }
}

/// What the plugin knows of a player.
#[derive(Default)]
struct Glider {
    /// They double-jumped since the last tick.
    jumped: bool,
    /// They glide in a glide this plugin started, and used its boost.
    gliding: bool,
    boosted: bool,
    /// Double jumps are allowed, as last told.
    may_jump: bool,
}

struct Elytra {
    region: Region,
    /// By player id.
    gliders: BTreeMap<String, Glider>,
}

/// The unit look vector of a yaw and pitch in degrees (`Entity.calculateViewVector`).
fn look(yaw: f32, pitch: f32) -> [f64; 3] {
    let (yaw, pitch) = (f64::from(yaw).to_radians(), f64::from(pitch).to_radians());
    [
        -yaw.sin() * pitch.cos(),
        -pitch.sin(),
        yaw.cos() * pitch.cos(),
    ]
}

fn block(p: &Player) -> [i32; 3] {
    [p.x, p.y, p.z].map(|c| c.floor() as i32)
}

impl Elytra {
    fn save(&self) -> Result<(), String> {
        storage::set(KEY, self.region.text().as_bytes())
            .map_err(|e| format!("Could not save the region: {e}"))
    }

    fn run_command(
        &mut self,
        player: &Player,
        action: Option<&str>,
    ) -> Result<Option<String>, String> {
        if players::permission(&player.id).unwrap_or(0) < OPERATOR {
            return Err("You need to be an operator for that.".into());
        }
        match action {
            Some(corner @ ("pos1" | "pos2")) => {
                if self.region.dimension.as_deref() != Some(player.dimension.as_str()) {
                    // Corners of another dimension don't make a region with this one.
                    self.region = Region {
                        dimension: Some(player.dimension.clone()),
                        ..Region::default()
                    };
                }
                let at = block(player);
                if corner == "pos1" {
                    self.region.pos1 = Some(at);
                } else {
                    self.region.pos2 = Some(at);
                }
                self.save()?;
                Ok(Some(format!("{corner} set: {} {} {}", at[0], at[1], at[2])))
            }
            Some("off") => {
                self.region = Region::default();
                self.save()?;
                Ok(Some("Spawn elytra region removed.".into()))
            }
            None => {
                let show = |p: Option<[i32; 3]>| {
                    p.map_or("not set".to_owned(), |[x, y, z]| format!("{x} {y} {z}"))
                };
                Ok(Some(format!(
                    "Pos1: {} | Pos2: {} (/spawnelytra pos1|pos2|off)",
                    show(self.region.pos1),
                    show(self.region.pos2)
                )))
            }
            Some(_) => Err("Usage: /spawnelytra <pos1|pos2|off>".into()),
        }
    }

    /// One tick of a player: a fall or a double jump in the region starts a glide, a landing
    /// ends it, and double jumps are allowed in the region outside a glide.
    fn tick_player(&mut self, p: &Player) {
        let Some(m) = movement::motion_of(&p.id) else {
            return;
        };
        let survival = matches!(p.game_mode, GameMode::Survival | GameMode::Adventure);
        let near = survival && self.region.contains(&p.dimension, [p.x, p.y, p.z]);
        let g = self.gliders.entry(p.id.clone()).or_default();
        let jumped = std::mem::take(&mut g.jumped);
        let started = (jumped || m.fall_distance > FALL)
            && near
            && !m.on_ground
            && !m.riding
            && !m.gliding
            && !g.gliding;
        if started && movement::glide(&p.id) {
            g.gliding = true;
            g.boosted = false;
            players::action_bar(&p.id, "§bPress F for a boost");
        } else if g.gliding && (m.on_ground || !m.gliding || !survival) {
            g.gliding = false;
        }
        let may_jump = near && !g.gliding;
        if may_jump != g.may_jump && movement::allow_double_jump(&p.id, may_jump) {
            g.may_jump = may_jump;
        }
    }
}

impl ServerMod for Elytra {
    fn init() -> Result<(Self, Registration), String> {
        let region = storage::get(KEY)
            .map(|b| Region::parse(&String::from_utf8_lossy(&b)))
            .unwrap_or_default();
        let hooks = Registration::new()
            .command_with("spawnelytra", [Argument::word("action").optional()])
            .on_tick()
            .on_joins();
        Ok((
            Elytra {
                region,
                gliders: BTreeMap::new(),
            },
            hooks,
        ))
    }

    fn tick(&mut self, _tick: u64) {
        for p in players::online() {
            self.tick_player(&p);
        }
    }

    fn player_left(&mut self, player: &Player) {
        self.gliders.remove(&player.id);
    }

    fn command(
        &mut self,
        player: &Player,
        _name: &str,
        args: &Args,
    ) -> Result<Option<String>, String> {
        self.run_command(player, args.string("action"))
    }

    fn double_jump(&mut self, player: &Player) {
        self.gliders.entry(player.id.clone()).or_default().jumped = true;
    }

    /// The one boost of a glide, along the look, in place of the swap.
    fn swap_hands(&mut self, player: &Player) -> bool {
        let Some(g) = self.gliders.get_mut(&player.id) else {
            return false;
        };
        if !g.gliding || g.boosted {
            return false;
        }
        g.boosted = true;
        let (yaw, pitch) = players::rotation(&player.id).unwrap_or((0.0, 0.0));
        let [x, y, z] = look(yaw, pitch).map(|c| c * BOOST);
        movement::push(&player.id, x, y, z);
        true
    }
}

horizon_mod_sdk::export_server_mod!(Elytra);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_look_follows_yaw_and_pitch() {
        let close = |a: [f64; 3], b: [f64; 3]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9);
        assert!(close(look(0.0, 0.0), [0.0, 0.0, 1.0]));
        assert!(close(look(90.0, 0.0), [-1.0, 0.0, 0.0]));
        assert!(close(look(0.0, -90.0), [0.0, 1.0, 0.0]));
    }

    #[test]
    fn the_region_takes_both_corner_blocks_in_its_dimension() {
        let r = Region {
            dimension: Some("minecraft:overworld".into()),
            pos1: Some([10, 200, -5]),
            pos2: Some([-10, 150, 5]),
        };
        assert!(r.contains("minecraft:overworld", [10.9, 200.9, 5.9]));
        assert!(r.contains("minecraft:overworld", [-10.0, 150.0, -5.0]));
        assert!(!r.contains("minecraft:overworld", [11.0, 180.0, 0.0]));
        assert!(!r.contains("minecraft:the_nether", [0.0, 180.0, 0.0]));
        assert!(!Region::default().contains("minecraft:overworld", [0.0; 3]));
        assert_eq!(Region::parse(&r.text()), r);
    }
}
