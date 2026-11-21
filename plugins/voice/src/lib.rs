//! Horizon Server plugin (`docs/plugins.md`): a server for the Simple Voice Chat mod. A player's
//! mod asks for a secret on channel `voicechat:request_secret`; with it, it talks to the
//! plugin's UDP port (24454, or the number in `storage` key `port`) over AES-128-GCM. The plugin
//! only relays the players' Opus frames: to the players near the speaker in their level, and to
//! the speaker's group wherever they are ([`protocol`]).
//!
//! A bot can take part too (Claude, from `/opt/claude-chat`): a program on the same machine
//! talks to the plugin's UDP port from 127.0.0.1 with the secret in `storage` key `bridge_token`
//! ([`bridge`]). It is told what the members of its group say, and its voice goes to them.
use horizon_mod_sdk::{
    messages::Messages,
    server::{
        BlockPos, Datagram, GameMode, Player, Registration, ServerMod, channels, command, network,
        players, random, world,
    },
    storage,
};

mod aes;
mod body;
mod bridge;
mod built;
mod protocol;
mod view;

use protocol::{Clients, PlayerState};

/// The players' UUID as the protocol has it.
fn uuid_of(id: &str) -> Option<u128> {
    u128::from_str_radix(&id.replace('-', ""), 16).ok()
}

/// A fresh IV from the secure source.
fn iv() -> Option<[u8; 12]> {
    random::bytes(12).try_into().ok()
}

fn send_all(packets: Vec<(String, Vec<u8>)>) {
    for (to, packet) in packets {
        network::udp_send(&to, &packet);
    }
}

/// The messages and their defaults, worded in `data/messages`.
const MESSAGES: &[(&str, &str)] = &[(
    "version_mismatch",
    "Your voice chat mod doesn't match the server's version (server {server}, yours {yours}).",
)];

struct Voice {
    clients: Clients,
    port: u16,
    messages: Messages,
    bot: bridge::Bot,
    /// What a player sees, while its rays are cast (a few hundred a tick).
    view: Option<view::View>,
    /// The bot's body, when it has one.
    body: Option<body::Body>,
    /// Whether bodies left from before a restart were taken away.
    swept: bool,
    /// The blocks the body placed: broken, they give nothing.
    built: built::Built,
    /// Spots broken last tick whose drops are taken away again (some come a tick late).
    sweeps: Vec<(String, [i32; 3])>,
    /// While taking away what the body built: blocks gone, and ones in unloaded chunks.
    clearing: Option<(u32, Vec<(u8, [i32; 3])>)>,
}

impl Voice {
    /// The players online with a secret: their id and state.
    fn voice_players(&self) -> Vec<(String, PlayerState)> {
        players::online()
            .into_iter()
            .filter_map(|p| {
                let uuid = uuid_of(&p.id)?;
                let (disabled, group) = self.clients.state(uuid)?;
                Some((
                    p.id,
                    PlayerState {
                        uuid,
                        name: p.name,
                        disabled,
                        disconnected: false,
                        group,
                    },
                ))
            })
            .collect()
    }

    /// One payload to everyone in the voice chat.
    fn broadcast(&self, channel: &str, data: &[u8]) {
        for (id, _) in self.voice_players() {
            channels::send(&id, channel, data);
        }
    }

    /// `PlayerStateManager.broadcastState`: one player's state to everyone in the voice chat.
    fn broadcast_state(&self, uuid: u128) {
        let players = self.voice_players();
        let Some((_, state)) = players.iter().find(|(_, s)| s.uuid == uuid) else {
            return;
        };
        let data = protocol::state_payload(state);
        for (id, _) in &players {
            channels::send(id, "voicechat:state", &data);
        }
    }

    /// `ServerGroupManager.cleanupGroups`: the empty groups go, for everyone.
    fn cleanup_groups(&mut self) {
        for id in self.clients.cleanup_groups() {
            self.broadcast(
                "voicechat:remove_group",
                &protocol::remove_group_payload(id),
            );
        }
    }

