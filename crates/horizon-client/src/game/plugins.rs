//! Server plugins (`docs/plugins.md`): WebAssembly server mods (`side = "server"`,
//! `crates/horizon-mod-sdk`) a dedicated server loads from `plugins/` beside its world. Each runs
//! in its own sandbox (a memory cap and a time budget per call, no file access but its own
//! `data/`, the network only through capability `network`'s two sockets); one that traps or
//! overruns is turned off and the server goes on.
//!
//! At the end of each server tick they hear who joined and left, then what players said, broke and
//! placed since the last one, then tick; a player's command reaches them at once. Their lines go to
//! the players they name, the blocks they set are made level by level. With no plugin, each hook
//! here is one check. Built with the `plugins` feature (wasmtime); without it nothing loads.
use super::{Game, lan::LanHost};
use horizon_protocol::packets::play::{self, Clientbound as C};
use horizon_world::registry::BlockStateId;

#[cfg(feature = "plugins")]
mod io;

/// How often, between ticks, the server looks at the plugins' sockets.
pub(super) const PUMP_EVERY: std::time::Duration = std::time::Duration::from_millis(5);

/// Who did something a plugin hears about, resolved at the tick's end.
#[cfg_attr(not(feature = "plugins"), allow(dead_code))]
enum Who {
    Conn(u32),
    Uuid(u128),
}

/// What players did during a tick, for the plugins at its end.
#[cfg_attr(not(feature = "plugins"), allow(dead_code))]
enum Event {
    Chat(Who, String),
    Block {
        who: Who,
        pos: [i32; 3],
        state: String,
        placed: bool,
    },
    /// A double jump a plugin allowed (capability `movement`).
    DoubleJump(Who),
}

/// The plugins and what is queued for them.
#[derive(Default)]
pub(super) struct Plugins {
    #[cfg(feature = "plugins")]
    mods: horizon_mods::Mods,
    /// `plugins/` was read (once, on the first tick).
    started: bool,
    /// A plugin runs: what players do is queued for it.
    running: bool,
    /// A running plugin has capability `world`.
    #[cfg_attr(not(feature = "plugins"), allow(dead_code))]
    world: bool,
    /// The players plugins saw last tick, for joins and leaves.
    #[cfg(feature = "plugins")]
    players: Vec<horizon_mods::ServerPlayer>,
    events: Vec<Event>,
    /// Blocks plugins set, by dimension, made before that level's next tick.
    #[cfg(feature = "plugins")]
    block_writes: Vec<(String, horizon_mods::BlockChange)>,
    /// Command lines plugins queued, run as the console at the tick's end: (line, quiet).
    #[cfg(feature = "plugins")]
    console: Vec<(String, bool)>,
    /// `/plugins reload`: the ticks until `plugins/` is read again, and who asked.
    #[cfg(feature = "plugins")]
    reload: Option<(u32, u32)>,
    /// How many reloads ran: part of the command tree's key, since a fresh set of plugins
    /// starts its generation again.
    #[cfg(feature = "plugins")]
    reloads: u64,
    /// Plugins' protected regions (`world.protect`), with the plugin that gave each.
    #[cfg(feature = "plugins")]
    pub(super) regions: Vec<(String, horizon_mods::Region)>,
    /// The guests a region keeps from damage now (`game/protection.rs`).
    #[cfg(feature = "plugins")]
    pub(super) shielded: std::collections::HashSet<u128>,
}

