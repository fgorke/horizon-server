//! Horizon Server plugin (`docs/plugins.md`): a cutscene for each player the first time they
//! join, which only they see while everyone else plays on. The spawn island floats in the sky
//! over the crater it was torn from; in the intro it is still in the ground. The camera flies a
//! recorded path, recorded figures walk (one of them the player, in their own skin), then the
//! ground rumbles and the island tears loose and rises, carrying the camera and whoever stands
//! on it up to where it floats. The blocks, displays, figures, dust and sounds go to that
//! player alone (capability `cinematic`): vanilla packets, no client mod.
//!
//! Operators set it up in game: `/intro build <lift>` builds the island from the schematic in
//! storage key `schematic` (`ground <y>`, then `x y z state` a line) where they stand, the
//! crater under them and the island `lift` blocks up (or `/intro island pos1|pos2`, `drop`,
//! `capture` takes one already built); `/intro camera record` and `/intro npc record
//! <name|self>` record while the intro plays around them, `/intro stop` ends a recording;
//! `/intro rip <tick> <rise ticks>` times the tearing loose.
mod build;
mod scene;
mod show;

use build::Build;
use horizon_mod_sdk::{
    command,
    messages::Messages,
    server::{Player, Registration, ServerMod, cinematic, players, world},
    storage,
};
use scene::{Footprint, Island, Pose, Schematic, Settings, is_air, parse_track, track_text};
use show::{BLOCKS_PER_TICK, Mode, Npc, Scene, Show};
use std::collections::{BTreeSet, VecDeque};

const OPERATOR: u8 = 2;
const SETTINGS: &str = "settings";
const ISLAND: &str = "island";
const SCHEMATIC: &str = "schematic";
const CAMERA: &str = "camera";
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
const MAX_ISLAND: usize = 32768;
const MAX_BOX: i64 = 128 * 128 * 128;
/// Ticks between a join and the intro (the player's chunks arrive meanwhile, blinded), and
/// before a show (the island's blocks go out meanwhile).
const JOIN_WAIT: u32 = 40;
const PLAY_WAIT: u32 = 10;

const MESSAGES: &[(&str, &str)] = &[
    ("not_operator", "You need to be an operator for that."),
    (
        "usage",
        "/intro status | play [player] | stop | skip | reset <player|all> | build <lift> | island pos1|pos2|drop <blocks>|capture | rip <tick> <rise ticks> | shake <blocks> | camera record|delete | npc record <name|self> | npc delete <name> | npc list",
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
        "Building the island: {count} blocks over the next ticks...",
    ),
    (
        "built",
        "The island stands, {count} blocks, {drop} blocks over its crater. Protect x {x1}..{x2}, y {y1}..{y2}, z {z1}..{z2}, and set the world spawn on it.",
    ),
    ("build_failed", "Building the island failed: {error}"),
    ("busy_build", "An island is being built already."),
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
    npcs: Vec<Npc>,
    seen: BTreeSet<String>,
    shows: Vec<Show>,
    /// Blocks still to set right for players whose show ended.
    restores: Vec<(String, VecDeque<Vec<cinematic::GhostBlock>>)>,
    build: Option<Build>,
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
        // The island's blocks take a few ticks to go out.
        self.restores.retain(|(p, _)| *p != player.id);
        let home = Pose {
            x: player.x,
            y: player.y,
            z: player.z,
            yaw,
            pitch,
        };
        let show = Show::new(
            &player.id,
            &player.name,
            mode,
            wait.max(5),
            home,
            &player.dimension,
            player.game_mode,
            &self.scene(),
        );
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
        self.say("saved", &[("what", &"the camera"), ("ticks", &ticks)])
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
        let count = island.blocks.len();
        let drop = island.drop;
        self.settings.drop = drop;
        save(SETTINGS, &self.settings.text());
        self.set_island(island);
        self.say("captured", &[("count", &count), ("drop", &drop)])
    }

    fn start_build(&mut self, player: &Player, lift: &str) -> Result<String, String> {
        if self.build.is_some() {
            return Err(self.say("busy_build", &[]));
        }
        let lift = lift
            .parse::<i32>()
            .ok()
            .filter(|l| (8..=256).contains(l))
            .ok_or_else(|| self.say("usage", &[]))?;
        let schematic = load(SCHEMATIC)
            .and_then(|t| Schematic::parse(&t))
            .ok_or_else(|| self.say("no_schematic", &[]))?;
        if schematic.blocks.len() > MAX_ISLAND {
            return Err(self.say("big_island", &[]));
        }
        let feet = [
            player.x.floor() as i32,
            player.y.floor() as i32,
            player.z.floor() as i32,
        ];
        let build = Build::plan(&schematic, &player.id, &player.dimension, feet, lift)
            .map_err(|why| self.say("bad_build", &[("why", &why)]))?;
        if !build.loaded() {
            return Err(self.say("unloaded", &[]));
        }
        let count = build.progress().1;
        self.build = Some(build);
        Ok(self.say("building", &[("count", &count)]))
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
                self.keep_island(island);
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
            ["camera", "delete"] => {
                self.camera.clear();
                storage::delete(CAMERA);
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
            npcs,
            seen,
            shows: Vec::new(),
            restores: Vec::new(),
            build: None,
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
        // Their client forgets what it was shown; their game mode goes back now.
        if let Some(i) = self.show_of(&player.id) {
            let show = self.shows.remove(i);
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
        let mut budget = BLOCKS_PER_TICK;
        for (player, queue) in &mut self.restores {
            while let Some(chunk) = queue.front() {
                if chunk.len() > budget {
                    break;
                }
                budget -= chunk.len();
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
