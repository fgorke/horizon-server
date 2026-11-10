//! Horizon Server plugin (`docs/plugins.md`): a cutscene for each player the first time they
//! join, which only they see while everyone else plays on. The spawn island floats in the sky
//! over the crater it was torn from; in the intro it is still in the ground. The camera flies a
//! recorded path, recorded figures walk (one of them the player, in their own skin), then the
//! ground rumbles and the island tears loose and rises, carrying the camera and whoever stands
//! on it up to where it floats. The blocks, displays, figures, dust and sounds go to that
//! player alone (capability `cinematic`): vanilla packets, no client mod.
//!
//! Operators set it up in game: `/intro build <lift>` builds the island from the schematic in
//! storage key `schematic` (`ground <y>`, then `x y z state` a block, `L x y z state` the
//! crater's lining) where they stand, the crater under them and the island `lift` blocks up,
//! and `/intro unbuild` puts back what it replaced (or `/intro island pos1|pos2`, `drop`,
//! `capture` takes one already built). The camera flies through points set one by one
//! (`/intro camera add [ticks]` where the operator is, `set`, `remove`, `points`), a curve
//! through them all, or as it was flown (`/intro camera record`); `/intro stage` shows the
//! operator the island in the ground, as the intro starts, to set the points by. `/intro npc
//! record <name|self>` records a figure while the intro plays around it, with the intro's
//! timeline as a bossbar; `/intro stop` ends a recording; `/intro rip <tick> <rise ticks>`
//! times the tearing loose.
mod build;
mod scene;
mod show;

use build::{Build, UNDO, Unbuild};
use horizon_mod_sdk::{
    command,
    messages::Messages,
    server::{Player, Registration, ServerMod, cinematic, players, world},
    storage,
};
use scene::{
    Footprint, Island, Point, Pose, Settings, is_air, parse_points, parse_track, path, point_tick,
    points_text, track_text,
};
use show::{BLOCKS_PER_TICK, Budget, CALLS_PER_TICK, EYES, Mode, Npc, Scene, Show};
use std::collections::{BTreeSet, VecDeque};

const OPERATOR: u8 = 2;
const SETTINGS: &str = "settings";
pub(crate) const ISLAND: &str = "island";
const SCHEMATIC: &str = "schematic";
const CAMERA: &str = "camera";
/// The camera's points, when it was set by points.
const POINTS: &str = "camera-points";
const MAX_POINTS: usize = 64;
/// Ticks from a camera point to the next, unless given.
const POINT_TICKS: u32 = 60;
const SEEN: &str = "seen";
/// The figures' names, a line each.
const NPCS: &str = "npcs";
/// A figure's storage key: `npc-<name>`.
const NPC: &str = "npc-";
const MAX_NAME: usize = 16;
const MAX_NPCS: usize = 32;
/// A recording's ticks: five minutes.
pub(crate) const MAX_FRAMES: usize = 6000;
/// The island's blocks and the box `capture` reads.
pub(crate) const MAX_ISLAND: usize = 32768;
const MAX_BOX: i64 = 128 * 128 * 128;
/// Ticks between a join and the intro (the player's chunks arrive meanwhile, blinded), and
/// before a show (the island's blocks go out meanwhile).
const JOIN_WAIT: u32 = 100;
const PLAY_WAIT: u32 = 10;