impl Game<'_> {
    /// A player broke (or placed) `state` at `pos`: the connection acting now.
    pub(super) fn plugin_block_event(&mut self, pos: [i32; 3], state: BlockStateId, placed: bool) {
        if !self.plugins.running {
            return;
        }
        let Some(conn) = self.lan_acting else {
            return;
        };
        let state = self.state_string(state);
        self.plugins.events.push(Event::Block {
            who: Who::Conn(conn),
            pos,
            state,
            placed,
        });
    }

    /// Once a tick for a guest, before its glide tick: a plugin's glide spares the fall
    /// damage until it ends, and a double jump waits for the plugins at the tick's end (the
    /// guest's game is told it doesn't fly after all).
    pub(super) fn plugin_guest_tick(&mut self, host: &mut LanHost, conn: u32) {
        let Some(g) = host.hub.guest_mut(conn) else {
            return;
        };
        if g.plugin_glide {
            if let Some(m) = g.motion.as_mut() {
                m.fall_distance = 0.0;
            }
            if g.on_ground || !g.fall_flying || g.creative() || g.spectator() {
                g.plugin_glide = false;
            }
        }
        if std::mem::take(&mut g.plugin_jump) {
            g.send(&C::PlayerAbilities(play::PlayerAbilities {
                flags: g.ability_flags(),
                flying_speed: 0.05,
                walking_speed: 0.1,
            }));
            if self.plugins.running {
                self.plugins.events.push(Event::DoubleJump(Who::Conn(conn)));
            }
        }
    }

    /// A chat message a player sent.
    pub(super) fn plugin_chat(&mut self, sender: u128, text: &str) {
        if self.plugins.running {
            self.plugins
                .events
                .push(Event::Chat(Who::Uuid(sender), text.to_owned()));
        }
    }
}

#[cfg(not(feature = "plugins"))]
impl Game<'_> {
    pub(super) fn tick_plugins(&mut self) {
        if !self.plugins.started && self.options.dedicated.is_some() {
            self.plugins.started = true;
            let dir = self.plugins_dir();
            if !plugin_paths(&dir).is_empty() {
                super::dedicated::log(
                    "WARN",
                    "plugins/ holds plugins, but this build runs none (built without the `plugins` feature)",
                );
            }
        }
    }
    pub(super) fn plugin_command(&mut self, _host: &mut LanHost, _conn: u32, _line: &str) -> bool {
        false
    }
    pub(super) fn plugins_command(
        &mut self,
        _host: &LanHost,
        _conn: u32,
        _args: &[&str],
    ) -> horizon_client::text::Text {
        horizon_client::text::Text::legacy("§cThis server was built without plugins.")
    }
    pub(super) fn apply_plugin_block_writes(&mut self) {}
    pub(super) fn plugin_swap_hands(&mut self, _host: &mut LanHost, _conn: u32) -> bool {
        false
    }
    pub(super) fn plugin_payload(
        &mut self,
        _host: &mut LanHost,
        _conn: u32,
        _channel: &str,
        _data: &[u8],
    ) -> bool {
        false
    }
    pub(super) fn pump_plugins(&mut self) -> bool {
        false
    }
}

impl Game<'_> {
    /// `plugins/` beside the world folder.
    fn plugins_dir(&self) -> std::path::PathBuf {
        self.world_root
            .parent()
            .map_or_else(|| "plugins".into(), |p| p.join("plugins"))
    }
}

/// The plugins in `plugins/`: one per folder, `plugins/<name>/<plugin>.hmod` (or an unpacked
/// plugin, a folder holding `horizon-mod.toml`), its data beside it in `plugins/<name>/data/`.
/// A `.hmod` loose in `plugins/` and a folder's second `.hmod` are logged and left out: two
/// plugins would share one `data/`.
fn plugin_paths(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let warn = |line: String| super::dedicated::log("WARN", &line);
    let mut found = Vec::new();
    for path in horizon_mods::find(dir) {
        if path.is_file() {
            let name = path.file_stem().unwrap_or_default().to_string_lossy();
            warn(format!(
                "{} is not loaded: a plugin goes in a folder of its own, plugins/{name}/",
                path.display()
            ));
        }
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return found;
    };
    let mut folders: Vec<_> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    folders.sort();
    for folder in folders {
        if folder.join(horizon_mods::manifest::MANIFEST).is_file() {
            found.push(folder);
            continue;
        }
        let mut inside = horizon_mods::find(&folder)
            .into_iter()
            .filter(|p| p.is_file());
        if let Some(first) = inside.next() {
            found.push(first);
        }
        for extra in inside {
            warn(format!(
                "{} is not loaded: one plugin per folder ({} holds another)",
                extra.display(),
                folder.display()
            ));
        }
    }
    found
}