    /// `ServerVoiceEvents` and `initializePlayerConnection`: the secret, the states and the
    /// groups to a player whose mod asked.
    fn request_secret(&mut self, player: &Player, uuid: u128, data: &[u8]) {
        let Some(version) = protocol::parse_request_secret(data) else {
            return;
        };
        if version != protocol::COMPATIBILITY_VERSION {
            let server = protocol::COMPATIBILITY_VERSION;
            let args: [(&str, &dyn std::fmt::Display); 2] =
                [("server", &server), ("yours", &version)];
            let text = self.messages.get("version_mismatch", &args);
            players::tell(&player.id, &text);
            return;
        }
        let Ok(secret) = <[u8; 16]>::try_from(random::bytes(16)) else {
            horizon_mod_sdk::warn("no secure random source: no secret");
            return;
        };
        self.clients.add(uuid, secret);
        horizon_mod_sdk::info(&format!("{} connects to the voice chat", player.name));
        let id = &player.id;
        channels::send(id, "minecraft:register", &protocol::register_payload());
        let payload = protocol::secret_payload(&secret, self.port, uuid);
        channels::send(id, "voicechat:secret", &payload);
        let mut states: Vec<PlayerState> =
            self.voice_players().into_iter().map(|(_, s)| s).collect();
        states.extend(self.bot.state());
        channels::send(id, "voicechat:states", &protocol::states_payload(&states));
        // ServerGroupManager.onPlayerCompatibilityCheckSucceeded.
        for (group, g) in self.clients.groups() {
            channels::send(
                id,
                "voicechat:add_group",
                &protocol::add_group_payload(group, &g),
            );
        }
        self.broadcast_state(uuid);
    }

    fn joined(id: &str, group: Option<u128>, wrong_password: bool) {
        let data = protocol::joined_group_payload(group, wrong_password);
        channels::send(id, "voicechat:joined_group", &data);
    }
}

impl ServerMod for Voice {
    fn init() -> Result<(Self, Registration), String> {
        let port = storage::get("port")
            .and_then(|b| String::from_utf8_lossy(&b).trim().parse().ok())
            .unwrap_or(protocol::PORT);
        channels::listen("voicechat:")?;
        network::udp_bind(port)?;
        let voice = Voice {
            clients: Clients::default(),
            port,
            messages: Messages::load(MESSAGES),
            bot: bridge::Bot::new(storage::get("bridge_token")),
            view: None,
            body: None,
            swept: false,
            built: built::Built::load(storage::get("built")),
            sweeps: Vec::new(),
            clearing: None,
        };
        if !voice.bot.enabled() {
            horizon_mod_sdk::info("voice: no bridge_token, no bot");
        }
        Ok((voice, Registration::new().on_tick().on_joins().on_blocks()))
    }

    /// Where each player is, for the relay, and the keep-alives once a second.
    fn tick(&mut self, tick: u64) {
        self.clients.set_tick(tick);
        for p in players::online() {
            if let Some(uuid) = uuid_of(&p.id) {
                let listening = p.health > 0.0 && p.game_mode != GameMode::Spectator;
                self.clients
                    .place(uuid, &p.dimension, [p.x, p.y, p.z], listening);
            }
        }
        if tick.is_multiple_of(protocol::KEEP_ALIVE_TICKS) {
            send_all(protocol::keep_alives(&mut self.clients, &mut iv));
        }
        self.bot_tick(tick);
        self.view_tick();
        self.body_tick(tick);
    }

    /// A block the bot built is broken: whatever it dropped goes, now and next tick.
    fn block_broken(&mut self, player: &Player, pos: BlockPos, _state: &str) {
        let at = [pos.x, pos.y, pos.z];
        if let Some(d) = built::dimension(&player.dimension)
            && self.built.take(d, at)
        {
            command::run_quiet(&built::sweep(&player.dimension, at));
            self.sweeps.push((player.dimension.clone(), at));
        }
    }

