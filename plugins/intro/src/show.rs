//! One player's intro, tick by tick. Only that player sees any of it: the island back in the
//! ground (blocks shown to them alone, `cinematic.show-blocks`), the camera and the figures
//! (entities only they have), the island tearing loose (block displays rising, the ground's
//! real crater shown again, block particles and sounds sent to them alone), and what stands on
//! the island rising with it.
use crate::scene::{Footprint, Island, Pose, Settings, shake, state_nbt};
use horizon_mod_sdk::{
    command,
    server::{GameMode, cinematic, players},
};
use std::collections::VecDeque;

/// `cinematic.show-blocks`' limit per call: what all shows send in a tick together.
pub const BLOCKS_PER_TICK: usize = 8192;
/// Blocks in one `show-blocks` call.
const CHUNK: usize = 4096;
/// Displays made per tick, ahead of the rip.
const DISPLAYS_PER_TICK: usize = 1500;
/// The camera eases this many ticks between its poses.
const CAMERA_EASE: u32 = 2;
/// The eyes' height above the feet.
pub const EYES: f64 = 1.62;
/// Ticks between bursts of dust while the island rises.
const DUST_EVERY: u32 = 4;
const RUMBLE_EVERY: u32 = 8;

/// What a show does with its player.
#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    /// The intro, through the recorded camera.
    Play,
    /// The intro around the operator, who flies (as a spectator) and is recorded as the camera.
    Camera,
    /// The intro around the operator, who walks and is recorded as this figure.
    Npc(String),
}

/// A figure: its name (`self` for the player watching) and its walk.
#[derive(Clone, Debug, PartialEq)]
pub struct Npc {
    pub name: String,
    pub frames: Vec<Pose>,
}

/// What a show plays: the intro's parts, as the plugin holds them.
pub struct Scene<'a> {
    pub settings: &'a Settings,
    pub island: Option<&'a Island>,
    pub footprint: Footprint,
    /// The island blocks a rising copy needs.
    pub surface: &'a [usize],
    pub camera: &'a [Pose],
    pub npcs: &'a [Npc],
}

impl Scene<'_> {
    /// Whether a pose at `x z` rides the island once it tears loose.
    fn rides(&self, pose: &Pose) -> bool {
        self.island.is_some() && crate::scene::over(&self.footprint, pose.x, pose.z)
    }

    /// Where a recorded pose is shown at `tick`: carried up by the island if it stands on it.
    pub fn shown(&self, pose: Pose, tick: u32) -> Pose {
        match self.island {
            Some(island) if self.rides(&pose) => pose.up(self.settings.lift(tick, island.drop)),
            _ => pose,
        }
    }

    /// What a pose seen at `tick` is recorded as: without the island's lift, as `shown` adds it.
    pub fn recorded(&self, pose: Pose, tick: u32) -> Pose {
        match self.island {
            Some(island) if self.rides(&pose) => pose.up(-self.settings.lift(tick, island.drop)),
            _ => pose,
        }
    }

    /// The middle of the island's ground, where the dust flies: `x y z`, and half its size.
    fn ground(&self) -> Option<([f64; 3], [f64; 2])> {
        let island = self.island?;
        let f = &self.footprint;
        let (low, high) = island.heights();
        let x = f64::from(f[0] + f[1] + 1) / 2.0;
        let z = f64::from(f[2] + f[3] + 1) / 2.0;
        let y = f64::from((low + high) / 2 - island.drop);
        let half = [
            f64::from(f[1] - f[0] + 1) / 2.0,
            f64::from(f[3] - f[2] + 1) / 2.0,
        ];
        Some(([x, y, z], half))
    }
}

/// One player's intro under way.
pub struct Show {
    pub player: String,
    pub name: String,
    pub mode: Mode,
    /// Ticks before it starts.
    pub wait: u32,
    /// Ticks since it started.
    pub tick: u32,
    camera: Option<u32>,
    /// The figures' entities, by index in the scene's figures.
    npcs: Vec<(u32, usize)>,
    /// The rising piece's displays, and how many surface blocks have one.
    displays: Vec<u32>,
    made: usize,
    /// Blocks still to show, in order.
    ghosts: VecDeque<Vec<cinematic::GhostBlock>>,
    /// The crater shown again, and the island back in the sky as it really is.
    cratered: bool,
    landed: bool,
    /// Where the player was and their game mode, given back at the end.
    pub home: Pose,
    pub dimension: String,
    pub game_mode: GameMode,
    pub recorded: Vec<Pose>,
}