/// A command name plugins may not take: vanilla's, and this server's own.
#[cfg_attr(not(feature = "plugins"), allow(dead_code))]
fn reserved_command(name: &str) -> bool {
    const OWN: [&str; 4] = ["dimension", "tps", "list", "plugins"];
    let d = horizon_command::vanilla::game_dispatcher();
    OWN.contains(&name)
        || d.nodes[0]
            .children
            .iter()
            .any(|&c| d.nodes[c].name() == name)
}

#[cfg(feature = "plugins")]
mod run {
    use super::{Event, Game, LanHost, Who, reserved_command};
    use crate::game::server_commands as sc;
    use horizon_client::chunk_map::ChunkMap;
    use horizon_client::{
        lan::data::component_nbt,
        text::{self, Style, Text},
    };
    use horizon_mods::{
        ArgumentKind, BlockPos, GameMode, Output, PlayerLook, ServerCommand, ServerPlayer,
        Snapshot, Status, WorldView,
    };
    use horizon_protocol::packets::play::{self, Clientbound as C};
    use horizon_world::registry::{BlockRegistry, BlockStateId};
    use serde_json::{Value, json};

    /// Blocks queued for levels that don't tick are kept up to this many; the oldest go first.
    const MAX_QUEUED_WRITES: usize = 1 << 16;

    /// Capability `world`'s view during a call: the levels' chunks, moved out of the game for
    /// the call and back after it (no copy).
    struct LentWorld {
        registry: &'static BlockRegistry,
        biomes: std::sync::Arc<horizon_client::biome::Biomes>,
        dims: Vec<(&'static str, ChunkMap)>,
    }
    impl WorldView for LentWorld {
        fn block(&self, dimension: &str, x: i32, y: i32, z: i32) -> Option<String> {
            let (_, chunks) = self.dims.iter().find(|(d, _)| *d == dimension)?;
            let chunk = chunks.get(&(x >> 4, z >> 4))?;
            Some(state_string(self.registry, chunk.block(x & 15, y, z & 15)))
        }
        fn biome(&self, dimension: &str, x: i32, y: i32, z: i32) -> Option<String> {
            let (_, chunks) = self.dims.iter().find(|(d, _)| *d == dimension)?;
            let chunk = chunks.get(&(x >> 4, z >> 4))?;
            Some(self.biomes.get(chunk.biome(x, y, z)).name.clone())
        }
        fn has_dimension(&self, dimension: &str) -> bool {
            matches!(
                dimension,
                "minecraft:overworld" | "minecraft:the_nether" | "minecraft:the_end"
            )
        }
        fn check_state(&self, state: &str) -> Result<(), String> {
            crate::game::render_control::parse_state(self.registry, state)
                .map(drop)
                .map_err(|e| format!("{state}: {e}"))
        }
    }

    /// Plugins' commands as a root of the commands report (`lan::commands`): a literal per
    /// command, then its arguments in a chain, each node runnable where only optional
    /// arguments follow.
    fn command_tree<'a>(commands: impl Iterator<Item = &'a ServerCommand>) -> Value {
        let mut root = serde_json::Map::new();
        for c in commands {
            let mut next: Option<(String, Value)> = None;
            for (i, a) in c.arguments.iter().enumerate().rev() {
                let executable = c.arguments.get(i + 1).is_none_or(|b| b.optional);
                let (parser, properties) = match a.kind {
                    ArgumentKind::Word => ("brigadier:string", json!({"type": "word"})),
                    ArgumentKind::Text => ("brigadier:string", json!({"type": "greedy"})),
                    ArgumentKind::Integer((min, max)) => {
                        let mut p = serde_json::Map::new();
                        if let Some(min) = min {
                            p.insert("min".into(), min.into());
                        }
                        if let Some(max) = max {
                            p.insert("max".into(), max.into());
                        }
                        ("brigadier:integer", Value::Object(p))
                    }
                    ArgumentKind::Number => ("brigadier:double", Value::Null),
                    ArgumentKind::Boolean => ("brigadier:bool", Value::Null),
                    ArgumentKind::Player => (
                        "minecraft:entity",
                        json!({"amount": "single", "type": "players"}),
                    ),
                    ArgumentKind::BlockPos => ("minecraft:block_pos", Value::Null),
                };
                let mut node = json!({
                    "type": "argument",
                    "parser": parser,
                    "properties": properties,
                    "executable": executable,
                });
                if let Some((name, child)) = next.take() {
                    node["children"] = json!({ name: child });
                }
                next = Some((a.name.clone(), node));
            }
            let executable = c.arguments.first().is_none_or(|a| a.optional);
            let mut literal = json!({"type": "literal", "executable": executable});
            if let Some((name, child)) = next {
                literal["children"] = json!({ name: child });
            }
            root.insert(c.name.clone(), literal);
        }
        for (name, node) in own_commands() {
            root.entry(name).or_insert(node);
        }
        json!({"type": "root", "children": root})
    }

