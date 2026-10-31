//! A mod run with no game: `cargo horizon-mod test` (docs/mods.md, "Testing a mod").
//!
//! [`TestGame`] loads one `.hmod` from a scratch copy (its `storage` lives and
//! dies with the run) into the same sandbox the game uses, and plays the game's
//! part: the snapshot, ticks, frames, chat lines and commands, and for a
//! server mod the players joining, leaving, chatting and breaking or placing
//! blocks. [`Script`] is a
//! test written in TOML, `tests/<name>.toml` in the mod's folder: steps, each
//! saying what happens and what the mod must have shown by then.
use crate::{
    BlockPos, Clock, Corner, Datagram, GameMode, Mods, Output, Panel, Player, PlayerLook,
    PlayerMotion, Request, ServerPlayer, Snapshot, Status,
};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

/// One mod in the sandbox, driven by a test.
pub struct TestGame {
    mods: Mods,
    dir: PathBuf,
    hmod: PathBuf,
    /// What the mod reads with `game-read`; ticks move the clock as the game does.
    pub player: Option<Player>,
    pub clock: Option<Clock>,
    /// The players on the server, for a server mod, in join order.
    pub players: Vec<ServerPlayer>,
    /// Their looks and permission levels, by id.
    pub looks: Vec<PlayerLook>,
    /// Their motion, by id: a player joins standing.
    pub motions: Vec<PlayerMotion>,
    /// Every name that joined, for their id.
    names: Vec<String>,
    tick: u64,
    /// Capability `world`'s Overworld: the blocks a script set or the mod
    /// changed; every other block, and the other dimensions, are air. Every
    /// biome is the dimension's plainest: plains, nether wastes, the end.
    pub world: std::collections::BTreeMap<(i32, i32, i32), String>,
}

/// The world a script's mod reads: [`TestGame::world`], copied for a step.
struct ScriptWorld(std::collections::BTreeMap<(i32, i32, i32), String>);

impl crate::WorldView for ScriptWorld {
    fn block(&self, dimension: &str, x: i32, y: i32, z: i32) -> Option<String> {
        let here = (dimension == "minecraft:overworld")
            .then(|| self.0.get(&(x, y, z)).cloned())
            .flatten();
        Some(here.unwrap_or_else(|| "minecraft:air".into()))
    }
    fn biome(&self, dimension: &str, _x: i32, _y: i32, _z: i32) -> Option<String> {
        let biome = match dimension {
            "minecraft:overworld" => "minecraft:plains",
            "minecraft:the_nether" => "minecraft:nether_wastes",
            "minecraft:the_end" => "minecraft:the_end",
            _ => return None,
        };
        Some(biome.into())
    }
    fn has_dimension(&self, dimension: &str) -> bool {
        matches!(
            dimension,
            "minecraft:overworld" | "minecraft:the_nether" | "minecraft:the_end"
        )
    }
    fn check_state(&self, state: &str) -> Result<(), String> {
        // No registry here: the shape only (the game checks the names).
        let name = state.split('[').next().unwrap_or_default();
        let plain = |t: &str| {
            !t.is_empty()
                && t.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-/".contains(&b))
        };
        let ok = match name.split_once(':') {
            Some((ns, path)) => plain(ns) && plain(path),
            None => plain(name),
        };
        let closed = !state.contains('[') || state.ends_with(']');
        (ok && closed)
            .then_some(())
            .ok_or_else(|| format!("{state} is not a block state"))
    }
}

/// Where a script starts: standing at 0.5 64 0.5 in the Overworld, facing
/// south, full health and food, at the first tick of a new world's first morning.
pub fn default_player() -> Player {
    Player {
        x: 0.5,
        y: 64.0,
        z: 0.5,
        yaw: 0.0,
        pitch: 0.0,
        health: 20.0,
        food: 20,
        dimension: "minecraft:overworld".into(),
    }
}

