//! The bot's body: a mannequin named after it that everyone sees. The plugin moves it a step a
//! tick with `/tp` (over the ground, a block up or down at most, around nothing: it waits when
//! a wall is in the way and catches up by teleport), lets it hit with `/damage` and `/swing`,
//! and places blocks for it with the world capability, only where there is air or plants.
//!
//! Everything it does comes from the bot's program as fixed orders (`Order`): no command line
//! from outside is ever run, and names and block states are checked first.
use std::collections::VecDeque;

/// Picks the body among all entities (the plugin removes others with the tag on start).
pub const SELECT: &str = "@e[type=minecraft:mannequin,tag=schorsch,limit=1]";
const KILL: &str = "kill @e[type=minecraft:mannequin,tag=schorsch]";
const WALK: f64 = 0.22; // blocks a tick: a player's walking speed
const NEAR: f64 = 2.5; // how close it walks up to whom it follows
const CATCH_UP: f64 = 24.0; // farther than this: it appears next to them
const STUCK_TICKS: u32 = 40;
const REACH: f64 = 3.0;
const HIT_EVERY: u64 = 12; // ticks between two hits (a sword's cooldown)
const ATTACK_TICKS: u32 = 300; // how long it keeps hitting one target (15 s)
const PLAYER_DAMAGE: u32 = 2;
const MOB_DAMAGE: u32 = 4;
pub const MAX_BUILD: usize = 512;
const BUILD_PER_TICK: usize = 3;
const BUILD_REACH: i32 = 24;
/// Blocks it never places: they burn, explode, flood or can't be undone by players.
const FORBIDDEN: &[&str] = &[
    "tnt",
    "lava",
    "fire",
    "soul_fire",
    "command_block",
    "chain_command_block",
    "repeating_command_block",
    "structure_block",
    "structure_void",
    "jigsaw",
    "barrier",
    "light",
    "bedrock",
    "end_portal",
    "end_portal_frame",
    "end_gateway",
    "nether_portal",
    "spawner",
    "trial_spawner",
    "vault",
    "respawn_anchor",
    "reinforced_deepslate",
    "test_block",
    "test_instance_block",
];

/// What the program asks the body to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Order {
    /// Appear next to the player (or walk there when close) and follow them.
    Come(String),
    Remove,
    Follow(String),
    Stay,
    /// Walk this far east (x) and south (z) from where it stands.
    Walk(i16, i16),
    AttackPlayer(String),
    /// The nearest mob of this kind (`zombie`, ...) within reach, for a while.
    AttackMob(String),
    /// Blocks relative to its feet: east, up, south, and the block state.
    Build(Vec<([i8; 3], String)>),
}

/// A player the body may follow or hit.
pub struct Seen {
    pub name: String,
    pub dimension: String,
    pub at: [f64; 3],
    pub yaw: f32,
}

#[derive(Clone, Debug, PartialEq)]
enum Goal {
    Stay,
    Follow(String),
    Walk([f64; 3]),
    Player(String, u32),
    Mob(String, u32),
}

/// What a tick of the body asks the server for.
#[derive(Default, Debug)]
pub struct Out {
    pub commands: Vec<String>,
    /// Blocks to set in the body's dimension: x, y, z, state.
    pub blocks: Vec<([i32; 3], String)>,
}

pub struct Body {
    pub dimension: String,
    pub at: [f64; 3],
    pub yaw: f32,
    goal: Goal,
    stuck: u32,
    builds: VecDeque<([i32; 3], String)>,
    holding: &'static str,
    moved: bool,
}

/// Air, plants and other things one walks through (and that a build may replace).
pub fn passable(state: &str) -> bool {
    let name = state.split('[').next().unwrap_or(state);
    let name = name.strip_prefix("minecraft:").unwrap_or(name);
    matches!(
        name,
        "air"
            | "cave_air"
            | "void_air"
            | "short_grass"
            | "tall_grass"
            | "fern"
            | "large_fern"
            | "dead_bush"
            | "short_dry_grass"
            | "tall_dry_grass"
            | "bush"
            | "firefly_bush"
            | "leaf_litter"
            | "wildflowers"
            | "pink_petals"
            | "snow"
            | "water"
            | "seagrass"
            | "tall_seagrass"
            | "kelp"
            | "kelp_plant"
            | "vine"
            | "glow_lichen"
            | "sugar_cane"
            | "wheat"
            | "carrots"
            | "potatoes"
            | "beetroots"
            | "torch"
            | "wall_torch"
            | "soul_torch"
            | "redstone_wire"
            | "rail"
            | "powered_rail"
            | "detector_rail"
            | "activator_rail"
            | "dandelion"
            | "poppy"
            | "blue_orchid"
            | "allium"
            | "azure_bluet"
            | "oxeye_daisy"
            | "cornflower"
            | "lily_of_the_valley"
            | "torchflower"
            | "brown_mushroom"
            | "red_mushroom"
    ) || [
        "_tulip",
        "_sapling",
        "_carpet",
        "_button",
        "_pressure_plate",
        "_sign",
        "_banner",
    ]
    .iter()
    .any(|end| name.ends_with(end))
}