    /// A player's own block where the bot's was (gone without a break): theirs now.
    fn block_placed(&mut self, player: &Player, pos: BlockPos, _state: &str) {
        if let Some(d) = built::dimension(&player.dimension) {
            self.built.take(d, [pos.x, pos.y, pos.z]);
        }
    }

    /// A player left: out of the voice chat, and gone from the others' lists.
    fn player_left(&mut self, player: &Player) {
        let Some(uuid) = uuid_of(&player.id).filter(|u| self.clients.has(*u)) else {
            return;
        };
        self.clients.remove(uuid);
        self.broadcast(
            "voicechat:remove_state",
            &protocol::remove_state_payload(uuid),
        );
        self.cleanup_groups();
    }

    fn udp(&mut self, datagrams: &[Datagram]) {
        for d in datagrams {
            if bridge::is_bridge(&d.address, &d.data) {
                self.bridge_datagram(&d.address, &d.data);
                continue;
            }
            send_all(protocol::handle(
                &mut self.clients,
                &d.data,
                &d.address,
                &mut iv,
            ));
        }
        self.pass_heard();
    }

    /// The mod's requests: the secret, groups (`ServerGroupManager`) and its state
    /// (`PlayerStateManager.onUpdateStatePacket`).
    fn payload(&mut self, player: &Player, channel: &str, data: &[u8]) {
        let Some(uuid) = uuid_of(&player.id) else {
            return;
        };
        let id = player.id.as_str();
        match channel {
            "voicechat:request_secret" => self.request_secret(player, uuid, data),
            "voicechat:create_group" => {
                let Some(group) = protocol::parse_create_group(data) else {
                    return;
                };
                let Some(group_id) = protocol::random_uuid(&random::bytes(16)) else {
                    return;
                };
                if !self.clients.create_group(uuid, group_id, group.clone()) {
                    return;
                }
                self.broadcast(
                    "voicechat:add_group",
                    &protocol::add_group_payload(group_id, &group),
                );
                Self::joined(id, Some(group_id), false);
                self.broadcast_state(uuid);
                self.cleanup_groups();
            }
            "voicechat:set_group" => {
                let Some((group, password)) = protocol::parse_join_group(data) else {
                    return;
                };
                match self.clients.join_group(uuid, group, password.as_deref()) {
                    Ok(()) => {
                        Self::joined(id, Some(group), false);
                        self.broadcast_state(uuid);
                        self.cleanup_groups();
                    }
                    Err(true) => Self::joined(id, None, true),
                    Err(false) => {}
                }
            }
            "voicechat:leave_group" => {
                self.clients.leave_group(uuid);
                Self::joined(id, None, false);
                self.broadcast_state(uuid);
                self.cleanup_groups();
            }
            "voicechat:update_state" => {
                let Some(disabled) = protocol::parse_update_state(data) else {
                    return;
                };
                self.clients.set_disabled(uuid, disabled);
                self.broadcast_state(uuid);
            }
            _ => {}
        }
    }
}

impl Voice {
    /// The bot's state to everyone in the voice chat: in its group, or gone.
    fn broadcast_bot(&self) {
        let (channel, data) = match self.bot.state() {
            Some(state) => ("voicechat:state", protocol::state_payload(&state)),
            None => (
                "voicechat:remove_state",
                protocol::remove_state_payload(bridge::BOT_UUID),
            ),
        };
        self.broadcast(channel, &data);
    }

    /// The bot leaves its group (asked to, its group is gone, or its program went quiet).
    fn bot_leave(&mut self) {
        if self.bot.group.take().is_some() {
            self.clients.listen_to(None);
            self.broadcast_bot();
            self.cleanup_groups();
        }
        self.bot.tell_status();
    }