impl TestGame {
    /// Starts the mod at `hmod`, as the game would with it turned on. Errs
    /// when it doesn't start, with the game's reason.
    pub fn load(hmod: &Path) -> Result<TestGame, String> {
        static RUN: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "horizon-mod-test-{}-{}",
            std::process::id(),
            RUN.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let copy = dir.join(hmod.file_name().unwrap_or("mod.hmod".as_ref()));
        let mut game = TestGame {
            mods: Mods::default(),
            hmod: copy.clone(),
            dir,
            player: Some(default_player()),
            clock: Some(Clock {
                game_time: 0,
                day_time: 0,
            }),
            players: Vec::new(),
            looks: Vec::new(),
            motions: Vec::new(),
            names: Vec::new(),
            tick: 0,
            world: Default::default(),
        };
        copy_mod(hmod, &copy)?;
        game.start()?;
        Ok(game)
    }

    fn start(&mut self) -> Result<(), String> {
        self.mods = Mods::load_simulated(std::slice::from_ref(&self.hmod));
        match &self.mods.entries()[0].status {
            Status::Failed(reason) => Err(format!("the mod didn't start: {reason}")),
            _ if !self.mods.is_running() => Err("the mod has no code to run".into()),
            _ => Ok(()),
        }
    }

    /// Stops the mod and starts it again, as a rebuild does in the game: its
    /// memory starts over, its `storage` stays.
    pub fn reload(&mut self) -> Result<(), String> {
        self.mods = Mods::default();
        self.start()
    }

    fn snapshot(&mut self) {
        self.mods.set_snapshot(Snapshot {
            player: self.player.clone(),
            clock: self.clock,
            players: self.players.clone(),
            player_looks: self.looks.clone(),
            motions: self.motions.clone(),
            // The GUI of a 1280 x 720 window at scale 3, as the trial run's.
            gui_size: (427.0, 240.0),
            ..Snapshot::default()
        });
    }
    /// Whether the mod is a server mod.
    pub fn server(&self) -> bool {
        self.mods.runs_server()
    }

    /// `n` game ticks; each moves the clock one tick on, as the game does.
    pub fn tick(&mut self, n: u64) {
        for _ in 0..n {
            self.tick += 1;
            if let Some(clock) = &mut self.clock {
                clock.game_time += 1;
                clock.day_time += 1;
            }
            self.snapshot();
            self.mods.tick(self.tick);
            self.mods.server_tick(self.tick);
        }
    }
    /// `n` frames, 1/60 s apart.
    pub fn frames(&mut self, n: u64) {
        self.snapshot();
        for _ in 0..n {
            self.mods.frame(1.0 / 60.0);
        }
    }
    /// A chat line the player receives.
    pub fn chat(&mut self, line: &str) {
        self.snapshot();
        self.mods.chat(line);
    }
    /// `/line` typed by the player (without the slash); false when the mod
    /// owns no such command.
    pub fn command(&mut self, line: &str) -> bool {
        self.snapshot();
        self.mods.command(line)
    }

    /// A player named `name` joins the server, standing where a script starts
    /// in survival. Their id is `00000000-0000-4000-8000-000000000001` for
    /// the first name that joined, `...0002` for the second, and so on.
    pub fn join(&mut self, name: &str) -> ServerPlayer {
        let n = match self.names.iter().position(|n| n == name) {
            Some(i) => i + 1,
            None => {
                self.names.push(name.to_owned());
                self.names.len()
            }
        };
        let p = default_player();
        let player = ServerPlayer {
            id: format!("00000000-0000-4000-8000-{n:012x}"),
            name: name.to_owned(),
            x: p.x,
            y: p.y,
            z: p.z,
            dimension: p.dimension,
            game_mode: GameMode::Survival,
            health: p.health,
        };
        self.players.push(player.clone());
        if !self.looks.iter().any(|l| l.id == player.id) {
            self.looks.push(PlayerLook {
                id: player.id.clone(),
                ..PlayerLook::default()
            });
        }
        if !self.motions.iter().any(|m| m.id == player.id) {
            self.motions.push(PlayerMotion {
                id: player.id.clone(),
                on_ground: true,
                ..PlayerMotion::default()
            });
        }
        self.snapshot();
        self.mods.player_joined(&player);
        player
    }
    /// The player named `name` leaves; false when no such player is on.
    pub fn leave(&mut self, name: &str) -> bool {
        let Some(i) = self.players.iter().position(|p| p.name == name) else {
            return false;
        };
        let player = self.players.remove(i);
        self.snapshot();
        self.mods.player_left(&player);
        true
    }
    /// A chat message `player` sends, for a server mod.
    pub fn player_chat(&mut self, player: &ServerPlayer, line: &str) {
        self.snapshot();
        self.mods.player_chat(player, line);
    }
    /// `/line` typed by `player`, for a server mod; false when the mod owns
    /// no such command.
    pub fn player_command(&mut self, player: &ServerPlayer, line: &str) -> bool {
        self.snapshot();
        self.mods.server_command(player, line)
    }
    /// `player` breaks (or places) `state` at `pos`.
    pub fn block(&mut self, player: &ServerPlayer, pos: BlockPos, state: &str, placed: bool) {
        self.snapshot();
        if placed {
            self.mods.block_placed(player, pos, state);
        } else {
            self.mods.block_broken(player, pos, state);
        }
    }

