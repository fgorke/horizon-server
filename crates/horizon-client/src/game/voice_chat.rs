//! Simple Voice Chat for the server's players (`horizon_client::voice`): the
//! mod's channel handshake, the player states it shows, and each player's
//! level and position for the voice server. The UDP server starts with the
//! first player who asks, on port 24454 (`HORIZON_VOICE_PORT` to change it).

use super::{Game, lan::LanHost};
use horizon_client::voice::{self, PlayerState, VoiceServer};
use horizon_protocol::packets::{
    configuration::CustomPayload,
    play::{self, Clientbound as C},
};
use std::sync::OnceLock;

static SERVER: OnceLock<Option<VoiceServer>> = OnceLock::new();

fn server() -> Option<&'static VoiceServer> {
    SERVER
        .get_or_init(|| {
            let port = std::env::var("HORIZON_VOICE_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(voice::PORT);
            match VoiceServer::start(port) {
                Ok(s) => Some(s),
                Err(e) => {
                    log::warn!("voice chat: port {port}: {e}");
                    None
                }
            }
        })
        .as_ref()
}

fn payload(channel: &str, data: Vec<u8>) -> C {
    C::CustomPayload(CustomPayload {
        channel: channel.into(),
        data,
    })
}

fn chat(host: &LanHost, conn: u32, text: &str) {
    let text = horizon_client::text::Text::legacy(text);
    host.hub.send_to(
        conn,
        &C::SystemChat(play::SystemChat {
            content: horizon_client::lan::data::component_nbt(&text.component()),
            overlay: false,
        }),
    );
}

/// The players in the voice chat that are on the server: their connection
/// and state.
fn voice_players(host: &LanHost, server: &VoiceServer) -> Vec<(u32, PlayerState)> {
    server
        .players()
        .into_iter()
        .filter_map(|(uuid, disabled, group)| {
            let g = host.hub.guests.iter().find(|g| g.uuid == uuid)?;
            Some((
                g.conn,
                PlayerState {
                    uuid,
                    name: g.name.clone(),
                    disabled,
                    disconnected: false,
                    group,
                },
            ))
        })
        .collect()
}

/// `PlayerStateManager.broadcastState`: one player's state to everyone in the
/// voice chat.
fn broadcast_state(host: &LanHost, server: &VoiceServer, uuid: u128) {
    let players = voice_players(host, server);
    let Some((_, state)) = players.iter().find(|(_, s)| s.uuid == uuid) else {
        return;
    };
    let packet = payload("voicechat:state", voice::state_payload(state));
    for (conn, _) in &players {
        host.hub.send_to(*conn, &packet);
    }
}

/// One payload to everyone in the voice chat.
fn broadcast(host: &LanHost, server: &VoiceServer, channel: &str, data: Vec<u8>) {
    let packet = payload(channel, data);
    for (conn, _) in voice_players(host, server) {
        host.hub.send_to(conn, &packet);
    }
}

/// `ServerGroupManager.cleanupGroups`: the empty groups go, for everyone.
fn cleanup_groups(host: &LanHost, server: &VoiceServer) {
    for id in server.cleanup_groups() {
        broadcast(
            host,
            server,
            "voicechat:remove_group",
            voice::remove_group_payload(id),
        );
    }
}

/// A player left the server: out of the voice chat, and gone from the
/// others' lists.
pub(super) fn guest_left(host: &LanHost, uuid: u128) {
    let Some(server) = SERVER.get().and_then(Option::as_ref) else {
        return;
    };
    if !server.has(uuid) {
        return;
    }
    server.remove(uuid);
    broadcast(
        host,
        server,
        "voicechat:remove_state",
        voice::remove_state_payload(uuid),
    );
    cleanup_groups(host, server);
}