const MESSAGES: &[(&str, &str)] = &[
    ("not_operator", "You need to be an operator for that."),
    (
        "usage",
        "/intro status | play [player] | stop | skip | reset <player|all> | build <lift> | unbuild | island pos1|pos2|drop <blocks>|capture | rip <tick> <rise ticks> | shake <blocks> | stage | camera add [ticks]|set <n>|remove <n>|points|record|delete | npc record <name|self> | npc delete <name> | npc list",
    ),
    ("pos", "Island corner {which} set to {x} {y} {z}."),
    (
        "drop",
        "The island starts {blocks} blocks lower, in the ground.",
    ),
    (
        "rip",
        "The island tears loose at tick {tick} and rises for {rise} ticks.",
    ),
    ("shake", "The camera shakes {blocks} blocks."),
    ("no_box", "Set /intro island pos1 and pos2 first."),
    ("big_box", "That box is too big: at most 128 x 128 x 128."),
    (
        "unloaded",
        "Part of the island or the ground under it isn't loaded: go near it and try again.",
    ),
    ("big_island", "The island has over 32768 blocks."),
    (
        "captured",
        "Captured {count} island blocks, and the ground {drop} blocks under them.",
    ),
    (
        "no_schematic",
        "There is no schematic: put one in the plugin's data/schematic.",
    ),
    ("bad_build", "Can't build there: {why}."),
    (
        "building",
        "Building the island from the schematic over the next ticks...",
    ),
    (
        "built",
        "The island stands, {count} blocks, {drop} blocks over its crater. Protect x {x1}..{x2}, y {y1}..{y2}, z {z1}..{z2}, and set the world spawn on it.",
    ),
    ("build_failed", "Building the island failed: {error}"),
    (
        "busy_build",
        "An island is being built or taken down already.",
    ),
    (
        "built_already",
        "There is a built island: /intro unbuild takes it down first.",
    ),
    ("no_undo", "There is no built island to take down."),
    (
        "unbuilding",
        "Taking the island down and putting back what it replaced...",
    ),
    (
        "unbuilt",
        "The island is gone: {count} blocks are back as they were.",
    ),
    ("unbuild_failed", "Taking the island down failed: {error}"),
    ("not_ready", "The intro isn't ready: {missing}."),
    ("playing", "Playing the intro for {name}."),
    ("busy", "{name} is in an intro already."),
    ("not_on", "No player {name} is on."),
    ("recording", "Recording {what}: /intro stop ends it."),
    ("saved", "Saved {what}: {ticks} ticks."),
    ("too_short", "Nothing recorded."),
    ("stopped", "Stopped."),
    ("nothing", "You're in no intro."),
    ("reset", "{name} sees the intro again on their next join."),
    (
        "reset_all",
        "Everyone sees the intro again on their next join.",
    ),
    ("deleted", "Deleted {what}."),
    ("unknown_npc", "There is no figure called {name}."),
    (
        "bad_name",
        "A figure's name is 1 to 16 letters, digits and _, or self.",
    ),
    ("too_many", "There are 32 figures already."),
    ("npcs", "Figures: {list}"),
    ("no_npcs", "No figures yet."),
    (
        "point_added",
        "Camera point {n} set here, {ticks} ticks after the one before. The camera runs {total} ticks.",
    ),
    ("point_set", "Camera point {n} moved here."),
    (
        "point_removed",
        "Camera point {n} removed. The camera runs {total} ticks.",
    ),
    ("no_point", "There is no camera point {n}."),
    ("points", "Camera points: {list}"),
    (
        "no_points",
        "No camera points yet: /intro camera add [ticks] sets one where you are.",
    ),
    (
        "too_long",
        "That is too many points or ticks: at most 64 points and 6000 ticks.",
    ),
    ("no_island", "There is no island yet."),
    (
        "staged_on",
        "You see the island in the ground, as the intro starts. /intro stage again shows it as it is.",
    ),
    ("staged_off", "You see the island as it is again."),
    (
        "bar",
        "{what} · {time} s · tears loose at {rip} s · up at {up} s",
    ),
    (
        "status",
        "Island: {island}. Camera: {camera}. Figures: {npcs}. Tears loose at tick {rip}, rises {rise} ticks, {drop} blocks.",
    ),
];

struct Intro {
    settings: Settings,
    island: Option<Island>,
    footprint: Footprint,
    surface: Vec<usize>,
    camera: Vec<Pose>,
    /// The points the camera's path was made from, if it was.
    points: Vec<Point>,
    npcs: Vec<Npc>,
    /// Operators who see the island in the ground (`/intro stage`).
    staged: BTreeSet<String>,
    seen: BTreeSet<String>,
    shows: Vec<Show>,
    /// Blocks still to set right for players whose show ended.
    restores: Vec<(String, VecDeque<Vec<cinematic::GhostBlock>>)>,
    build: Option<Build>,
    unbuild: Option<Unbuild>,
    messages: Messages,
}