    /// `player`'s swap-hands key: true when the mod took it.
    pub fn swap_hands(&mut self, player: &ServerPlayer) -> bool {
        self.snapshot();
        self.mods.swap_hands(player)
    }
    /// A double jump of `player`.
    pub fn double_jump(&mut self, player: &ServerPlayer) {
        self.snapshot();
        self.mods.double_jump(player);
    }
    /// A payload from `player`: false when the mod doesn't listen to its channel.
    pub fn payload(&mut self, player: &ServerPlayer, channel: &str, data: &[u8]) -> bool {
        self.snapshot();
        self.mods.payload(player, channel, data)
    }
    /// Datagrams to the mod's UDP socket.
    pub fn udp(&mut self, datagrams: &[Datagram]) {
        self.snapshot();
        self.mods.udp_arrived(datagrams);
    }
    /// A request to the mod's HTTP server: status, content type and body; none when it
    /// serves none.
    pub fn http(&mut self, request: &Request) -> Option<(u16, String, Vec<u8>)> {
        self.snapshot();
        let r = self.mods.http_request(request)?;
        Some((r.status, r.content_type, r.body))
    }

    /// The scratch copy the mod runs from; its `storage` is beside it.
    pub fn hmod(&self) -> &Path {
        &self.hmod
    }
    /// The commands the mod registered.
    pub fn commands(&self) -> Vec<String> {
        let server = self.mods.server_commands().map(|c| c.name.as_str());
        self.mods
            .commands()
            .chain(server)
            .map(str::to_owned)
            .collect()
    }
    pub fn panel(&self) -> Option<&Panel> {
        self.mods.panels().next()
    }
    pub fn status(&self) -> &Status {
        &self.mods.entries()[0].status
    }
    /// What the mod showed since the last call.
    pub fn drain(&mut self) -> Vec<Output> {
        self.mods.drain()
    }
}

impl Drop for TestGame {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A `.hmod` file, or a mod folder (`horizon-mod.toml` + `mod.wasm`).
fn copy_mod(from: &Path, to: &Path) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{}: {e}", from.display());
    if !from.is_dir() {
        return std::fs::copy(from, to).map(drop).map_err(err);
    }
    std::fs::create_dir_all(to).map_err(err)?;
    for entry in std::fs::read_dir(from).map_err(err)? {
        let entry = entry.map_err(err)?;
        if entry.file_type().map_err(err)?.is_file() {
            std::fs::copy(entry.path(), to.join(entry.file_name())).map_err(err)?;
        }
    }
    Ok(())
}

/// A test in TOML: where a client mod's player starts, then steps run in order.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Script {
    #[serde(default)]
    pub player: PlayerSet,
    #[serde(default)]
    pub clock: ClockSet,
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
}

/// Fields of the player to change; the others keep their values. A server
/// mod's players have no `food`, and a client mod's player no `game-mode` or
/// `permission` (0 to 4; a player joins with 0).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct PlayerSet {
    x: Option<f64>,
    y: Option<f64>,
    z: Option<f64>,
    yaw: Option<f32>,
    pitch: Option<f32>,
    health: Option<f32>,
    food: Option<u32>,
    dimension: Option<String>,
    game_mode: Option<Mode>,
    permission: Option<u8>,
    /// Server mods' `movement.motion-of`: a player joins standing.
    on_ground: Option<bool>,
    gliding: Option<bool>,
    fall_distance: Option<f64>,
    riding: Option<bool>,
}

/// `players.game-mode` as a script spells it.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Survival,
    Creative,
    Adventure,
    Spectator,
}
impl From<Mode> for GameMode {
    fn from(m: Mode) -> GameMode {
        match m {
            Mode::Survival => GameMode::Survival,
            Mode::Creative => GameMode::Creative,
            Mode::Adventure => GameMode::Adventure,
            Mode::Spectator => GameMode::Spectator,
        }
    }
}