    /// Horizon Server's own commands (`game/lan_actions.rs`): in the tree players get, so their
    /// chat box colours and completes them as vanilla's.
    fn own_commands() -> [(String, Value); 3] {
        let leaf = json!({"type": "literal", "executable": true});
        let on_off = json!({
            "type": "literal",
            "executable": false,
            "children": {"on": leaf.clone(), "off": leaf.clone()},
        });
        [
            ("tps".into(), leaf.clone()),
            (
                "plugins".into(),
                json!({"type": "literal", "executable": true, "children": {"reload": leaf}}),
            ),
            (
                "dimension".into(),
                json!({
                    "type": "literal",
                    "executable": true,
                    "children": {"nether": on_off.clone(), "end": on_off},
                }),
            ),
        ]
    }

    /// `minecraft:oak_log[axis=y]`.
    fn state_string(registry: &BlockRegistry, state: BlockStateId) -> String {
        let Some(s) = registry.state(state) else {
            return "minecraft:air".into();
        };
        if s.properties.is_empty() {
            return s.name.clone();
        }
        let props: Vec<String> = s
            .properties
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        format!("{}[{}]", s.name, props.join(","))
    }

    /// `UUID.toString()`.
    pub(super) fn uuid_string(u: u128) -> String {
        let h = format!("{u:032x}");
        format!(
            "{}-{}-{}-{}-{}",
            &h[..8],
            &h[8..12],
            &h[12..16],
            &h[16..20],
            &h[20..]
        )
    }

    fn mode(id: i32) -> GameMode {
        match id {
            1 => GameMode::Creative,
            2 => GameMode::Adventure,
            3 => GameMode::Spectator,
            _ => GameMode::Survival,
        }
    }

    /// The players on the server.
    pub(super) fn server_players(host: &LanHost) -> Vec<ServerPlayer> {
        host.hub
            .guests
            .iter()
            .filter(|g| g.placed)
            .map(|g| ServerPlayer {
                id: uuid_string(g.uuid),
                name: g.name.clone(),
                x: g.position[0],
                y: g.position[1],
                z: g.position[2],
                dimension: g.dimension.clone(),
                game_mode: mode(g.game_mode),
                health: g.health,
            })
            .collect()
    }

    /// Their looks and permission levels.
    fn player_looks(host: &LanHost) -> Vec<PlayerLook> {
        host.hub
            .guests
            .iter()
            .filter(|g| g.placed)
            .map(|g| PlayerLook {
                id: uuid_string(g.uuid),
                yaw: g.yaw,
                pitch: g.pitch,
                permission: g.permission,
            })
            .collect()
    }

    /// What plugins read of the server during a call.
    pub(super) fn snapshot(host: &LanHost) -> Snapshot {
        Snapshot {
            players: server_players(host),
            player_looks: player_looks(host),
            motions: super::io::motions(host),
            ..Snapshot::default()
        }
    }

    /// `/plugins reload`'s permission level.
    const RELOAD_LEVEL: u8 = 3;
    /// Ticks between stopping the plugins and starting them again: their sockets' threads let
    /// go of the ports within `plugin_io`'s 100 ms poll.
    const RELOAD_TICKS: u32 = 5;