pub fn ghost([x, y, z]: [i32; 3], state: &str) -> cinematic::GhostBlock {
    cinematic::GhostBlock {
        x,
        y,
        z,
        state: state.to_owned(),
    }
}

/// A display of `state`, `offset` blocks lower than it stands, getting there in `ease` ticks.
pub fn display_nbt(state: &str, offset: i32, ease: u32) -> String {
    format!(
        "{{block_state:{},start_interpolation:0,interpolation_duration:{ease},transformation:{{translation:[0f,{}f,0f],left_rotation:[0f,0f,0f,1f],scale:[1f,1f,1f],right_rotation:[0f,0f,0f,1f]}}}}",
        state_nbt(state),
        -offset
    )
}

impl Show {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        player: &str,
        name: &str,
        mode: Mode,
        wait: u32,
        home: Pose,
        dimension: &str,
        game_mode: GameMode,
        scene: &Scene,
    ) -> Show {
        let mut show = Show {
            player: player.to_owned(),
            name: name.to_owned(),
            mode,
            wait,
            tick: 0,
            camera: None,
            npcs: Vec::new(),
            displays: Vec::new(),
            made: 0,
            ghosts: VecDeque::new(),
            cratered: false,
            landed: false,
            home,
            dimension: dimension.to_owned(),
            game_mode,
            recorded: Vec::new(),
        };
        // Meanwhile (blinded), the island leaves the sky and is back in the ground.
        if let Some(island) = scene.island {
            show.queue(
                island
                    .blocks
                    .iter()
                    .map(|(p, _)| ghost(*p, "minecraft:air")),
            );
            show.queue(
                island
                    .blocks
                    .iter()
                    .map(|([x, y, z], s)| ghost([*x, *y - island.drop, *z], s)),
            );
        }
        show
    }

    fn queue(&mut self, blocks: impl Iterator<Item = cinematic::GhostBlock>) {
        let blocks: Vec<_> = blocks.collect();
        for chunk in blocks.chunks(CHUNK) {
            self.ghosts.push_back(chunk.to_vec());
        }
    }

    /// Sends what of the queue fits in `budget`.
    pub fn flush(&mut self, budget: &mut usize) {
        while let Some(chunk) = self.ghosts.front() {
            if chunk.len() > *budget {
                return;
            }
            *budget -= chunk.len();
            let chunk = self.ghosts.pop_front().unwrap_or_default();
            cinematic::show_blocks(&self.player, &chunk);
        }
    }

    /// One tick: true when the show is over.
    pub fn step(&mut self, scene: &Scene, budget: &mut usize) -> bool {
        self.flush(budget);
        if self.wait > 0 {
            self.wait -= 1;
            return false;
        }
        match self.tick {
            0 => self.begin(scene),
            2 if self.mode == Mode::Play => {
                command::run_quiet(&[format!("effect clear {} minecraft:blindness", self.name)]);
            }
            _ => {}
        }
        self.move_figures(scene);
        self.tear_loose(scene);
        self.effects(scene);
        if self.mode != Mode::Play {
            self.record(scene);
        }
        self.tick += 1;
        match self.mode {
            Mode::Play => self.tick as usize >= scene.camera.len(),
            _ => self.recorded.len() >= crate::MAX_FRAMES,
        }
    }

    /// The intro's first tick: the others hidden, the camera and the figures out.
    fn begin(&mut self, scene: &Scene) {
        cinematic::hide_players(&self.player, true);
        if self.mode == Mode::Play
            && let Some(first) = scene.camera.first()
        {
            let nbt = format!("{{teleport_duration:{CAMERA_EASE}}}");
            self.camera = cinematic::spawn(
                &self.player,
                "item_display",
                first.x,
                first.y,
                first.z,
                first.yaw,
                first.pitch,
                &nbt,
            )
            .ok();
            if let Some(camera) = self.camera {
                cinematic::view(&self.player, Some(camera));
            }
        }
        for (i, npc) in scene.npcs.iter().enumerate() {
            if self.mode == Mode::Npc(npc.name.clone()) {
                continue;
            }
            let Some(first) = npc.frames.first() else {
                continue;
            };
            let shown = if npc.name == "self" {
                self.name.as_str()
            } else {
                npc.name.as_str()
            };
            let nbt =
                format!("{{profile:\"{shown}\",CustomName:\"{shown}\",CustomNameVisible:1b}}");
            if let Ok(id) = cinematic::spawn(
                &self.player,
                "mannequin",
                first.x,
                first.y,
                first.z,
                first.yaw,
                first.pitch,
                &nbt,
            ) {
                self.npcs.push((id, i));
            }
        }
    }

    fn move_figures(&self, scene: &Scene) {
        let t = self.tick;
        if let Some(camera) = self.camera
            && let Some(pose) = scene.camera.get(t as usize)
        {
            let p = scene.shown(*pose, t);
            let [dx, dy, dz] = shake(t, scene.settings);
            cinematic::move_to(
                &self.player,
                camera,
                p.x + dx,
                p.y + dy,
                p.z + dz,
                p.yaw,
                p.pitch,
            );
        }
        for &(id, i) in &self.npcs {
            if let Some(pose) = scene.npcs[i].frames.get(t as usize) {
                let p = scene.shown(*pose, t);
                cinematic::move_to(&self.player, id, p.x, p.y, p.z, p.yaw, p.pitch);
            }
        }
    }

    /// The island tearing loose: its displays made ahead of time in the ground, the real
    /// crater shown and the displays rising at the rip, the real island back once they are up.
    fn tear_loose(&mut self, scene: &Scene) {
        let Some(island) = scene.island else {
            return;
        };
        let s = scene.settings;
        let rip = s.rip.max(3);
        let batches = scene.surface.len().div_ceil(DISPLAYS_PER_TICK) as u32;
        if self.tick >= rip.saturating_sub(batches + 1).max(1) && self.made < scene.surface.len() {
            let end = (self.made + DISPLAYS_PER_TICK).min(scene.surface.len());
            for &i in &scene.surface[self.made..end] {
                let ([x, y, z], state) = &island.blocks[i];
                let nbt = display_nbt(state, island.drop, 0);
                let at = |v: i32| f64::from(v);
                if let Ok(id) = cinematic::spawn(
                    &self.player,
                    "block_display",
                    at(*x),
                    at(*y),
                    at(*z),
                    0.0,
                    0.0,
                    &nbt,
                ) {
                    self.displays.push(id);
                }
            }
            self.made = end;
        }
        if self.tick == rip {
            self.show_crater(island);
            for (&id, &i) in self.displays.iter().zip(scene.surface) {
                let nbt = display_nbt(&island.blocks[i].1, 0, s.rise.max(1));
                cinematic::set(&self.player, id, &nbt);
            }
        }
        if self.tick > rip + s.rise.max(1) && !self.landed {
            self.land(island);
        }
        // The displays go once the real island is there to take their place.
        if self.landed && self.ghosts.is_empty() && !self.displays.is_empty() {
            cinematic::remove(&self.player, &self.displays);
            self.displays.clear();
        }
    }

    fn show_crater(&mut self, island: &Island) {
        if !self.cratered {
            self.cratered = true;
            let real = island.blocks.iter().zip(&island.ground);
            self.queue(real.map(|(([x, y, z], _), g)| ghost([*x, *y - island.drop, *z], g)));
        }
    }

    fn land(&mut self, island: &Island) {
        if !self.landed {
            self.landed = true;
            self.queue(island.blocks.iter().map(|(p, s)| ghost(*p, s)));
        }
    }

    /// Dust, debris and rumbling around the rip, sent to this player alone.
    fn effects(&self, scene: &Scene) {
        let Some(([x, y, z], [hx, hz])) = scene.ground() else {
            return;
        };
        let s = scene.settings;
        let rip = s.rip.max(3);
        let t = self.tick;
        let n = &self.name;
        let mut lines = Vec::new();
        let rumbling = t + crate::scene::RUMBLE >= rip && t < rip;
        if rumbling && (t + crate::scene::RUMBLE - rip).is_multiple_of(RUMBLE_EVERY) {
            lines.push(format!(
                "playsound minecraft:block.stone.break master {n} {x} {y} {z} 3 0.5"
            ));
            lines.push(format!(
                "particle minecraft:block{{block_state:\"minecraft:dirt\"}} {x} {} {z} {hx} 0.3 {hz} 0.1 60 force {n}",
                y + 1.0
            ));
        }
        if t == rip {
            lines.push(format!(
                "playsound minecraft:entity.generic.explode master {n} {x} {y} {z} 6 0.5"
            ));
            lines.push(format!(
                "playsound minecraft:entity.warden.emerge master {n} {x} {y} {z} 6 0.6"
            ));
            lines.push(format!(
                "particle minecraft:explosion_emitter {x} {} {z} {hx} 1 {hz} 0 6 force {n}",
                y + 1.0
            ));
            lines.push(format!(
                "particle minecraft:block{{block_state:\"minecraft:stone\"}} {x} {} {z} {hx} 2 {hz} 0.6 600 force {n}",
                y + 1.0
            ));
        }
        let rising = t > rip && t < rip + s.rise;
        if rising && (t - rip).is_multiple_of(DUST_EVERY) {
            // Debris raining from the island's underside, and dust off the crater's rim.
            let under = scene
                .island
                .map_or(y, |i| f64::from(i.heights().0 - i.drop) + s.lift(t, i.drop));
            lines.push(format!(
                "particle minecraft:block{{block_state:\"minecraft:dirt\"}} {x} {under} {z} {} 1 {} 0 80 force {n}",
                hx * 0.6,
                hz * 0.6
            ));
            lines.push(format!(
                "particle minecraft:campfire_cosy_smoke {x} {} {z} {hx} 0.5 {hz} 0.02 20 force {n}",
                y + 1.0
            ));
        }
        if rising && (t - rip).is_multiple_of(RUMBLE_EVERY) {
            lines.push(format!(
                "playsound minecraft:block.gravel.break master {n} {x} {y} {z} 3 0.5"
            ));
        }
        if !lines.is_empty() {
            command::run_quiet(&lines);
        }
    }

    fn record(&mut self, scene: &Scene) {
        let Some(p) = players::online().into_iter().find(|p| p.id == self.player) else {
            return;
        };
        let (yaw, pitch) = players::rotation(&p.id).unwrap_or((0.0, 0.0));
        let eyes = if self.mode == Mode::Camera { EYES } else { 0.0 };
        let pose = Pose {
            x: p.x,
            y: p.y + eyes,
            z: p.z,
            yaw,
            pitch,
        };
        self.recorded.push(scene.recorded(pose, self.tick));
    }

    /// Ends the show for the player: their own view and the others back, the entities gone.
    /// The blocks still to set right, queued for the plugin to send.
    pub fn end(mut self, scene: &Scene) -> (String, VecDeque<Vec<cinematic::GhostBlock>>) {
        let p = &self.player;
        let mut ids: Vec<u32> = self.npcs.iter().map(|(id, _)| *id).collect();
        ids.extend(self.camera);
        ids.extend(&self.displays);
        cinematic::view(p, None);
        if !ids.is_empty() {
            cinematic::remove(p, &ids);
        }
        cinematic::hide_players(p, false);
        if let Some(island) = scene.island {
            self.show_crater(island);
            self.land(island);
        }
        (self.player, self.ghosts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_start_lower_and_ease() {
        let nbt = display_nbt("minecraft:stone", 40, 0);
        assert!(nbt.contains("translation:[0f,-40f,0f]"));
        assert!(nbt.contains("interpolation_duration:0"));
        let up = display_nbt("minecraft:stone", 0, 80);
        assert!(up.contains("translation:[0f,0f,0f]"));
        assert!(up.contains("interpolation_duration:80"));
        assert!(up.starts_with("{block_state:{Name:\"minecraft:stone\"}"));
    }

    #[test]
    fn what_stands_on_the_island_rides_it() {
        let island = Island {
            dimension: "minecraft:overworld".into(),
            drop: 60,
            blocks: vec![([0, 100, 0], "minecraft:stone".into())],
            ground: vec!["minecraft:air".into()],
        };
        let settings = Settings {
            rip: 10,
            rise: 20,
            ..Settings::default()
        };
        let scene = Scene {
            settings: &settings,
            island: Some(&island),
            footprint: island.footprint(),
            surface: &[0],
            camera: &[],
            npcs: &[],
        };
        let on = Pose {
            x: 0.5,
            y: 41.0,
            z: 0.5,
            ..Pose::default()
        };
        let off = Pose { x: 10.5, ..on };
        assert_eq!(scene.shown(on, 5).y, 41.0);
        assert_eq!(scene.shown(on, 20).y, 71.0);
        assert_eq!(scene.shown(on, 40).y, 101.0);
        assert_eq!(scene.shown(off, 40).y, 41.0);
        assert_eq!(scene.recorded(scene.shown(on, 20), 20), on);
    }
}