/// A block a player breaks or places.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockSet {
    x: i32,
    y: i32,
    z: i32,
    state: String,
}

/// A payload a player sends: its channel, and its bytes in hex.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadSet {
    channel: String,
    #[serde(default)]
    data: String,
}

/// A datagram to the mod's socket: its sender (`ip:port`) and its bytes in hex.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatagramSet {
    from: String,
    #[serde(default)]
    data: String,
}

/// Bytes written as hex, `"0a1b"`, spaces allowed.
fn unhex(text: &str) -> Result<Vec<u8>, String> {
    let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !digits.len().is_multiple_of(2) {
        return Err(format!("{text:?} is not hex: an odd number of digits"));
    }
    digits
        .chunks(2)
        .map(|d| {
            let d = std::str::from_utf8(d).unwrap_or("");
            u8::from_str_radix(d, 16).map_err(|_| format!("{text:?} is not hex"))
        })
        .collect()
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ClockSet {
    game_time: Option<u64>,
    day_time: Option<u64>,
}

/// One step. Its parts happen in this order: `reload`, `title-screen`,
/// `blocks`, `join`, `player`, `clock`, `ticks`, `frames`, `chat`, `command`,
/// `break`, `place`, `leave`; then the blocks the mod set are made, and
/// `expect` is checked against what the mod showed and changed during the
/// step. For a server mod, `player`, `chat`, `command`, `break`
/// and `place` are the doing of the player named by `as`, or of the first
/// player on.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Step {
    #[serde(default)]
    reload: bool,
    /// Leave the world: the mod reads no player and no clock until a step sets them.
    #[serde(default)]
    title_screen: bool,
    player: Option<PlayerSet>,
    clock: Option<ClockSet>,
    #[serde(default)]
    ticks: u64,
    #[serde(default)]
    frames: u64,
    chat: Option<String>,
    command: Option<String>,
    /// Server mods: a player joins, leaves, breaks or places a block.
    join: Option<String>,
    leave: Option<String>,
    #[serde(rename = "as")]
    actor: Option<String>,
    #[serde(rename = "break")]
    broken: Option<BlockSet>,
    place: Option<BlockSet>,
    /// Server plugins, after `place`: the player presses the swap-hands key, double-jumps,
    /// sends a payload; datagrams reach the plugin's socket; a `GET` of this path reaches
    /// its HTTP server.
    #[serde(default)]
    swap_hands: bool,
    #[serde(default)]
    double_jump: bool,
    payload: Option<PayloadSet>,
    #[serde(default)]
    udp: Vec<DatagramSet>,
    http: Option<String>,
    /// Blocks of the Overworld before the rest of the step (capability
    /// `world`); every other block is air.
    #[serde(default)]
    blocks: Vec<BlockSet>,
    #[serde(default)]
    expect: Expect,
}

/// What a step checks; a part left out is not checked.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Expect {
    /// The commands the mod owns.
    commands: Option<Vec<String>>,
    /// Every chat line shown during the step, in order.
    chat: Option<Vec<String>>,
    /// The command's feedback, and its red error.
    feedback: Option<String>,
    error: Option<String>,
    /// Server mods: every `players.tell` as `"Name: text"`, and every
    /// `players.broadcast`, in order.
    told: Option<Vec<String>>,
    broadcast: Option<Vec<String>>,
    /// Server mods: every `players.action-bar` as `"Name: text"`, in order.
    action_bar: Option<Vec<String>>,
    /// Server mods: every `players.teleport` as `"Name: dimension x y z"`, in
    /// order; the player is moved there for the next steps.
    teleported: Option<Vec<String>>,
    /// Server plugins (capability `movement`): every `glide` as `"Name"`, every
    /// `allow-double-jump` as `"Name: true"`, every `push` as `"Name: x y z"`, in order;
    /// and whether the swap-hands key was taken. A glide makes the player glide for the
    /// next steps.
    glides: Option<Vec<String>>,
    double_jump: Option<Vec<String>>,
    pushed: Option<Vec<String>>,
    swap_taken: Option<bool>,
    /// Every `channels.send` as `"Name: channel hex"`, and every datagram sent as
    /// `"ip:port hex"`, in order.
    payloads: Option<Vec<String>>,
    udp: Option<Vec<String>>,
    /// The `http` request's response: its status, content type, and text its body holds.
    http_status: Option<u16>,
    http_type: Option<String>,
    http_body: Option<String>,
    /// The panel's lines (`[]`: no panel) and its corner.
    panel: Option<Vec<String>>,
    corner: Option<String>,
    /// Server plugins (capability `world`): the regions of the step's last `protect`, each as
    /// `"dimension x1 y1 z1 x2 y2 z2 deny=break-blocks,pvp exempt=1: message"`.
    protected: Option<Vec<String>>,
    /// Blocks of the Overworld after the step, the mod's changes made
    /// (capability `world`).
    blocks: Option<Vec<BlockSet>>,
    /// The mod is turned off during this step, for a reason holding this text.
    /// Without it, a mod that stops fails the test.
    stopped: Option<String>,
}