    /// The bot joins the group the player is in; false when the player is in none.
    fn bot_join(&mut self, player: u128) -> bool {
        let Some((_, Some(group))) = self.clients.state(player) else {
            return false;
        };
        self.bot.group = Some(group);
        self.clients.listen_to(Some(group));
        self.broadcast_bot();
        true
    }

    fn bridge_datagram(&mut self, from: &str, data: &[u8]) {
        let Some(message) = self.bot.read(from, data) else {
            return;
        };
        match message {
            bridge::Message::Hello => self.bot.tell_status(),
            bridge::Message::Join(player) => {
                if !self.bot_join(player) {
                    self.bot.group = None;
                    self.clients.listen_to(None);
                }
                self.bot.tell_status();
            }
            bridge::Message::Leave => self.bot_leave(),
            bridge::Message::View(name) => self.start_view(&name),
            bridge::Message::Body(order) => self.body_order(order),
            bridge::Message::ActionBar(name, text) => {
                if let Some(p) = players::online().into_iter().find(|p| p.name == name) {
                    players::action_bar(&p.id, &text);
                }
            }
            bridge::Message::Audio(sequence, opus) => {
                if let Some(group) = self.bot.group {
                    send_all(protocol::speak(
                        &self.clients,
                        bridge::BOT_UUID,
                        group,
                        &opus,
                        sequence,
                        &mut iv,
                    ));
                }
            }
        }
    }

    /// What the bot's group said, on to its program.
    fn pass_heard(&mut self) {
        let heard = self.clients.heard();
        if self.bot.group.is_some() {
            for (speaker, opus) in heard {
                self.bot.tell_heard(speaker, &opus);
            }
        }
    }

    /// A view from the player's eyes, cast over the next ticks; one at a time.
    fn start_view(&mut self, name: &str) {
        let player = players::online().into_iter().find(|p| p.name == name);
        let Some((p, (yaw, pitch))) = player.and_then(|p| {
            let rotation = players::rotation(&p.id)?;
            Some((p, rotation))
        }) else {
            self.bot.tell_no_view();
            return;
        };
        if self.view.is_none() {
            self.view = Some(view::View::new(&p.dimension, [p.x, p.y, p.z], yaw, pitch));
        }
    }

    fn view_tick(&mut self) {
        let Some(v) = self.view.as_mut() else {
            return;
        };
        let dimension = v.dimension().to_owned();
        v.cast(view::RAYS_PER_TICK, &mut |[x, y, z]| {
            world::get_block(&dimension, x, y, z)
        });
        if v.done()
            && let Some(v) = self.view.take()
        {
            self.bot.tell_view(&v);
        }
    }

    /// The players as the body sees them.
    fn seen() -> Vec<body::Seen> {
        players::online()
            .into_iter()
            .map(|p| body::Seen {
                yaw: players::rotation(&p.id).map_or(0.0, |r| r.0),
                name: p.name,
                dimension: p.dimension,
                at: [p.x, p.y, p.z],
            })
            .collect()
    }

    fn body_order(&mut self, order: body::Order) {
        match order {
            body::Order::Come(name) => {
                let seen = Self::seen();
                let Some(p) = seen.iter().find(|p| p.name == name) else {
                    return;
                };
                let near = self.body.as_ref().is_some_and(|b| {
                    b.dimension == p.dimension
                        && ((b.at[0] - p.at[0]).powi(2) + (b.at[2] - p.at[2]).powi(2)).sqrt() < 16.0
                });
                if near {
                    if let Some(b) = self.body.as_mut() {
                        b.order(body::Order::Follow(name));
                    }
                } else {
                    let dimension = p.dimension.clone();
                    let (b, made) =
                        body::Body::new(p, &mut |[x, y, z]| world::get_block(&dimension, x, y, z));
                    command::run_quiet(&made);
                    self.body = Some(b);
                }
            }
            body::Order::Clear => {
                if self.clearing.is_none() {
                    self.clearing = Some((0, Vec::new()));
                }
            }
            body::Order::Remove => {
                command::run_quiet(&[body::Body::remove_all()]);
                self.body = None;
            }
            order => {
                if let Some(b) = self.body.as_mut() {
                    b.order(order);
                }
            }
        }
        self.bot.tell_body(self.body.as_ref());
    }