/// A block state the body may place: `name` or `name[key=value,...]`, no forbidden block.
pub fn allowed_state(state: &str) -> Option<String> {
    let full = if state.contains(':') {
        state.to_owned()
    } else {
        format!("minecraft:{state}")
    };
    let (id, props) = match full.split_once('[') {
        Some((id, props)) => (id, Some(props.strip_suffix(']')?)),
        None => (full.as_str(), None),
    };
    let (space, name) = id.split_once(':')?;
    let word = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    if space != "minecraft" || !word(name) || FORBIDDEN.contains(&name) || full.len() > 120 {
        return None;
    }
    if let Some(props) = props {
        let ok = props
            .split(',')
            .all(|kv| kv.split_once('=').is_some_and(|(k, v)| word(k) && word(v)));
        if !ok {
            return None;
        }
    }
    Some(full)
}

/// A player name as Minecraft allows them.
pub fn player_name(name: &str) -> bool {
    (1..=16).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A mob kind (`zombie`, `minecraft:skeleton`): its id, never a player or the body itself.
pub fn mob_kind(kind: &str) -> Option<String> {
    let name = kind.strip_prefix("minecraft:").unwrap_or(kind);
    let ok = (1..=40).contains(&name.len())
        && name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        && !matches!(name, "player" | "mannequin");
    ok.then(|| format!("minecraft:{name}"))
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// The yaw that looks from `a` to `b`, as Minecraft counts it (0 south, 90 west).
fn yaw_to(a: [f64; 3], b: [f64; 3]) -> f32 {
    (-(b[0] - a[0]).atan2(b[2] - a[2])).to_degrees() as f32
}

/// Where feet can stand near `y` at x/z: solid below, room for body and head; the same height
/// first, then a step up, then down to four blocks.
fn ground(at: [f64; 3], world: &mut impl FnMut([i32; 3]) -> Option<String>) -> Option<f64> {
    let (x, z) = (at[0].floor() as i32, at[2].floor() as i32);
    let feet = (at[1] + 0.01).floor() as i32;
    let free = |s: Option<String>| s.is_some_and(|s| passable(&s));
    for dy in [0, 1, -1, -2, -3, -4] {
        let y = feet + dy;
        let below = world([x, y - 1, z]);
        if below.as_deref().is_some_and(|s| !passable(s))
            && free(world([x, y, z]))
            && free(world([x, y + 1, z]))
        {
            return Some(f64::from(y));
        }
    }
    None
}

fn tp(dimension: &str, at: [f64; 3], yaw: f32) -> String {
    format!(
        "execute in {dimension} run tp {SELECT} {:.2} {:.2} {:.2} {:.1} 0",
        at[0], at[1], at[2], yaw
    )
}

impl Body {
    /// A body two blocks in front of the player, facing them; the commands that make it.
    pub fn new(
        player: &Seen,
        world: &mut impl FnMut([i32; 3]) -> Option<String>,
    ) -> (Self, Vec<String>) {
        let yaw = f64::from(player.yaw).to_radians();
        let front = [
            player.at[0] - yaw.sin() * 2.0,
            player.at[1],
            player.at[2] + yaw.cos() * 2.0,
        ];
        let at = match ground(front, world) {
            Some(y) => [front[0], y, front[2]],
            None => player.at,
        };
        let facing = yaw_to(at, player.at);
        let body = Body {
            dimension: player.dimension.clone(),
            at,
            yaw: facing,
            goal: Goal::Follow(player.name.clone()),
            stuck: 0,
            builds: VecDeque::new(),
            holding: "air",
            moved: false,
        };
        let summon = format!(
            "execute in {} run summon minecraft:mannequin {:.2} {:.2} {:.2} \
             {{profile:\"Schorsch\",CustomName:\"Schorsch\",CustomNameVisible:1b,\
             hide_description:1b,immovable:1b,Invulnerable:1b,Tags:[\"schorsch\"],\
             Rotation:[{:.1}f,0f]}}",
            body.dimension, at[0], at[1], at[2], facing
        );
        (body, vec![KILL.to_owned(), summon])
    }

    /// Takes all bodies away (also ones left from before a restart).
    pub fn remove_all() -> String {
        KILL.to_owned()
    }

    /// What the body does now, for the program to tell Claude.
    pub fn doing(&self) -> String {
        match &self.goal {
            Goal::Stay if !self.builds.is_empty() => "baut".to_owned(),
            Goal::Stay => "steht".to_owned(),
            Goal::Follow(name) => format!("folgt {name}"),
            Goal::Walk(_) => "läuft".to_owned(),
            Goal::Player(name, _) => format!("haut {name}"),
            Goal::Mob(kind, _) => format!("haut {}", kind.trim_start_matches("minecraft:")),
        }
    }

    /// Takes an order (not `Come`/`Remove`, which make or end the body); false when refused.
    pub fn order(&mut self, order: Order) -> bool {
        self.stuck = 0;
        match order {
            Order::Follow(name) => self.goal = Goal::Follow(name),
            Order::Stay => self.goal = Goal::Stay,
            Order::Walk(dx, dz) => {
                self.goal = Goal::Walk([
                    self.at[0] + f64::from(dx),
                    self.at[1],
                    self.at[2] + f64::from(dz),
                ])
            }
            Order::AttackPlayer(name) => self.goal = Goal::Player(name, ATTACK_TICKS),
            Order::AttackMob(kind) => self.goal = Goal::Mob(kind, ATTACK_TICKS),
            Order::Build(blocks) => {
                let base = self.at.map(|v| v.floor() as i32);
                self.builds.extend(
                    blocks
                        .into_iter()
                        .take(MAX_BUILD)
                        .filter(|(d, _)| d.iter().all(|v| i32::from(*v).abs() <= BUILD_REACH))
                        .map(|(d, s)| {
                            (
                                [
                                    base[0] + i32::from(d[0]),
                                    base[1] + i32::from(d[1]),
                                    base[2] + i32::from(d[2]),
                                ],
                                s,
                            )
                        }),
                );
                self.goal = Goal::Stay;
            }
            Order::Come(_) | Order::Remove => return false,
        }
        true
    }

    /// Puts the body next to the player (another dimension, far away, or stuck).
    fn catch_up(
        &mut self,
        player: &Seen,
        world: &mut impl FnMut([i32; 3]) -> Option<String>,
        out: &mut Out,
    ) {
        let side = [player.at[0] + 1.0, player.at[1], player.at[2] + 1.0];
        self.at = ground(side, world).map_or(player.at, |y| [side[0], y, side[2]]);
        self.dimension = player.dimension.clone();
        self.yaw = yaw_to(self.at, player.at);
        self.stuck = 0;
        let _ = out;
    }

    /// A step toward `target`, stopping `stop` blocks before it; false when blocked.
    fn step(
        &mut self,
        target: [f64; 3],
        stop: f64,
        world: &mut impl FnMut([i32; 3]) -> Option<String>,
    ) -> bool {
        let left = distance(self.at, target) - stop;
        self.yaw = yaw_to(self.at, target);
        if left <= 0.0 {
            return true;
        }
        let k = WALK.min(left) / distance(self.at, target);
        let next = [
            self.at[0] + (target[0] - self.at[0]) * k,
            self.at[1],
            self.at[2] + (target[2] - self.at[2]) * k,
        ];
        match ground(next, world) {
            Some(y) => {
                self.at = [next[0], y, next[2]];
                self.moved = true;
                true
            }
            None => false,
        }
    }

    fn hold(&mut self, item: &'static str, out: &mut Out) {
        if self.holding != item {
            self.holding = item;
            out.commands.push(format!(
                "item replace entity {SELECT} weapon.mainhand with minecraft:{item}"
            ));
        }
    }

    pub fn tick(
        &mut self,
        tick: u64,
        players: &[Seen],
        world: &mut impl FnMut([i32; 3]) -> Option<String>,
    ) -> Out {
        let mut out = Out::default();
        let before = (self.at, self.yaw);
        self.moved = false;
        let find = |name: &str| players.iter().find(|p| p.name == name);
        match self.goal.clone() {
            Goal::Stay => {}
            Goal::Follow(name) => match find(&name) {
                Some(p)
                    if p.dimension != self.dimension
                        || distance(self.at, p.at) > CATCH_UP
                        || self.stuck > STUCK_TICKS =>
                {
                    self.catch_up(p, world, &mut out)
                }
                Some(p) => {
                    if !self.step(p.at, NEAR, world) {
                        self.stuck += 1;
                    } else {
                        self.stuck = 0;
                    }
                }
                None => self.goal = Goal::Stay,
            },
            Goal::Walk(target) => {
                if !self.step(target, 0.3, world) {
                    self.stuck += 1;
                }
                if distance(self.at, target) < 0.4 || self.stuck > STUCK_TICKS {
                    self.goal = Goal::Stay;
                }
            }
            Goal::Player(name, left) => match find(&name) {
                Some(p) if left > 0 && p.dimension == self.dimension => {
                    self.goal = Goal::Player(name.clone(), left - 1);
                    self.hold("iron_sword", &mut out);
                    if distance(self.at, p.at) > CATCH_UP {
                        self.catch_up(p, world, &mut out);
                    } else if !self.step(p.at, REACH - 0.8, world) {
                        self.stuck += 1;
                    }
                    if distance(self.at, p.at) <= REACH && tick.is_multiple_of(HIT_EVERY) {
                        out.commands.push(format!("swing {SELECT} mainhand"));
                        out.commands.push(format!(
                            "damage {name} {PLAYER_DAMAGE} minecraft:mob_attack by {SELECT}"
                        ));
                    }
                }
                _ => self.goal = Goal::Stay,
            },
            Goal::Mob(kind, left) => {
                if left == 0 {
                    self.goal = Goal::Stay;
                } else {
                    self.goal = Goal::Mob(kind.clone(), left - 1);
                    self.hold("iron_sword", &mut out);
                    if tick.is_multiple_of(HIT_EVERY) {
                        let near =
                            format!("@e[type={kind},distance=..{REACH},sort=nearest,limit=1]");
                        out.commands.push(format!(
                            "execute as {SELECT} at @s if entity {near} run rotate @s facing entity {near} eyes"
                        ));
                        out.commands.push(format!(
                            "execute as {SELECT} at @s if entity {near} run swing @s mainhand"
                        ));
                        out.commands.push(format!(
                            "execute as {SELECT} at @s run damage {near} {MOB_DAMAGE} minecraft:mob_attack by @s"
                        ));
                    }
                }
            }
        }
        if !matches!(self.goal, Goal::Player(..) | Goal::Mob(..)) && self.builds.is_empty() {
            self.hold("air", &mut out);
        }
        self.build(world, &mut out);
        let turned = (self.yaw - before.1).abs() > 2.0;
        if !matches!(self.goal, Goal::Mob(..)) && (self.at != before.0 || turned) {
            out.commands.push(tp(&self.dimension, self.at, self.yaw));
        }
        out
    }

    /// Places a few of the asked blocks, where nothing solid is; swings and faces them.
    fn build(&mut self, world: &mut impl FnMut([i32; 3]) -> Option<String>, out: &mut Out) {
        let mut placed = 0;
        while placed < BUILD_PER_TICK {
            let Some((at, state)) = self.builds.pop_front() else {
                return;
            };
            if !world(at).is_some_and(|s| passable(&s)) {
                continue; // something is there already: never replaced
            }
            if placed == 0 {
                let item = state
                    .split('[')
                    .next()
                    .unwrap_or("air")
                    .trim_start_matches("minecraft:");
                out.commands.push(format!("swing {SELECT} mainhand"));
                out.commands.push(format!(
                    "item replace entity {SELECT} weapon.mainhand with minecraft:{item}"
                ));
                self.holding = "";
                self.yaw = yaw_to(
                    self.at,
                    [f64::from(at[0]) + 0.5, 0.0, f64::from(at[2]) + 0.5],
                );
            }
            out.blocks.push((at, state));
            placed += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flat stone at y 63, air above, a wall at x 5 (y 64-65) and a step at x -3.
    fn flat(at: [i32; 3]) -> Option<String> {
        let state = if at[1] <= 63 || (at[0] == 5 && at[1] <= 65) || (at[0] == -3 && at[1] == 64) {
            "minecraft:stone"
        } else {
            "minecraft:air"
        };
        Some(state.to_owned())
    }

    fn alex(x: f64, z: f64) -> Seen {
        Seen {
            name: "Alex".into(),
            dimension: "minecraft:overworld".into(),
            at: [x, 64.0, z],
            yaw: 0.0,
        }
    }

    #[test]
    fn it_appears_in_front_of_the_player_and_follows_over_steps() {
        let (mut body, made) = Body::new(&alex(0.5, 0.5), &mut flat);
        assert_eq!(made[0], KILL);
        assert!(
            made[1].contains("summon minecraft:mannequin 0.50 64.00 2.50"),
            "{}",
            made[1]
        );
        assert!(made[1].contains("Tags:[\"schorsch\"]"));
        // Alex walks west past the step: the body follows, a block up and down again.
        let mut heights = vec![];
        for tick in 0..200 {
            let out = body.tick(tick, &[alex(-8.5, 0.5)], &mut flat);
            assert!(out.commands.iter().all(|c| !c.contains("damage")));
            heights.push(body.at[1]);
        }
        assert!(heights.contains(&65.0));
        assert_eq!(body.at[1], 64.0);
        assert!(distance(body.at, [-8.5, 64.0, 0.5]) <= NEAR + 0.01);
        assert_eq!(body.doing(), "folgt Alex");
    }

    #[test]
    fn a_wall_stops_it_and_then_it_catches_up_by_teleport() {
        let (mut body, _) = Body::new(&alex(0.5, 0.5), &mut flat);
        let far = alex(12.5, 2.5); // behind the wall at x 5 (two high: no step)
        let mut teleported = false;
        for tick in 0..120 {
            let out = body.tick(tick, &[far.clone_seen()], &mut flat);
            teleported |= body.at[0] > 6.0;
            assert!(body.at[0] < 5.0 || teleported, "walked through the wall");
            let _ = out;
        }
        assert!(teleported);
    }

    #[test]
    fn it_builds_only_into_air_and_hits_players_in_reach() {
        let (mut body, _) = Body::new(&alex(0.5, 0.5), &mut flat);
        body.order(Order::Stay);
        body.order(Order::Build(vec![
            ([1, 0, 0], "minecraft:oak_planks".into()),
            ([0, -1, 0], "minecraft:glass".into()), // the stone under its feet stays
            ([1, 1, 0], "minecraft:oak_planks".into()),
        ]));
        let out = body.tick(1, &[alex(0.5, 0.5)], &mut flat);
        let at = body.at.map(|v| v.floor() as i32);
        assert_eq!(
            out.blocks,
            vec![
                ([at[0] + 1, at[1], at[2]], "minecraft:oak_planks".to_owned()),
                (
                    [at[0] + 1, at[1] + 1, at[2]],
                    "minecraft:oak_planks".to_owned()
                ),
            ]
        );
        assert!(out.commands.iter().any(|c| c.starts_with("swing ")));
        body.order(Order::AttackPlayer("Alex".into()));
        let hits: usize = (0..48)
            .map(|t| {
                body.tick(t, &[alex(0.5, 0.5)], &mut flat)
                    .commands
                    .iter()
                    .filter(|c| c.starts_with("damage Alex 2"))
                    .count()
            })
            .sum();
        assert_eq!(hits, 4);
    }

    #[test]
    fn states_names_and_kinds_are_checked() {
        assert_eq!(
            allowed_state("oak_stairs[facing=east,half=top]").as_deref(),
            Some("minecraft:oak_stairs[facing=east,half=top]")
        );
        assert_eq!(allowed_state("minecraft:tnt"), None);
        assert_eq!(allowed_state("lava"), None);
        assert_eq!(allowed_state("stone{x:1}"), None);
        assert_eq!(allowed_state("stone] run kill @a"), None);
        assert_eq!(allowed_state("other:stone"), None);
        assert!(player_name("Qaroq_1"));
        assert!(!player_name("@a"));
        assert!(!player_name("a b"));
        assert_eq!(mob_kind("zombie").as_deref(), Some("minecraft:zombie"));
        assert_eq!(mob_kind("player"), None);
        assert_eq!(mob_kind("zombie,limit=99"), None);
    }

    impl Seen {
        fn clone_seen(&self) -> Seen {
            Seen {
                name: self.name.clone(),
                dimension: self.dimension.clone(),
                at: self.at,
                yaw: self.yaw,
            }
        }
    }
}