    impl Game<'_> {
        /// Reads `plugins/` and starts every plugin there, logging each.
        fn start_plugins(&mut self) {
            self.plugins.started = true;
            let dir = self.plugins_dir();
            if let Err(e) = std::fs::create_dir_all(&dir) {
                super::super::dedicated::log(
                    "WARN",
                    &format!("could not make {}: {e}", dir.display()),
                );
            }
            let paths = super::plugin_paths(&dir);
            let mods = horizon_mods::Mods::load_plugins(&paths, reserved_command);
            for e in mods.entries() {
                let name = e.manifest.as_ref().map_or_else(
                    || e.path.display().to_string(),
                    |m| format!("{} {}", m.name, m.version),
                );
                let (level, line) = match &e.status {
                    Status::Running => ("INFO", format!("Plugin {name} loaded")),
                    Status::Failed(reason) => ("WARN", format!("Plugin {name} failed: {reason}")),
                    Status::Off | Status::Elsewhere => (
                        "INFO",
                        format!("Plugin {name} is not a server plugin: skipped"),
                    ),
                };
                super::super::dedicated::log(level, &line);
            }
            self.plugins.mods = mods;
            self.set_plugin_files();
            self.plugins.running = self.plugins.mods.runs_server();
            self.plugins.world = self.plugins.mods.wants_world();
        }

        /// `/plugins`: each plugin and how it runs. `/plugins reload` (operators): the plugins
        /// stop, each told the players online left, and [`RELOAD_TICKS`] later `plugins/` is
        /// read again, so new and changed plugins and their `data/` (messages, settings) take
        /// effect without a restart; the players online then join them afresh.
        pub(in crate::game) fn plugins_command(
            &mut self,
            host: &LanHost,
            conn: u32,
            args: &[&str],
        ) -> Text {
            match args {
                [] => self.plugin_list("Plugins"),
                ["reload"] => {
                    if host
                        .hub
                        .guest(conn)
                        .is_none_or(|g| g.permission < RELOAD_LEVEL)
                    {
                        return sc::not_operator();
                    }
                    if self.plugins.reload.is_some() {
                        return Text::markup("&e⟳ &7The plugins are reloading already.");
                    }
                    for p in std::mem::take(&mut self.plugins.players) {
                        self.plugins.mods.player_left(&p);
                    }
                    // Dropped, they free their ports and threads; what they said last is not shown.
                    self.plugins.mods = horizon_mods::Mods::default();
                    self.plugins.regions.clear();
                    self.plugins.running = false;
                    self.plugins.world = false;
                    self.plugins.events.clear();
                    self.plugins.reload = Some((RELOAD_TICKS, conn));
                    super::super::dedicated::log("INFO", "Plugins stopped for a reload");
                    Text::markup("&e⟳ &7Reloading the plugins…")
                }
                _ => Text::markup("&7Usage: &8/plugins [reload]"),
            }
        }

        /// Each plugin, `✔` running, `✖` failed (and why), `○` not a server plugin.
        fn plugin_list(&self, heading: &str) -> Text {
            let entries = self.plugins.mods.entries();
            let mut out = sc::title(heading);
            if entries.is_empty() {
                out += "\n &8» &7No plugins in &fplugins/&7.";
            }
            for e in entries {
                let (name, version) = e.manifest.as_ref().map_or_else(
                    || (e.path.display().to_string(), String::new()),
                    |m| (m.name.clone(), m.version.to_string()),
                );
                out += &match &e.status {
                    Status::Running => format!("\n &a✔ &f{name} &8{version}"),
                    Status::Failed(reason) => {
                        format!("\n &c✖ &f{name} &8{version} &7– &c{reason}")
                    }
                    Status::Off | Status::Elsewhere => {
                        format!("\n &7○ &7{name} &8{version} – not a server plugin")
                    }
                };
            }
            Text::markup(&out)
        }