    fn body_tick(&mut self, tick: u64) {
        if !self.swept && self.bot.enabled() && tick > 40 {
            self.swept = true;
            command::run_quiet(&[body::Body::remove_all()]);
        }
        if self.body.is_some() && self.bot.silent() {
            command::run_quiet(&[body::Body::remove_all()]);
            self.body = None;
        }
        self.clear_tick();
        for (dimension, at) in std::mem::take(&mut self.sweeps) {
            command::run_quiet(&built::sweep(&dimension, at));
        }
        if tick.is_multiple_of(100) {
            self.built.save();
        }
        let Some(b) = self.body.as_mut() else {
            return;
        };
        let dimension = b.dimension.clone();
        let out = b.tick(tick, &Self::seen(), &mut |[x, y, z]| {
            world::get_block(&dimension, x, y, z)
        });
        if !out.commands.is_empty() {
            command::run_quiet(&out.commands);
        }
        let known = built::dimension(&b.dimension);
        if let Some(d) = known.filter(|_| !out.blocks.is_empty()) {
            for (at, _) in &out.blocks {
                self.built.add(d, *at);
            }
            let changes: Vec<world::BlockChange> = out
                .blocks
                .into_iter()
                .map(|([x, y, z], state)| world::BlockChange { x, y, z, state })
                .collect();
            if let Err(err) = world::set_blocks(&b.dimension, &changes) {
                horizon_mod_sdk::info(&format!("voice: the body couldn't build: {err}"));
            }
        }
        if tick.is_multiple_of(20) {
            self.bot.tell_body(self.body.as_ref());
        }
    }

    /// Takes away up to `CLEAR_PER_TICK` of the body's blocks a tick; ones in chunks nobody
    /// has loaded are kept for later.
    fn clear_tick(&mut self) {
        const CLEAR_PER_TICK: usize = 1000;
        let Some((removed, waiting)) = self.clearing.as_mut() else {
            return;
        };
        let spots = self.built.drain(CLEAR_PER_TICK);
        let done = spots.is_empty();
        for (d, dimension) in built::DIMENSIONS.iter().enumerate() {
            let changes: Vec<world::BlockChange> = spots
                .iter()
                .filter(|(sd, _)| usize::from(*sd) == d)
                .filter_map(
                    |(sd, [x, y, z])| match world::get_block(dimension, *x, *y, *z) {
                        None => {
                            waiting.push((*sd, [*x, *y, *z]));
                            None
                        }
                        Some(s) if body::empty(&s) => None,
                        Some(_) => Some(world::BlockChange {
                            x: *x,
                            y: *y,
                            z: *z,
                            state: "minecraft:air".to_owned(),
                        }),
                    },
                )
                .collect();
            if changes.is_empty() {
                continue;
            }
            *removed += changes.len() as u32;
            if let Err(err) = world::set_blocks(dimension, &changes) {
                horizon_mod_sdk::info(&format!(
                    "voice: couldn't take the bot's blocks away: {err}"
                ));
            }
        }
        if done && let Some((removed, waiting)) = self.clearing.take() {
            for (d, at) in &waiting {
                self.built.add(*d, *at);
            }
            self.built.save();
            self.bot.tell_cleared(removed, waiting.len() as u32);
        }
    }

    /// The bot leaves when its group is gone or its program stopped greeting.
    fn bot_tick(&mut self, tick: u64) {
        self.bot.set_tick(tick);
        let gone = self.bot.group.is_some_and(|g| !self.clients.has_group(g));
        if self.bot.group.is_some() && (gone || self.bot.silent()) {
            self.bot_leave();
        }
    }
}

horizon_mod_sdk::export_server_mod!(Voice);
