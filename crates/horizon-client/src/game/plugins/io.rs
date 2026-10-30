//! The server's side of plugins' capabilities `movement`, `channels`, `network` and `statistics`:
//! the swap-hands key and payloads reach them at once, their sockets between ticks
//! ([`Game::pump_plugins`]), and the statistics of the players online once a second. What they
//! ask of players (glides, double jumps, pushes, payloads) is made in [`apply`].
use super::run::{server_players, snapshot, uuid_string};
use crate::game::{Game, lan::LanHost};
use horizon_core::geometry::Vec3;
use horizon_mods::{Output, PlayerMotion, ServerFiles, ServerPlayer};
use horizon_protocol::packets::{
    configuration::CustomPayload,
    play::{self, Clientbound as C},
};
use std::collections::HashMap;

/// How often, in ticks, the statistics of the players online reach the plugins.
const STATS_EVERY: u64 = 20;

/// The motion of the players on the server.
pub(super) fn motions(host: &LanHost) -> Vec<PlayerMotion> {
    host.hub
        .guests
        .iter()
        .filter(|g| g.placed)
        .map(|g| PlayerMotion {
            id: uuid_string(g.uuid),
            on_ground: g.on_ground,
            gliding: g.fall_flying,
            fall_distance: g.motion.as_ref().map_or(0.0, |m| m.fall_distance),
            riding: g.vehicle.is_some(),
        })
        .collect()
}

/// The player behind a connection, as plugins see them.
fn plugin_player(host: &LanHost, conn: u32) -> Option<ServerPlayer> {
    let id = uuid_string(host.hub.guest(conn)?.uuid);
    server_players(host).into_iter().find(|p| p.id == id)
}

/// What a plugin asked of a player: a glide without an elytra, double jumps allowed or not, a
/// push, a payload. A player no longer on is skipped.
pub(super) fn apply(host: &mut LanHost, output: Output) {
    let player = match &output {
        Output::Glide { player }
        | Output::AllowDoubleJump { player, .. }
        | Output::Push { player, .. }
        | Output::Payload { player, .. } => player.clone(),
        _ => return,
    };
    let Some(g) = host
        .hub
        .guests
        .iter_mut()
        .find(|g| g.placed && uuid_string(g.uuid) == player)
    else {
        return;
    };
    match output {
        Output::Glide { .. } => {
            if !g.on_ground && g.vehicle.is_none() {
                g.plugin_glide = true;
                g.fall_flying = true;
            }
        }
        Output::AllowDoubleJump { allow, .. } => {
            if g.plugin_may_fly != allow {
                g.plugin_may_fly = allow;
                g.send(&C::PlayerAbilities(play::PlayerAbilities {
                    flags: g.ability_flags(),
                    flying_speed: 0.05,
                    walking_speed: 0.1,
                }));
            }
        }
        Output::Push { velocity, .. } => {
            if let Some(m) = g.motion.as_mut() {
                m.velocity = Vec3::new(velocity[0], velocity[1], velocity[2]);
            }
            g.send(&C::SetEntityMotion(play::SetEntityMotion {
                id: g.entity_id,
                velocity,
            }));
        }
        Output::Payload { channel, data, .. } => {
            g.send(&C::CustomPayload(CustomPayload { channel, data }));
        }
        _ => {}
    }
}

impl Game<'_> {
    /// The swap-hands key of a connection's player: true when a plugin took it.
    pub(in crate::game) fn plugin_swap_hands(&mut self, host: &mut LanHost, conn: u32) -> bool {
        if !self.plugins.running {
            return false;
        }
        let Some(me) = plugin_player(host, conn) else {
            return false;
        };
        self.plugins.mods.set_snapshot(snapshot(host));
        let taken = self.plugins.mods.swap_hands(&me);
        self.route_plugin_output(host);
        taken
    }

    /// A payload from a connection's player: true when a plugin listens to its channel.
    pub(in crate::game) fn plugin_payload(
        &mut self,
        host: &mut LanHost,
        conn: u32,
        channel: &str,
        data: &[u8],
    ) -> bool {
        if !self.plugins.running || !self.plugins.mods.listens(channel) {
            return false;
        }
        let Some(me) = plugin_player(host, conn) else {
            return false;
        };
        self.plugins.mods.set_snapshot(snapshot(host));
        let heard = self.plugins.mods.payload(&me, channel, data);
        self.route_plugin_output(host);
        heard
    }

    /// Between ticks: what reached the plugins' sockets. False when no plugin has one (the
    /// server then sleeps to the next tick).
    pub(in crate::game) fn pump_plugins(&mut self) -> bool {
        if !self.plugins.running || !self.plugins.mods.wants_network() {
            return false;
        }
        self.plugins.mods.pump_network();
        if let Some(mut host) = self.lan.take() {
            self.route_plugin_output(&mut host);
            self.lan = Some(host);
        }
        self.run_plugin_console();
        true
    }

    /// Where capability `statistics` reads: the world, `usercache.json` beside it and the
    /// game's resources.
    pub(super) fn set_plugin_files(&mut self) {
        let usercache = self
            .world_root
            .parent()
            .unwrap_or(std::path::Path::new(""))
            .join("usercache.json");
        self.plugins.mods.set_server_files(ServerFiles {
            world: self.world_root.clone(),
            usercache,
            resources: self.options.resources.clone(),
            live: HashMap::new(),
        });
    }

    /// Once a second, while a plugin reads statistics: those of the players online.
    pub(super) fn feed_plugin_statistics(&mut self, host: &LanHost) {
        if !self.ticks.is_multiple_of(STATS_EVERY) || !self.plugins.mods.wants_statistics() {
            return;
        }
        let live = host
            .hub
            .guests
            .iter()
            .filter(|g| g.placed)
            .filter_map(|g| {
                let play = host.play.get(&g.conn)?;
                let json = serde_json::to_string(&play.stats.values).ok()?;
                Some((uuid_string(g.uuid), json))
            })
            .collect();
        self.plugins.mods.set_live_statistics(live);
    }
}