fn load(key: &str) -> Option<String> {
    storage::get(key).map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn save(key: &str, text: &str) {
    if let Err(e) = storage::set(key, text.as_bytes()) {
        horizon_mod_sdk::warn(&format!("intro: saving {key}: {e}"));
    }
}

fn npc_name_ok(name: &str) -> bool {
    name == "self"
        || (1..=MAX_NAME).contains(&name.len())
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn mode_name(mode: horizon_mod_sdk::server::GameMode) -> &'static str {
    use horizon_mod_sdk::server::GameMode;
    match mode {
        GameMode::Survival => "survival",
        GameMode::Creative => "creative",
        GameMode::Adventure => "adventure",
        GameMode::Spectator => "spectator",
    }
}

fn find_player(name: &str) -> Option<Player> {
    players::online()
        .into_iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
}

impl Intro {
    fn say(&self, key: &str, values: &[(&str, &dyn std::fmt::Display)]) -> String {
        self.messages.get(key, values)
    }

    fn scene(&self) -> Scene<'_> {
        Scene {
            settings: &self.settings,
            island: self.island.as_ref(),
            footprint: self.footprint,
            surface: &self.surface,
            camera: &self.camera,
            npcs: &self.npcs,
        }
    }

    fn set_island(&mut self, island: Island) {
        self.footprint = island.footprint();
        self.surface = island.surface();
        self.island = Some(island);
    }

    /// What the intro still lacks, or None when it can play.
    fn missing(&self) -> Option<&'static str> {
        if self.island.is_none() {
            Some("build or capture the island (/intro build, /intro island capture)")
        } else if self.camera.is_empty() {
            Some("record the camera (/intro camera record)")
        } else {
            None
        }
    }

    fn show_of(&self, player: &str) -> Option<usize> {
        self.shows.iter().position(|s| s.player == player)
    }

    fn start(&mut self, player: &Player, mode: Mode, wait: u32) -> Result<(), String> {
        if self.show_of(&player.id).is_some() {
            return Err(self.say("busy", &[("name", &player.name)]));
        }
        if self.build.is_some() || self.unbuild.is_some() {
            return Err(self.say("busy_build", &[]));
        }
        let (yaw, pitch) = players::rotation(&player.id).unwrap_or((0.0, 0.0));
        let mut lines = Vec::new();
        if !matches!(mode, Mode::Npc(_)) {
            lines.push(format!("gamemode spectator {}", player.name));
        }
        if mode == Mode::Play {
            lines.push(format!(
                "effect give {} minecraft:blindness {} 0 true",
                player.name,
                wait / 20 + 3
            ));
        }
        if !lines.is_empty() {
            command::run_quiet(&lines);
        }
        // The show sets the island's blocks itself, and right again at its end.
        self.restores.retain(|(p, _)| *p != player.id);
        self.staged.remove(&player.id);
        let bar = match &mode {
            Mode::Play => None,
            Mode::Camera => Some("the camera".to_owned()),
            Mode::Npc(name) => Some(format!("figure {name}")),
        }
        .map(|what| {
            self.say(
                "bar",
                &[
                    ("what", &what),
                    ("time", &"{time}"),
                    ("rip", &"{rip}"),
                    ("up", &"{up}"),
                ],
            )
        });
        let home = Pose {
            x: player.x,
            y: player.y,
            z: player.z,
            yaw,
            pitch,
        };
        let mut show = Show::new(
            &player.id,
            &player.name,
            mode,
            wait.max(5),
            home,
            &player.dimension,
            player.game_mode,
        );
        show.bar = bar;
        self.shows.push(show);
        Ok(())
    }

    /// Ends a show: the player's view, game mode and place back, the real blocks back, and a
    /// recording saved. The message for the player, if any.
    fn finish(&mut self, show: Show) -> Option<String> {
        let (mode, name, home, dimension, game_mode, recorded) = (
            show.mode.clone(),
            show.name.clone(),
            show.home,
            show.dimension.clone(),
            show.game_mode,
            show.recorded.clone(),
        );
        let (player, ghosts) = show.end(&self.scene());
        if !ghosts.is_empty() {
            self.restores.push((player.clone(), ghosts));
        }
        if matches!(mode, Mode::Play | Mode::Camera) {
            command::run_quiet(&[
                format!("effect clear {name} minecraft:blindness"),
                format!("gamemode {} {name}", mode_name(game_mode)),
            ]);
            let h = home;
            players::teleport(&player, &dimension, h.x, h.y, h.z, h.yaw, h.pitch);
        }
        match mode {
            Mode::Play => {
                if self.seen.insert(player) {
                    self.save_seen();
                }
                None
            }
            Mode::Camera => Some(self.keep_camera(recorded)),
            Mode::Npc(name) => Some(self.keep_npc(name, recorded)),
        }
    }

    fn save_seen(&self) {
        let ids: Vec<&str> = self.seen.iter().map(String::as_str).collect();
        save(SEEN, &ids.join("\n"));
    }

    fn keep_camera(&mut self, frames: Vec<Pose>) -> String {
        if frames.len() < 2 {
            return self.say("too_short", &[]);
        }
        save(CAMERA, &track_text(&frames));
        let ticks = frames.len();
        self.camera = frames;
        // A flown camera takes the place of one set by points.
        self.points.clear();
        storage::delete(POINTS);
        self.say("saved", &[("what", &"the camera"), ("ticks", &ticks)])
    }

    /// The operator's eyes now, as the camera would be there at `tick` of the intro.
    fn eyes(&self, player: &Player, tick: u32) -> Pose {
        let (yaw, pitch) = players::rotation(&player.id).unwrap_or((0.0, 0.0));
        let pose = Pose {
            x: player.x,
            y: player.y + EYES,
            z: player.z,
            yaw,
            pitch,
        };
        self.scene().recorded(pose, tick)
    }

    /// Takes the points as the camera's path: its frames made anew and both saved. With
    /// under two points there is no path.
    fn keep_points(&mut self, points: Vec<Point>) -> Result<usize, String> {
        let total = point_tick(&points, points.len().saturating_sub(1)) as usize + 1;
        if points.len() > MAX_POINTS || total > MAX_FRAMES {
            return Err(self.say("too_long", &[]));
        }
        self.camera = path(&points);
        self.points = points;
        if self.points.is_empty() {
            storage::delete(POINTS);
        } else {
            save(POINTS, &points_text(&self.points));
        }
        if self.camera.is_empty() {
            storage::delete(CAMERA);
        } else {
            save(CAMERA, &track_text(&self.camera));
        }
        Ok(self.camera.len())
    }

    fn camera_points(&mut self, player: &Player, words: &[&str]) -> Result<String, String> {
        let index = |n: &str| n.parse::<usize>().ok().filter(|n| *n >= 1).map(|n| n - 1);
        let mut points = self.points.clone();
        match words {
            ["add"] | ["add", _] => {
                let ticks = match words.get(1) {
                    Some(t) => t
                        .parse::<u32>()
                        .ok()
                        .filter(|t| (1..=1200).contains(t))
                        .ok_or_else(|| self.say("usage", &[]))?,
                    None => POINT_TICKS,
                };
                let ticks = if points.is_empty() { 0 } else { ticks };
                let at = point_tick(&points, points.len().saturating_sub(1)) + ticks;
                points.push(Point {
                    pose: self.eyes(player, at),
                    ticks,
                });
                let n = points.len();
                let total = self.keep_points(points)?;
                Ok(self.say(
                    "point_added",
                    &[("n", &n), ("ticks", &ticks), ("total", &total)],
                ))
            }
            ["set", n] => {
                let i = index(n)
                    .filter(|i| *i < points.len())
                    .ok_or_else(|| self.say("no_point", &[("n", n)]))?;
                points[i].pose = self.eyes(player, point_tick(&points, i));
                self.keep_points(points)?;
                Ok(self.say("point_set", &[("n", n)]))
            }
            ["remove", n] => {
                let i = index(n)
                    .filter(|i| *i < points.len())
                    .ok_or_else(|| self.say("no_point", &[("n", n)]))?;
                points.remove(i);
                if let Some(first) = points.first_mut() {
                    first.ticks = 0;
                }
                let total = self.keep_points(points)?;
                Ok(self.say("point_removed", &[("n", n), ("total", &total)]))
            }
            ["points"] => {
                if points.is_empty() {
                    return Ok(self.say("no_points", &[]));
                }
                let list: Vec<String> = points
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let at = point_tick(&points, i);
                        let Pose { x, y, z, .. } = p.pose;
                        format!("{} at tick {at} ({x:.0} {y:.0} {z:.0})", i + 1)
                    })
                    .collect();
                Ok(self.say("points", &[("list", &list.join(", "))]))
            }
            _ => Err(self.say("usage", &[])),
        }
    }

    /// Shows an operator the island in the ground, or as it is again.
    fn stage(&mut self, player: &Player) -> Result<String, String> {
        if self.show_of(&player.id).is_some() {
            return Err(self.say("busy", &[("name", &player.name)]));
        }
        let island = self
            .island
            .as_ref()
            .ok_or_else(|| self.say("no_island", &[]))?;
        let on = !self.staged.remove(&player.id);
        let blocks = if on {
            show::in_ground(island)
        } else {
            show::as_it_is(island)
        };
        if on {
            self.staged.insert(player.id.clone());
        }
        self.restores.retain(|(p, _)| *p != player.id);
        self.restores
            .push((player.id.clone(), show::chunks(blocks)));
        Ok(self.say(if on { "staged_on" } else { "staged_off" }, &[]))
    }

    fn keep_npc(&mut self, name: String, frames: Vec<Pose>) -> String {
        if frames.len() < 2 {
            return self.say("too_short", &[]);
        }
        save(&format!("{NPC}{name}"), &track_text(&frames));
        let ticks = frames.len();
        let what = format!("figure {name}");
        match self.npcs.iter_mut().find(|n| n.name == name) {
            Some(n) => n.frames = frames,
            None => self.npcs.push(Npc { name, frames }),
        }
        self.save_npc_names();
        self.say("saved", &[("what", &what), ("ticks", &ticks)])
    }

    fn save_npc_names(&self) {
        let names: Vec<&str> = self.npcs.iter().map(|n| n.name.as_str()).collect();
        save(NPCS, &names.join("\n"));
    }

    fn stop(&mut self, player: &str) -> Option<String> {
        let i = self.show_of(player)?;
        let show = self.shows.remove(i);
        self.finish(show)
    }

    /// Reads the island's blocks in the box, and the ground `drop` blocks under each.
    fn capture(&mut self, player: &Player) -> Result<String, String> {
        let s = &self.settings;
        let (Some(a), Some(b)) = (s.pos1, s.pos2) else {
            return Err(self.say("no_box", &[]));
        };
        let lo = [a[0].min(b[0]), a[1].min(b[1]), a[2].min(b[2])];
        let hi = [a[0].max(b[0]), a[1].max(b[1]), a[2].max(b[2])];
        let volume: i64 = (0..3).map(|i| i64::from(hi[i] - lo[i]) + 1).product();
        if volume > MAX_BOX {
            return Err(self.say("big_box", &[]));
        }
        let dim = player.dimension.clone();
        let unloaded = self.say("unloaded", &[]);
        let get = |x, y, z| world::get_block(&dim, x, y, z).ok_or_else(|| unloaded.clone());
        let mut island = Island {
            dimension: dim.clone(),
            drop: s.drop,
            ..Island::default()
        };
        for y in lo[1]..=hi[1] {
            for z in lo[2]..=hi[2] {
                for x in lo[0]..=hi[0] {
                    let state = get(x, y, z)?;
                    if is_air(&state) {
                        continue;
                    }
                    if island.blocks.len() >= MAX_ISLAND {
                        return Err(self.say("big_island", &[]));
                    }
                    island.ground.push(get(x, y - s.drop, z)?);
                    island.blocks.push(([x, y, z], state));
                }
            }
        }
        Ok(self.keep_island(island))
    }

    fn keep_island(&mut self, island: Island) -> String {
        save(ISLAND, &island.text());
        self.adopt_island(island)
    }

    /// Takes an island already saved.
    fn adopt_island(&mut self, island: Island) -> String {
        let count = island.blocks.len();
        let drop = island.drop;
        self.settings.drop = drop;
        save(SETTINGS, &self.settings.text());
        self.set_island(island);
        self.say("captured", &[("count", &count), ("drop", &drop)])
    }

    fn start_unbuild(&mut self, player: &Player) -> Result<String, String> {
        if self.build.is_some() || self.unbuild.is_some() {
            return Err(self.say("busy_build", &[]));
        }
        let unbuild = load(UNDO)
            .and_then(|text| Unbuild::new(text, &player.id))
            .ok_or_else(|| self.say("no_undo", &[]))?;
        // Nobody watches an island that is going away.
        for show in std::mem::take(&mut self.shows) {
            let player = show.player.clone();
            if let Some(line) = self.finish(show) {
                players::tell(&player, &line);
            }
        }
        self.restores.clear();
        self.staged.clear();
        self.unbuild = Some(unbuild);
        Ok(self.say("unbuilding", &[]))
    }

    fn step_unbuild(&mut self) {
        let Some(unbuild) = self.unbuild.as_mut() else {
            return;
        };
        let player = unbuild.player.clone();
        let line = match unbuild.step() {
            Ok(false) => return,
            Ok(true) => {
                let count = unbuild.count();
                storage::delete(UNDO);
                storage::delete(ISLAND);
                self.island = None;
                self.footprint = [0; 4];
                self.surface.clear();
                self.say("unbuilt", &[("count", &count)])
            }
            Err(error) => self.say("unbuild_failed", &[("error", &error)]),
        };
        self.unbuild = None;
        players::tell(&player, &line);
    }

    fn start_build(&mut self, player: &Player, lift: &str) -> Result<String, String> {
        if self.build.is_some() || self.unbuild.is_some() {
            return Err(self.say("busy_build", &[]));
        }
        if storage::get(UNDO).is_some() {
            return Err(self.say("built_already", &[]));
        }
        let lift = lift
            .parse::<i32>()
            .ok()
            .filter(|l| (8..=256).contains(l))
            .ok_or_else(|| self.say("usage", &[]))?;
        let text = load(SCHEMATIC).ok_or_else(|| self.say("no_schematic", &[]))?;
        let feet = [
            player.x.floor() as i32,
            player.y.floor() as i32,
            player.z.floor() as i32,
        ];
        let build = Build::new(text, &player.id, &player.dimension, feet, lift)
            .map_err(|why| self.say("bad_build", &[("why", &why)]))?;
        self.build = Some(build);
        Ok(self.say("building", &[]))
    }

    fn step_build(&mut self) {
        let Some(build) = self.build.as_mut() else {
            return;
        };
        let player = build.player.clone();
        match build.step() {
            Ok(None) => {}
            Ok(Some(island)) => {
                self.build = None;
                let f = island.footprint();
                let (_, top) = island.heights();
                let (low, _) = island.heights();
                let bottom = low - island.drop;
                let drop = island.drop;
                self.adopt_island(island);
                let count = self.island.as_ref().map_or(0, |i| i.blocks.len());
                let line = self.say(
                    "built",
                    &[
                        ("count", &count),
                        ("drop", &drop),
                        ("x1", &f[0]),
                        ("x2", &f[1]),
                        ("y1", &bottom),
                        ("y2", &top),
                        ("z1", &f[2]),
                        ("z2", &f[3]),
                    ],
                );
                players::tell(&player, &line);
            }
            Err(error) => {
                self.build = None;
                let line = self.say("build_failed", &[("error", &error)]);
                players::tell(&player, &line);
            }
        }
    }

    fn status(&self) -> String {
        let island = match &self.island {
            Some(i) => format!("{} blocks", i.blocks.len()),
            None => "none".into(),
        };
        let camera = if self.camera.is_empty() {
            "not recorded".into()
        } else {
            format!("{} ticks", self.camera.len())
        };
        let s = &self.settings;
        self.say(
            "status",
            &[
                ("island", &island),
                ("camera", &camera),
                ("npcs", &self.npcs.len()),
                ("rip", &s.rip),
                ("rise", &s.rise),
                ("drop", &s.drop),
            ],
        )
    }

    fn operator(&mut self, player: &Player, words: &[&str]) -> Result<String, String> {
        let number = |w: &str| w.parse::<i64>().ok();
        match words {
            ["status"] => Ok(self.status()),
            ["play"] => self.play(player),
            ["play", name] => {
                let target =
                    find_player(name).ok_or_else(|| self.say("not_on", &[("name", name)]))?;
                self.play(&target)
            }
            ["reset", "all"] => {
                self.seen.clear();
                save(SEEN, "");
                Ok(self.say("reset_all", &[]))
            }
            ["reset", name] => {
                if let Some(p) = find_player(name) {
                    self.seen.remove(&p.id);
                    self.save_seen();
                }
                Ok(self.say("reset", &[("name", name)]))
            }
            ["build", lift] => self.start_build(player, lift),
            ["unbuild"] => self.start_unbuild(player),
            ["island", which @ ("pos1" | "pos2")] => {
                let at = [
                    player.x.floor() as i32,
                    player.y.floor() as i32,
                    player.z.floor() as i32,
                ];
                if *which == "pos1" {
                    self.settings.pos1 = Some(at);
                } else {
                    self.settings.pos2 = Some(at);
                }
                save(SETTINGS, &self.settings.text());
                let [x, y, z] = at;
                Ok(self.say("pos", &[("which", which), ("x", &x), ("y", &y), ("z", &z)]))
            }
            ["island", "drop", blocks] => {
                let blocks = number(blocks)
                    .filter(|b| (1..=256).contains(b))
                    .ok_or_else(|| self.say("usage", &[]))?;
                self.settings.drop = blocks as i32;
                save(SETTINGS, &self.settings.text());
                Ok(self.say("drop", &[("blocks", &blocks)]))
            }
            ["island", "capture"] => self.capture(player),
            ["rip", tick, rise] => {
                let (Some(tick), Some(rise)) = (number(tick), number(rise)) else {
                    return Err(self.say("usage", &[]));
                };
                if !(3..=6000).contains(&tick) || !(1..=1200).contains(&rise) {
                    return Err(self.say("usage", &[]));
                }
                self.settings.rip = tick as u32;
                self.settings.rise = rise as u32;
                save(SETTINGS, &self.settings.text());
                Ok(self.say("rip", &[("tick", &tick), ("rise", &rise)]))
            }
            ["shake", blocks] => {
                let blocks = blocks
                    .parse::<f32>()
                    .ok()
                    .filter(|b| (0.0..=3.0).contains(b))
                    .ok_or_else(|| self.say("usage", &[]))?;
                self.settings.shake = blocks;
                save(SETTINGS, &self.settings.text());
                Ok(self.say("shake", &[("blocks", &blocks)]))
            }
            ["camera", "record"] => {
                self.start(player, Mode::Camera, 0)?;
                Ok(self.say("recording", &[("what", &"the camera")]))
            }
            ["stage"] => self.stage(player),
            ["camera", rest @ ..]
                if matches!(rest.first(), Some(&("add" | "set" | "remove" | "points"))) =>
            {
                self.camera_points(player, rest)
            }
            ["camera", "delete"] => {
                self.camera.clear();
                self.points.clear();
                storage::delete(CAMERA);
                storage::delete(POINTS);
                Ok(self.say("deleted", &[("what", &"the camera")]))
            }
            ["npc", "record", name] => {
                if !npc_name_ok(name) {
                    return Err(self.say("bad_name", &[]));
                }
                if self.npcs.len() >= MAX_NPCS && !self.npcs.iter().any(|n| n.name == *name) {
                    return Err(self.say("too_many", &[]));
                }
                self.start(player, Mode::Npc((*name).to_owned()), 0)?;
                let what = format!("figure {name}");
                Ok(self.say("recording", &[("what", &what)]))
            }
            ["npc", "delete", name] => {
                let i = self
                    .npcs
                    .iter()
                    .position(|n| n.name == *name)
                    .ok_or_else(|| self.say("unknown_npc", &[("name", name)]))?;
                self.npcs.remove(i);
                storage::delete(&format!("{NPC}{name}"));
                self.save_npc_names();
                let what = format!("figure {name}");
                Ok(self.say("deleted", &[("what", &what)]))
            }
            ["npc", "list"] => {
                if self.npcs.is_empty() {
                    return Ok(self.say("no_npcs", &[]));
                }
                let list: Vec<String> = self
                    .npcs
                    .iter()
                    .map(|n| format!("{} ({} ticks)", n.name, n.frames.len()))
                    .collect();
                Ok(self.say("npcs", &[("list", &list.join(", "))]))
            }
            _ => Err(self.say("usage", &[])),
        }
    }

    fn play(&mut self, target: &Player) -> Result<String, String> {
        if let Some(missing) = self.missing() {
            return Err(self.say("not_ready", &[("missing", &missing)]));
        }
        self.start(target, Mode::Play, PLAY_WAIT)?;
        Ok(self.say("playing", &[("name", &target.name)]))
    }
}