impl Script {
    pub fn parse(text: &str) -> Result<Script, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    /// Runs every step on `game`; the error names the first step that failed.
    pub fn run(&self, game: &mut TestGame) -> Result<(), String> {
        if let Some(p) = &mut game.player {
            self.player.apply(p);
        }
        if let Some(c) = &mut game.clock {
            self.clock.apply(c);
        }
        for (i, step) in self.steps.iter().enumerate() {
            step.run(game)
                .map_err(|e| format!("step {} ({}): {e}", i + 1, step.describe()))?;
        }
        Ok(())
    }
}

impl PlayerSet {
    fn apply(&self, p: &mut Player) {
        let set = |to: &mut f64, v: Option<f64>| *to = v.unwrap_or(*to);
        set(&mut p.x, self.x);
        set(&mut p.y, self.y);
        set(&mut p.z, self.z);
        p.yaw = self.yaw.unwrap_or(p.yaw);
        p.pitch = self.pitch.unwrap_or(p.pitch);
        p.health = self.health.unwrap_or(p.health);
        p.food = self.food.unwrap_or(p.food);
        if let Some(d) = &self.dimension {
            p.dimension.clone_from(d);
        }
    }
    fn apply_server(&self, p: &mut ServerPlayer, look: Option<&mut PlayerLook>) {
        if let Some(l) = look {
            l.yaw = self.yaw.unwrap_or(l.yaw);
            l.pitch = self.pitch.unwrap_or(l.pitch);
            l.permission = self.permission.unwrap_or(l.permission);
        }
        let set = |to: &mut f64, v: Option<f64>| *to = v.unwrap_or(*to);
        set(&mut p.x, self.x);
        set(&mut p.y, self.y);
        set(&mut p.z, self.z);
        p.health = self.health.unwrap_or(p.health);
        p.game_mode = self.game_mode.map_or(p.game_mode, GameMode::from);
        if let Some(d) = &self.dimension {
            p.dimension.clone_from(d);
        }
    }
}

impl PlayerSet {
    fn apply_motion(&self, m: &mut PlayerMotion) {
        m.on_ground = self.on_ground.unwrap_or(m.on_ground);
        m.gliding = self.gliding.unwrap_or(m.gliding);
        m.fall_distance = self.fall_distance.unwrap_or(m.fall_distance);
        m.riding = self.riding.unwrap_or(m.riding);
    }
}

impl ClockSet {
    fn apply(&self, c: &mut Clock) {
        c.game_time = self.game_time.unwrap_or(c.game_time);
        c.day_time = self.day_time.unwrap_or(c.day_time);
    }
}