impl Game<'_> {
    /// A `voicechat:` payload from a guest: the secret request
    /// (`ServerVoiceEvents`, then `initializePlayerConnection`) or a state
    /// change (`PlayerStateManager.onUpdateStatePacket`).
    pub(super) fn voice_payload(
        &mut self,
        host: &mut LanHost,
        conn: u32,
        channel: &str,
        data: &[u8],
    ) {
        let Some(uuid) = host.hub.guest(conn).map(|g| g.uuid) else {
            return;
        };
        match channel {
            "voicechat:request_secret" => {
                let Some(version) = voice::parse_request_secret(data) else {
                    return;
                };
                if version != voice::COMPATIBILITY_VERSION {
                    let text = format!(
                        "§cVoice chat: your mod version doesn't match the server's (server {}, yours {}).",
                        voice::COMPATIBILITY_VERSION,
                        version
                    );
                    chat(host, conn, &text);
                    return;
                }
                let Some(server) = server() else {
                    chat(
                        host,
                        conn,
                        "§cVoice chat isn't available on this server right now.",
                    );
                    return;
                };
                let secret = match server.add(uuid) {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("voice chat: secret: {e}");
                        return;
                    }
                };
                host.hub.send_to(
                    conn,
                    &payload("minecraft:register", voice::register_payload()),
                );
                host.hub.send_to(
                    conn,
                    &payload(
                        "voicechat:secret",
                        voice::secret_payload(&secret, server.port(), uuid),
                    ),
                );
                let states: Vec<PlayerState> = voice_players(host, server)
                    .into_iter()
                    .map(|(_, s)| s)
                    .collect();
                host.hub.send_to(
                    conn,
                    &payload("voicechat:states", voice::states_payload(&states)),
                );
                // ServerGroupManager.onPlayerCompatibilityCheckSucceeded.
                for (id, group) in server.groups() {
                    host.hub.send_to(
                        conn,
                        &payload("voicechat:add_group", voice::add_group_payload(id, &group)),
                    );
                }
                broadcast_state(host, server, uuid);
            }
            "voicechat:create_group" => {
                let (Some(group), Some(server)) = (
                    voice::parse_create_group(data),
                    SERVER.get().and_then(Option::as_ref),
                ) else {
                    return;
                };
                let Some(id) = server.create_group(uuid, group.clone()) else {
                    return;
                };
                broadcast(
                    host,
                    server,
                    "voicechat:add_group",
                    voice::add_group_payload(id, &group),
                );
                host.hub.send_to(
                    conn,
                    &payload(
                        "voicechat:joined_group",
                        voice::joined_group_payload(Some(id), false),
                    ),
                );
                broadcast_state(host, server, uuid);
                cleanup_groups(host, server);
            }
            "voicechat:set_group" => {
                let (Some((id, password)), Some(server)) = (
                    voice::parse_join_group(data),
                    SERVER.get().and_then(Option::as_ref),
                ) else {
                    return;
                };
                match server.join_group(uuid, id, password.as_deref()) {
                    Ok(()) => {
                        host.hub.send_to(
                            conn,
                            &payload(
                                "voicechat:joined_group",
                                voice::joined_group_payload(Some(id), false),
                            ),
                        );
                        broadcast_state(host, server, uuid);
                        cleanup_groups(host, server);
                    }
                    Err(true) => {
                        host.hub.send_to(
                            conn,
                            &payload(
                                "voicechat:joined_group",
                                voice::joined_group_payload(None, true),
                            ),
                        );
                    }
                    Err(false) => {}
                }
            }
            "voicechat:leave_group" => {
                let Some(server) = SERVER.get().and_then(Option::as_ref) else {
                    return;
                };
                server.leave_group(uuid);
                host.hub.send_to(
                    conn,
                    &payload(
                        "voicechat:joined_group",
                        voice::joined_group_payload(None, false),
                    ),
                );
                broadcast_state(host, server, uuid);
                cleanup_groups(host, server);
            }
            "voicechat:update_state" => {
                let (Some(disabled), Some(server)) = (
                    voice::parse_update_state(data),
                    SERVER.get().and_then(Option::as_ref),
                ) else {
                    return;
                };
                server.set_disabled(uuid, disabled);
                broadcast_state(host, server, uuid);
            }
            _ => {}
        }
    }

    /// Once a tick for a guest: their level and position for the voice
    /// server, and whether they hear the others (alive, not a spectator).
    pub(super) fn voice_tick(&self, host: &LanHost, conn: u32) {
        let (Some(server), Some(g)) = (SERVER.get().and_then(Option::as_ref), host.hub.guest(conn))
        else {
            return;
        };
        server.place(g.uuid, &g.dimension, g.position, !g.dead && !g.spectator());
    }
}