        /// A reload's wait, then `plugins/` read again: who asked, once.
        fn finish_plugin_reload(&mut self) -> Option<u32> {
            let (left, by) = self.plugins.reload?;
            if left > 0 {
                self.plugins.reload = Some((left - 1, by));
                return None;
            }
            self.plugins.reload = None;
            self.plugins.reloads += 1;
            self.start_plugins();
            Some(by)
        }

        /// The tick's end: joins and leaves, the tick's events, the tick hook.
        pub(in crate::game) fn tick_plugins(&mut self) {
            if self.options.dedicated.is_none() {
                return;
            }
            if !self.plugins.started {
                self.start_plugins();
            }
            let reloaded = self.finish_plugin_reload();
            let Some(mut host) = self.lan.take() else {
                return;
            };
            if let Some(conn) = reloaded {
                let content = component_nbt(&self.plugin_list("Plugins reloaded").component());
                host.hub.send_to(
                    conn,
                    &C::SystemChat(play::SystemChat {
                        content,
                        overlay: false,
                    }),
                );
            }
            self.send_plugin_commands(&mut host);
            self.shield_guests(&mut host);
            if self.plugins.running {
                self.run_plugin_tick(&mut host);
            }
            self.lan = Some(host);
            self.run_plugin_console();
        }

        /// The command lines plugins queued (`command.run`, `players.teleport`), run as the
        /// server console; a quiet one's feedback isn't logged nor sent to the operators.
        pub(super) fn run_plugin_console(&mut self) {
            if self.plugins.console.is_empty() {
                return;
            }
            for (line, quiet) in std::mem::take(&mut self.plugins.console) {
                if !quiet {
                    super::super::dedicated::log("INFO", &format!("A plugin ran /{line}"));
                }
                self.chat_capture = Some(Vec::new());
                let fed = self.admin_feedback.len();
                self.as_console(|g| g.run_command(&line));
                if quiet {
                    self.admin_feedback.truncate(fed);
                }
                for said in self.chat_capture.take().unwrap_or_default() {
                    if !quiet {
                        super::super::dedicated::log("INFO", &said.plain());
                    }
                }
            }
            // Players moving to another level, and the operators' share of the feedback.
            if let Some(mut host) = self.lan.take() {
                self.run_level_teleports(&mut host);
                self.send_admin_feedback(&mut host, None);
                self.lan = Some(host);
            }
            self.admin_feedback.clear();
        }

        fn run_plugin_tick(&mut self, host: &mut LanHost) {
            let players = server_players(host);
            self.feed_plugin_statistics(host);
            self.plugins.mods.set_snapshot(snapshot(host));
            self.lend_world();
            let before = std::mem::take(&mut self.plugins.players);
            for p in before
                .iter()
                .filter(|p| !players.iter().any(|q| q.id == p.id))
            {
                self.plugins.mods.player_left(p);
            }
            for p in players
                .iter()
                .filter(|p| !before.iter().any(|q| q.id == p.id))
            {
                self.plugins.mods.player_joined(p);
            }
            for event in std::mem::take(&mut self.plugins.events) {
                let who = |w: &Who| {
                    let id = match w {
                        Who::Uuid(u) => uuid_string(*u),
                        Who::Conn(c) => host
                            .hub
                            .guest(*c)
                            .map_or_else(String::new, |g| uuid_string(g.uuid)),
                    };
                    players.iter().chain(&before).find(|p| p.id == id).cloned()
                };
                match event {
                    Event::Chat(w, text) => {
                        if let Some(p) = who(&w) {
                            self.plugins.mods.player_chat(&p, &text);
                        }
                    }
                    Event::Block {
                        who: w,
                        pos,
                        state,
                        placed,
                    } => {
                        let Some(p) = who(&w) else { continue };
                        let pos = BlockPos {
                            x: pos[0],
                            y: pos[1],
                            z: pos[2],
                        };
                        if placed {
                            self.plugins.mods.block_placed(&p, pos, &state);
                        } else {
                            self.plugins.mods.block_broken(&p, pos, &state);
                        }
                    }
                    Event::DoubleJump(w) => {
                        if let Some(p) = who(&w) {
                            self.plugins.mods.double_jump(&p);
                        }
                    }
                }
            }
            self.plugins.mods.server_tick(self.ticks);
            self.return_world();
            self.plugins.players = players;
            self.route_plugin_output(host);
        }