impl Step {
    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.reload {
            parts.push("reload".to_string());
        }
        let count = |n: u64, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
        if self.ticks > 0 {
            parts.push(count(self.ticks, "tick"));
        }
        if self.frames > 0 {
            parts.push(count(self.frames, "frame"));
        }
        if let Some(line) = &self.chat {
            parts.push(format!("chat {line:?}"));
        }
        if let Some(line) = &self.command {
            parts.push(format!("/{line}"));
        }
        if let Some(name) = &self.join {
            parts.insert(0, format!("{name} joins"));
        }
        for (what, b) in [("breaks", &self.broken), ("places", &self.place)] {
            if let Some(b) = b {
                parts.push(format!("{what} {} at {} {} {}", b.state, b.x, b.y, b.z));
            }
        }
        if self.swap_hands {
            parts.push("swap-hands".into());
        }
        if self.double_jump {
            parts.push("double jump".into());
        }
        if let Some(p) = &self.payload {
            parts.push(format!("payload {}", p.channel));
        }
        if !self.udp.is_empty() {
            parts.push(format!("{} datagrams", self.udp.len()));
        }
        if let Some(path) = &self.http {
            parts.push(format!("GET {path}"));
        }
        if let Some(name) = &self.leave {
            parts.push(format!("{name} leaves"));
        }
        if parts.is_empty() {
            parts.push("checks".into());
        }
        parts.join(", ")
    }

    fn run(&self, game: &mut TestGame) -> Result<(), String> {
        if self.reload {
            game.reload()?;
        }
        if self.title_screen {
            game.player = None;
            game.clock = None;
        }
        for b in &self.blocks {
            game.world.insert((b.x, b.y, b.z), b.state.clone());
        }
        game.mods
            .lend_world(Box::new(ScriptWorld(game.world.clone())));
        if let Some(name) = &self.join {
            game.join(name);
        }
        let server = game.server();
        let actor = |game: &TestGame| match &self.actor {
            Some(name) => game
                .players
                .iter()
                .position(|p| p.name == *name)
                .ok_or_else(|| format!("{name} is not on the server")),
            None => (!game.players.is_empty())
                .then_some(0)
                .ok_or_else(|| "no player is on the server: add join = \"Name\"".to_string()),
        };
        let acts = self.player.is_some()
            || self.chat.is_some()
            || self.command.is_some()
            || self.broken.is_some()
            || self.place.is_some()
            || self.swap_hands
            || self.double_jump
            || self.payload.is_some();
        let actor = if server && acts {
            Some(actor(game)?)
        } else {
            None
        };
        if let Some(set) = &self.player {
            match actor {
                Some(i) => {
                    let id = game.players[i].id.clone();
                    let look = game.looks.iter_mut().find(|l| l.id == id);
                    set.apply_server(&mut game.players[i], look);
                    if let Some(m) = game.motions.iter_mut().find(|m| m.id == id) {
                        set.apply_motion(m);
                    }
                }
                None => set.apply(game.player.get_or_insert_with(default_player)),
            }
        }
        if let Some(set) = &self.clock {
            set.apply(game.clock.get_or_insert(Clock {
                game_time: 0,
                day_time: 0,
            }));
        }
        game.tick(self.ticks);
        game.frames(self.frames);
        let who = actor.map(|i| game.players[i].clone());
        if let Some(line) = &self.chat {
            match &who {
                Some(p) => game.player_chat(p, line),
                None => game.chat(line),
            }
        }
        let owned = self.command.as_ref().map(|line| match &who {
            Some(p) => game.player_command(p, line),
            None => game.command(line),
        });
        for (b, placed) in [(&self.broken, false), (&self.place, true)] {
            if let (Some(b), Some(p)) = (b, &who) {
                let pos = BlockPos {
                    x: b.x,
                    y: b.y,
                    z: b.z,
                };
                game.block(p, pos, &b.state, placed);
            }
        }
        let mut swap_taken = None;
        if let Some(p) = who.as_ref().filter(|_| self.swap_hands) {
            swap_taken = Some(game.swap_hands(p));
        }
        if let Some(p) = who.as_ref().filter(|_| self.double_jump) {
            game.double_jump(p);
        }
        if let (Some(set), Some(p)) = (&self.payload, &who)
            && !game.payload(p, &set.channel, &unhex(&set.data)?)
        {
            return Err(format!("the mod doesn't listen to {}", set.channel));
        }
        if !self.udp.is_empty() {
            let datagrams = self
                .udp
                .iter()
                .map(|d| {
                    Ok(Datagram {
                        address: d.from.clone(),
                        data: unhex(&d.data)?,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            game.udp(&datagrams);
        }
        let response = match &self.http {
            Some(path) => {
                let request = Request {
                    method: "GET".into(),
                    path: path.clone(),
                    body: Vec::new(),
                };
                Some(game.http(&request).ok_or("the mod serves no HTTP")?)
            }
            None => None,
        };
        if let Some(name) = &self.leave
            && !game.leave(name)
        {
            return Err(format!("{name} is not on the server"));
        }
        game.mods.take_world();
        let out = game.drain();
        for o in &out {
            if let Output::SetBlocks {
                dimension, blocks, ..
            } = o
                && dimension == "minecraft:overworld"
            {
                for b in blocks {
                    game.world.insert((b.x, b.y, b.z), b.state.clone());
                }
            }
        }
        let name = |players: &[ServerPlayer], id: &str| {
            let p = players.iter().find(|p| p.id == id);
            p.map_or(id.to_owned(), |p| p.name.clone())
        };
        let mut teleported = Vec::new();
        for o in &out {
            if let Output::Teleport {
                player,
                dimension,
                at,
                yaw,
                pitch,
                ..
            } = o
            {
                let [x, y, z] = *at;
                teleported.push(format!(
                    "{}: {dimension} {x} {y} {z}",
                    name(&game.players, player)
                ));
                if let Some(p) = game.players.iter_mut().find(|p| p.id == *player) {
                    (p.x, p.y, p.z) = (x, y, z);
                    p.dimension.clone_from(dimension);
                }
                if let Some(l) = game.looks.iter_mut().find(|l| l.id == *player) {
                    (l.yaw, l.pitch) = (*yaw, *pitch);
                }
            }
        }
        let action_bar: Vec<String> = out
            .iter()
            .filter_map(|o| match o {
                Output::ActionBar { player, text, .. } => {
                    Some(format!("{}: {text}", name(&game.players, player)))
                }
                _ => None,
            })
            .collect();
        let mut moved = (Vec::new(), Vec::new(), Vec::new());
        let (mut payloads, mut udp) = (Vec::new(), Vec::new());
        for o in &out {
            match o {
                Output::Glide { player } => {
                    moved.0.push(name(&game.players, player));
                    if let Some(m) = game.motions.iter_mut().find(|m| m.id == *player) {
                        m.gliding = true;
                    }
                }
                Output::AllowDoubleJump { player, allow } => {
                    moved
                        .1
                        .push(format!("{}: {allow}", name(&game.players, player)));
                }
                Output::Push {
                    player,
                    velocity: [x, y, z],
                } => moved
                    .2
                    .push(format!("{}: {x} {y} {z}", name(&game.players, player))),
                Output::Payload {
                    player,
                    channel,
                    data,
                } => payloads.push(format!(
                    "{}: {channel} {}",
                    name(&game.players, player),
                    hex(data)
                )),
                Output::UdpSent { address, data } => udp.push(format!("{address} {}", hex(data))),
                _ => {}
            }
        }
        let protected: Vec<String> = out
            .iter()
            .rev()
            .find_map(|o| match o {
                Output::Protect { regions, .. } => Some(regions.iter().map(region_line).collect()),
                _ => None,
            })
            .unwrap_or_default();
        let stopped = out.iter().find_map(|o| match o {
            Output::Stopped { reason, .. } => Some(reason.as_str()),
            _ => None,
        });
        let e = &self.expect;
        match (stopped, &e.stopped) {
            (Some(reason), Some(want)) if !reason.contains(want.as_str()) => {
                return Err(format!("the mod stopped ({reason}), expected {want:?}"));
            }
            (Some(_), Some(_)) => return Ok(()),
            (Some(reason), None) => return Err(format!("the mod stopped: {reason}")),
            (None, Some(want)) => {
                return Err(format!(
                    "the mod kept running, expected it to stop ({want:?})"
                ));
            }
            (None, None) => {}
        }
        if owned == Some(false) {
            let line = self.command.as_deref().unwrap_or_default();
            let name = line.split(' ').next().unwrap_or_default();
            return Err(format!("the mod owns no command /{name}"));
        }
        if let Some(want) = &e.commands {
            same("commands", want, &game.commands())?;
        }
        if let Some(want) = &e.chat {
            let got: Vec<String> = out
                .iter()
                .filter_map(|o| match o {
                    Output::Chat { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect();
            same("chat", want, &got)?;
        }
        let feedback = |error: bool| {
            out.iter().rev().find_map(|o| match o {
                Output::Feedback { text, error: e } | Output::Reply { text, error: e, .. }
                    if *e == error =>
                {
                    Some(text.clone())
                }
                _ => None,
            })
        };
        if let Some(want) = &e.told {
            let name = |id: &str| {
                let p = game.players.iter().find(|p| p.id == id);
                p.map_or(id.to_owned(), |p| p.name.clone())
            };
            let got: Vec<String> = out
                .iter()
                .filter_map(|o| match o {
                    Output::Tell { player, text, .. } => Some(format!("{}: {text}", name(player))),
                    _ => None,
                })
                .collect();
            same("told", want, &got)?;
        }
        if let Some(want) = &e.broadcast {
            let got: Vec<String> = out
                .iter()
                .filter_map(|o| match o {
                    Output::Broadcast { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect();
            same("broadcast", want, &got)?;
        }
        if let Some(want) = &e.action_bar {
            same("action-bar", want, &action_bar)?;
        }
        if let Some(want) = &e.teleported {
            same("teleported", want, &teleported)?;
        }
        if let Some(want) = &e.glides {
            same("glides", want, &moved.0)?;
        }
        if let Some(want) = &e.double_jump {
            same("double-jump", want, &moved.1)?;
        }
        if let Some(want) = &e.pushed {
            same("pushed", want, &moved.2)?;
        }
        if let Some(want) = e.swap_taken {
            same("swap-taken", &Some(want), &swap_taken)?;
        }
        if let Some(want) = &e.payloads {
            same("payloads", want, &payloads)?;
        }
        if let Some(want) = &e.udp {
            same("udp", want, &udp)?;
        }
        if let Some(want) = &e.protected {
            same("protected", want, &protected)?;
        }
        if let Some((status, kind, body)) = &response {
            if let Some(want) = e.http_status {
                same("http-status", &want, status)?;
            }
            if let Some(want) = &e.http_type {
                same("http-type", want, kind)?;
            }
            if let Some(want) = &e.http_body {
                let text = String::from_utf8_lossy(body);
                if !text.contains(want.as_str()) {
                    return Err(format!("http-body: {want:?} is not in {text:?}"));
                }
            }
        }
        if let Some(want) = &e.feedback {
            same("feedback", &Some(want.clone()), &feedback(false))?;
        }
        if let Some(want) = &e.error {
            same("error", &Some(want.clone()), &feedback(true))?;
        }
        if let Some(want) = &e.panel {
            let got: Vec<String> = game
                .panel()
                .map(|p| p.lines.iter().map(|l| l.text.clone()).collect())
                .unwrap_or_default();
            same("panel", want, &got)?;
        }
        if let Some(want) = &e.blocks {
            for b in want {
                let got = game.world.get(&(b.x, b.y, b.z));
                let got = got.map_or("minecraft:air", String::as_str);
                if got != b.state {
                    return Err(format!(
                        "block at {} {} {}: expected {}, got {got}",
                        b.x, b.y, b.z, b.state
                    ));
                }
            }
        }
        if let Some(want) = &e.corner {
            let got = game.panel().map(|p| corner_name(p.corner).to_string());
            same("corner", &Some(want.clone()), &got)?;
        }
        Ok(())
    }
}

fn corner_name(c: Corner) -> &'static str {
    match c {
        Corner::TopLeft => "top-left",
        Corner::TopRight => "top-right",
        Corner::BottomLeft => "bottom-left",
        Corner::BottomRight => "bottom-right",
    }
}

fn same<T: PartialEq + std::fmt::Debug>(what: &str, want: &T, got: &T) -> Result<(), String> {
    if want == got {
        return Ok(());
    }
    Err(format!(
        "{what}\n    expected: {want:?}\n    got:      {got:?}"
    ))
}

/// A protected region as `expect.protected` writes it.
fn region_line(r: &crate::Region) -> String {
    use crate::ProtectedAction as A;
    let deny: Vec<&str> = r
        .deny
        .iter()
        .map(|a| match a {
            A::BreakBlocks => "break-blocks",
            A::PlaceBlocks => "place-blocks",
            A::UseBlocks => "use-blocks",
            A::Containers => "containers",
            A::Pvp => "pvp",
            A::Damage => "damage",
            A::HurtMobs => "hurt-mobs",
        })
        .collect();
    let ((x1, y1, z1), (x2, y2, z2)) = (r.pos1, r.pos2);
    format!(
        "{} {x1} {y1} {z1} {x2} {y2} {z2} deny={} exempt={}: {}",
        r.dimension,
        deny.join(","),
        r.exempt.len(),
        r.message
    )
}
