//! Horizon Server plugin (`docs/plugins.md`): a server for the Simple Voice Chat mod. A player's
//! mod asks for a secret on channel `voicechat:request_secret`; with it, it talks to the
//! plugin's UDP port (24454, or the number in `storage` key `port`) over AES-128-GCM. The plugin
//! only relays the players' Opus frames: to the players near the speaker in their level, and to
//! the speaker's group wherever they are ([`protocol`]).
use horizon_mod_sdk::{
    server::{
        Datagram, GameMode, Player, Registration, ServerMod, channels, network, players, random,
    },
    storage,
};

mod aes;
mod protocol;

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

struct Voice {
    clients: Clients,
    port: u16,
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
            let text = format!(
                "Your voice chat mod doesn't match the server's version (server {}, yours {version}).",
                protocol::COMPATIBILITY_VERSION
            );
            players::tell(&player.id, &text);
            return;
        }
        let Ok(secret) = <[u8; 16]>::try_from(random::bytes(16)) else {
            horizon_mod_sdk::warn("no secure random source: no secret");
            return;
        };
        self.clients.add(uuid, secret);
        let id = &player.id;
        channels::send(id, "minecraft:register", &protocol::register_payload());
        let payload = protocol::secret_payload(&secret, self.port, uuid);
        channels::send(id, "voicechat:secret", &payload);
        let states: Vec<PlayerState> = self.voice_players().into_iter().map(|(_, s)| s).collect();
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
        };
        Ok((voice, Registration::new().on_tick().on_joins()))
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
            send_all(protocol::handle(
                &mut self.clients,
                &d.data,
                &d.address,
                &mut iv,
            ));
        }
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

horizon_mod_sdk::export_server_mod!(Voice);