        /// Players' command trees list the plugins' commands after vanilla's, whenever they
        /// change.
        fn send_plugin_commands(&mut self, host: &mut LanHost) {
            let key = self
                .plugins
                .mods
                .generation()
                .wrapping_add(self.plugins.reloads << 32);
            if host.hub.mod_commands_key() == Some(key) {
                return;
            }
            let tree = command_tree(self.plugins.mods.server_commands());
            host.hub.set_mod_commands(key, tree);
        }

        /// `/line` typed by a connection's player: true when a plugin owns it, whatever the
        /// player's permission level (a plugin checks what it needs).
        pub(in crate::game) fn plugin_command(
            &mut self,
            host: &mut LanHost,
            conn: u32,
            line: &str,
        ) -> bool {
            if !self.plugins.running {
                return false;
            }
            let Some(g) = host.hub.guest(conn) else {
                return false;
            };
            let id = uuid_string(g.uuid);
            let Some(me) = server_players(host).into_iter().find(|p| p.id == id) else {
                return false;
            };
            self.plugins.mods.set_snapshot(snapshot(host));
            self.lend_world();
            let owned = self.plugins.mods.server_command(&me, line);
            self.return_world();
            if owned {
                self.route_plugin_output(host);
            }
            owned
        }

        /// Lends the levels' chunks to capability `world` for the calls up to
        /// [`Game::return_world`], when a running plugin has it.
        fn lend_world(&mut self) {
            if !self.plugins.world {
                return;
            }
            let mut dims = vec![(self.dim.id(), std::mem::take(&mut self.world.chunks))];
            for (dim, chunks) in self.levels.parked_chunks() {
                dims.push((dim.id(), std::mem::take(chunks)));
            }
            let registry = self.pack.assets.registry;
            let biomes = self.res.biomes().clone();
            self.plugins.mods.lend_world(Box::new(LentWorld {
                registry,
                biomes,
                dims,
            }));
        }

        /// Puts back the chunks [`Game::lend_world`] lent.
        fn return_world(&mut self) {
            let Some(view) = self.plugins.mods.take_world() else {
                return;
            };
            let view: Box<dyn std::any::Any> = view;
            let Ok(lent) = view.downcast::<LentWorld>() else {
                return;
            };
            let mut dims = lent.dims;
            if let Some(i) = dims.iter().position(|(d, _)| *d == self.dim.id()) {
                self.world.chunks = dims.swap_remove(i).1;
            }
            for (dim, chunks) in self.levels.parked_chunks() {
                if let Some(i) = dims.iter().position(|(d, _)| *d == dim.id()) {
                    *chunks = dims.swap_remove(i).1;
                }
            }
        }

        /// Makes the blocks plugins set in the level in place, in order, as `/setblock` does.
        /// Those of other dimensions wait for their level.
        pub(in crate::game) fn apply_plugin_block_writes(&mut self) {
            if self.plugins.block_writes.is_empty() {
                return;
            }
            let dim = self.dim.id();
            let (now, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.plugins.block_writes)
                .into_iter()
                .partition(|(d, _)| d == dim);
            self.plugins.block_writes = later;
            let registry = self.pack.assets.registry;
            for (_, b) in now {
                let state = match crate::game::render_control::parse_state(registry, &b.state) {
                    Ok(state) => state,
                    Err(e) => {
                        log::warn!("a plugin set {}: {e}", b.state);
                        continue;
                    }
                };
                if let Err(e) = self.place_state([b.x, b.y, b.z], state, false) {
                    log::debug!("a plugin's block at {} {} {}: {e:?}", b.x, b.y, b.z);
                }
            }
        }