impl ServerMod for Intro {
    fn init() -> Result<(Self, Registration), String> {
        let settings = load(SETTINGS)
            .map(|t| Settings::parse(&t))
            .unwrap_or_default();
        let camera = load(CAMERA).map(|t| parse_track(&t)).unwrap_or_default();
        let points = load(POINTS).map(|t| parse_points(&t)).unwrap_or_default();
        let seen = load(SEEN)
            .map(|t| {
                t.lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let npcs = load(NPCS)
            .unwrap_or_default()
            .lines()
            .filter_map(|name| {
                let frames = parse_track(&load(&format!("{NPC}{name}"))?);
                (npc_name_ok(name) && !frames.is_empty()).then(|| Npc {
                    name: name.to_owned(),
                    frames,
                })
            })
            .collect();
        let mut intro = Intro {
            settings,
            island: None,
            footprint: [0; 4],
            surface: Vec::new(),
            camera,
            points,
            npcs,
            staged: BTreeSet::new(),
            seen,
            shows: Vec::new(),
            restores: Vec::new(),
            build: None,
            unbuild: None,
            messages: Messages::load(MESSAGES),
        };
        if let Some(island) = load(ISLAND).and_then(|t| Island::parse(&t)) {
            intro.set_island(island);
        }
        Ok((
            intro,
            Registration::new().command("intro").on_tick().on_joins(),
        ))
    }

    fn player_joined(&mut self, player: &Player) {
        if self.seen.contains(&player.id) || self.missing().is_some() {
            return;
        }
        let _ = self.start(player, Mode::Play, JOIN_WAIT);
    }

    fn player_left(&mut self, player: &Player) {
        self.restores.retain(|(p, _)| *p != player.id);
        self.staged.remove(&player.id);
        // Their client forgets what it was shown; their game mode goes back now.
        if let Some(i) = self.show_of(&player.id) {
            let show = self.shows.remove(i);
            show.drop_bar();
            if matches!(show.mode, Mode::Play | Mode::Camera) {
                command::run_quiet(&[format!(
                    "gamemode {} {}",
                    mode_name(show.game_mode),
                    show.name
                )]);
            }
        }
    }

    fn command(
        &mut self,
        player: &Player,
        _name: &str,
        args: &horizon_mod_sdk::server::Args,
    ) -> Result<Option<String>, String> {
        let words: Vec<&str> = args.text().split_whitespace().collect();
        // Everyone may end their own intro.
        if matches!(words.as_slice(), ["skip"] | ["stop"]) {
            if self.show_of(&player.id).is_none() {
                return Err(self.say("nothing", &[]));
            }
            let line = self.stop(&player.id);
            return Ok(Some(line.unwrap_or_else(|| self.say("stopped", &[]))));
        }
        if players::permission(&player.id).unwrap_or(0) < OPERATOR {
            return Err(self.say("not_operator", &[]));
        }
        self.operator(player, &words).map(Some)
    }

    fn tick(&mut self, _tick: u64) {
        self.step_build();
        self.step_unbuild();
        let mut budget = Budget {
            blocks: BLOCKS_PER_TICK,
            calls: CALLS_PER_TICK,
        };
        for (player, queue) in &mut self.restores {
            while let Some(chunk) = queue.front() {
                if chunk.len() > budget.blocks {
                    break;
                }
                budget.blocks -= chunk.len();
                let chunk = queue.pop_front().unwrap_or_default();
                cinematic::show_blocks(player, &chunk);
            }
        }
        self.restores.retain(|(_, q)| !q.is_empty());
        let mut shows = std::mem::take(&mut self.shows);
        let mut done = Vec::new();
        {
            let scene = self.scene();
            for (i, show) in shows.iter_mut().enumerate() {
                if show.step(&scene, &mut budget) {
                    done.push(i);
                }
            }
        }
        for i in done.into_iter().rev() {
            let show = shows.remove(i);
            let player = show.player.clone();
            if let Some(line) = self.finish(show) {
                players::tell(&player, &line);
            }
        }
        shows.append(&mut self.shows);
        self.shows = shows;
    }
}

horizon_mod_sdk::export_server_mod!(Intro);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figure_names() {
        assert!(npc_name_ok("self"));
        assert!(npc_name_ok("Guide_1"));
        assert!(!npc_name_ok(""));
        assert!(!npc_name_ok("a b"));
        assert!(!npc_name_ok(&"x".repeat(17)));
    }
}