        /// Sends a plugin's line to the player with this id, as a system message (in the action
        /// bar with `overlay`).
        fn tell_plugin_player(host: &mut LanHost, id: &str, line: &Text, overlay: bool) {
            let to = host
                .hub
                .guests
                .iter()
                .find(|g| g.placed && uuid_string(g.uuid) == id);
            if let Some(conn) = to.map(|g| g.conn) {
                host.hub.send_to(
                    conn,
                    &C::SystemChat(play::SystemChat {
                        content: component_nbt(&line.component()),
                        overlay,
                    }),
                );
            }
        }

        /// What plugins said and did: `Tell` and `Reply` to their player, `Broadcast` to
        /// everyone and the log, blocks queued for their level, a stopped plugin logged.
        /// Their lines are markup (`horizon_client_core::markup`), after the plugin's
        /// `prefix` or, for `Tell` and `Broadcast`, its name.
        pub(super) fn route_plugin_output(&mut self, host: &mut LanHost) {
            let line = |prefix: Option<String>, head: String, text: &str, error: bool| {
                let base = if error {
                    Style::color(text::RED)
                } else {
                    Style::default()
                };
                let mut line = Text::markup(&prefix.unwrap_or(head));
                line.spans.extend(Text::markup_on(text, &base).spans);
                line
            };
            for output in self.plugins.mods.drain() {
                match output {
                    Output::Tell {
                        name,
                        player,
                        text,
                        prefix,
                    } => {
                        let line = line(prefix, format!("&7[{name}] "), &text, false);
                        Self::tell_plugin_player(host, &player, &line, false);
                    }
                    Output::Reply {
                        player,
                        text,
                        error,
                        prefix,
                    } => {
                        let line = line(prefix, String::new(), &text, error);
                        Self::tell_plugin_player(host, &player, &line, false);
                    }
                    Output::Broadcast { name, text, prefix } => {
                        let line = line(prefix, format!("&7[{name}] "), &text, false);
                        super::super::dedicated::log("INFO", &line.plain());
                        for p in server_players(host) {
                            Self::tell_plugin_player(host, &p.id, &line, false);
                        }
                    }
                    Output::SetBlocks {
                        name,
                        dimension,
                        blocks,
                    } => {
                        let queued = &mut self.plugins.block_writes;
                        queued.extend(blocks.into_iter().map(|b| (dimension.clone(), b)));
                        if queued.len() > MAX_QUEUED_WRITES {
                            let over = queued.len() - MAX_QUEUED_WRITES;
                            queued.drain(..over);
                            log::warn!(
                                "plugin {name} set more blocks than wait for their level: {over} dropped"
                            );
                        }
                    }
                    Output::Protect { name, regions } => {
                        let all = &mut self.plugins.regions;
                        all.retain(|(owner, _)| *owner != name);
                        all.extend(regions.into_iter().map(|r| (name.clone(), r)));
                    }
                    Output::Stopped { name, reason } => {
                        super::super::dedicated::log(
                            "WARN",
                            &format!("Plugin {name} stopped: {reason}"),
                        );
                        self.plugins.running = self.plugins.mods.runs_server();
                        self.plugins.world = self.plugins.mods.wants_world();
                    }
                    Output::Chat { name, text } => {
                        super::super::dedicated::log("INFO", &format!("[{name}] {text}"));
                    }
                    Output::ActionBar { player, text, .. } => {
                        Self::tell_plugin_player(host, &player, &Text::markup(&text), true);
                    }
                    Output::Teleport {
                        name: _,
                        player,
                        dimension,
                        at: [x, y, z],
                        yaw,
                        pitch,
                    } => self.plugins.console.push((
                        format!("execute in {dimension} run tp {player} {x} {y} {z} {yaw} {pitch}"),
                        true,
                    )),
                    Output::Command { line } => self.plugins.console.push((line, false)),
                    Output::QuietCommands { lines, .. } => {
                        self.plugins
                            .console
                            .extend(lines.into_iter().map(|l| (l, true)));
                    }
                    o @ (Output::Glide { .. }
                    | Output::AllowDoubleJump { .. }
                    | Output::Push { .. }
                    | Output::Payload { .. }) => super::io::apply(host, o),
                    // Client mods' outputs: a plugin has none of these.
                    _ => {}
                }
            }
        }
    }
}
