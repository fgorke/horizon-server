//! /spawn, /setspawn and spawn elytra. /spawn waits [`COUNTDOWN`] ticks with
//! the player standing still (looking around is fine), then takes them to the
//! spot an operator set with /setspawn, or to the world spawn. In the region
//! an operator set with /spawnelytra pos1|pos2 a survival player who falls off
//! (or double-jumps) glides without an elytra, boosts once with the
//! swap-hands key and lands without fall damage; the glide goes on past the
//! region until the player lands.

use super::{Dim, Game, lan::LanHost, server_travel::LevelTeleport, spawn_config};
use horizon_core::geometry::Vec3;
use horizon_protocol::packets::play::{self, Clientbound as C};

/// /spawn's wait, in ticks.
const COUNTDOWN: u32 = 100;
/// How far the player may drift during the wait before it is cancelled.
const STILL: f64 = 0.1;
/// The fall, in blocks, after which a player in the region starts gliding:
/// more than a jump's.
const FALL: f64 = 1.5;
/// The boost's speed along the look, in blocks per tick.
const BOOST: f64 = 2.0;

/// The unit look vector of a yaw and pitch in degrees (Entity.calculateViewVector).
fn look(yaw: f32, pitch: f32) -> [f64; 3] {
    let (yaw, pitch) = (f64::from(yaw).to_radians(), f64::from(pitch).to_radians());
    [
        -yaw.sin() * pitch.cos(),
        -pitch.sin(),
        yaw.cos() * pitch.cos(),
    ]
}

fn chat(host: &LanHost, conn: u32, text: &str, overlay: bool) {
    let text = horizon_client::text::Text::legacy(text);
    host.hub.send_to(
        conn,
        &C::SystemChat(play::SystemChat {
            content: horizon_client::lan::data::component_nbt(&text.component()),
            overlay,
        }),
    );
}

impl Game<'_> {
    /// /spawn, /setspawn and /spawnelytra [pos1|pos2|off] from a guest.
    pub(super) fn spawn_command(&mut self, host: &mut LanHost, conn: u32, words: &[&str]) {
        let Some(g) = host.hub.guest_mut(conn) else {
            return;
        };
        if words == ["spawn"] {
            g.spawn_countdown = Some((COUNTDOWN, g.position));
            return;
        }
        let (op, at, yaw, pitch) = (g.permission >= 2, g.position, g.yaw, g.pitch);
        let dimension = g.dimension.clone();
        if !op {
            chat(host, conn, "§cYou need to be an operator for that.", false);
            return;
        }
        let block = at.map(|c| c.floor() as i32);
        let root = self.world_root.clone();
        let (saved, reply) = match words {
            ["setspawn"] => (
                spawn_config::change(&root, |c| {
                    c.spawn = Some(spawn_config::SpawnPoint {
                        dimension,
                        at,
                        yaw,
                        pitch,
                    });
                }),
                format!("§aSpawn set: {:.1} {:.1} {:.1}", at[0], at[1], at[2]),
            ),
            ["spawnelytra", corner @ ("pos1" | "pos2")] => (
                spawn_config::change(&root, |c| {
                    if c.region_dimension.as_deref() != Some(dimension.as_str()) {
                        // Corners of another level don't make a region with this one.
                        c.pos1 = None;
                        c.pos2 = None;
                    }
                    c.region_dimension = Some(dimension);
                    if *corner == "pos1" {
                        c.pos1 = Some(block);
                    } else {
                        c.pos2 = Some(block);
                    }
                }),
                format!("§a{corner} set: {} {} {}", block[0], block[1], block[2]),
            ),
            ["spawnelytra", "off"] => (
                spawn_config::change(&root, |c| {
                    c.region_dimension = None;
                    c.pos1 = None;
                    c.pos2 = None;
                }),
                "§eSpawn elytra region removed.".to_owned(),
            ),
            ["spawnelytra"] => {
                let corners = spawn_config::read(&root, |c| (c.pos1, c.pos2));
                let show = |p: Option<[i32; 3]>| {
                    p.map_or("not set".to_owned(), |p| {
                        format!("{} {} {}", p[0], p[1], p[2])
                    })
                };
                let reply = format!(
                    "§ePos1: {} §7| §ePos2: {} §7(/spawnelytra pos1|pos2|off)",
                    show(corners.0),
                    show(corners.1)
                );
                (true, reply)
            }
            _ => (true, "§eUsage: /spawnelytra <pos1|pos2|off>".to_owned()),
        };
        let reply = if saved {
            reply
        } else {
            "§cCould not write horizon-spawn.json.".to_owned()
        };
        chat(host, conn, &reply, false);
    }

    /// Once a tick for a guest, before its glide tick: /spawn's countdown,
    /// then spawn elytra (a glide the fall or double jump in the region
    /// starts, no fall damage while it lasts or on its landing, and the
    /// fly-jump allowed only in the region and not while gliding).
    pub(super) fn spawn_elytra_tick(&mut self, host: &mut LanHost, conn: u32) {
        self.spawn_countdown_tick(host, conn);
        let root = self.world_root.clone();
        let Some(g) = host.hub.guest_mut(conn) else {
            return;
        };
        let survival = !g.creative() && !g.spectator();
        let near = survival && spawn_config::read(&root, |c| c.in_region(&g.dimension, g.position));
        let jumped = std::mem::take(&mut g.spawn_jump);
        let falling = g.motion.as_ref().is_some_and(|m| m.fall_distance > FALL);
        let started = (jumped || falling)
            && near
            && !g.on_ground
            && g.vehicle.is_none()
            && !g.fall_flying
            && !g.spawn_gliding;
        if started {
            g.spawn_gliding = true;
            g.spawn_boosted = false;
            g.fall_flying = true;
        }
        if g.spawn_gliding {
            if let Some(m) = g.motion.as_mut() {
                m.fall_distance = 0.0;
            }
            if g.on_ground || !g.fall_flying || !survival {
                g.spawn_gliding = false;
            }
        }
        let may_fly = near && !g.spawn_gliding;
        // A double jump also made the client fly: the abilities stop that.
        if jumped || may_fly != g.spawn_may_fly {
            g.spawn_may_fly = may_fly;
            g.send(&C::PlayerAbilities(play::PlayerAbilities {
                flags: g.ability_flags(),
                flying_speed: 0.05,
                walking_speed: 0.1,
            }));
        }
        if started {
            chat(host, conn, "§bPress F for a boost", true);
        }
    }

    /// /spawn's wait: the seconds left in the action bar, cancelled by a
    /// move, and at its end the teleport.
    fn spawn_countdown_tick(&mut self, host: &mut LanHost, conn: u32) {
        let Some(g) = host.hub.guest_mut(conn) else {
            return;
        };
        let Some((left, from)) = g.spawn_countdown else {
            return;
        };
        if (0..3).any(|k| (g.position[k] - from[k]).abs() > STILL) {
            g.spawn_countdown = None;
            chat(host, conn, "§cTeleport cancelled: you moved.", true);
            return;
        }
        if left > 0 {
            g.spawn_countdown = Some((left - 1, from));
            if left % 20 == 0 {
                let text = format!("§eTeleport in {} s, don't move", left / 20);
                chat(host, conn, &text, true);
            }
            return;
        }
        g.spawn_countdown = None;
        match spawn_config::read(&self.world_root, |c| c.spawn.clone()) {
            Some(p) => self.teleport_to_spawn(host, conn, &p),
            None => self.respawn_at_world_spawn(host, conn),
        }
        chat(host, conn, "§aTeleported.", true);
    }

    /// To the /setspawn spot: a plain teleport in the player's level, a
    /// level change into another.
    fn teleport_to_spawn(&mut self, host: &mut LanHost, conn: u32, p: &spawn_config::SpawnPoint) {
        let Some(i) = host.hub.guests.iter().position(|g| g.conn == conn) else {
            return;
        };
        let g = &mut host.hub.guests[i];
        if g.dimension != p.dimension {
            self.level_teleports.push(LevelTeleport {
                conn,
                dim: Dim::from_id(&p.dimension),
                at: p.at,
                rotation: Some([p.yaw, p.pitch]),
            });
            self.run_level_teleports(host);
            return;
        }
        g.yaw = p.yaw;
        g.pitch = p.pitch;
        g.vehicle = None;
        if let Some(m) = g.motion.as_mut() {
            m.fall_distance = 0.0;
            m.velocity = Vec3::new(0.0, 0.0, 0.0);
        }
        host.hub.teleport(i, Some(p.at));
    }

    /// The swap-hands key during a spawn glide: the one boost along the look,
    /// in place of the swap. False when it is not that glide's boost.
    pub(super) fn spawn_elytra_boost(&mut self, host: &mut LanHost, conn: u32) -> bool {
        let Some(g) = host.hub.guest_mut(conn) else {
            return false;
        };
        if !g.spawn_gliding || g.spawn_boosted {
            return false;
        }
        g.spawn_boosted = true;
        let v = look(g.yaw, g.pitch).map(|c| c * BOOST);
        if let Some(m) = g.motion.as_mut() {
            m.velocity = Vec3::new(v[0], v[1], v[2]);
        }
        g.send(&C::SetEntityMotion(play::SetEntityMotion {
            id: g.entity_id,
            velocity: v,
        }));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_look_follows_yaw_and_pitch() {
        let close = |a: [f64; 3], b: [f64; 3]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9);
        assert!(close(look(0.0, 0.0), [0.0, 0.0, 1.0]));
        assert!(close(look(90.0, 0.0), [-1.0, 0.0, 0.0]));
        assert!(close(look(0.0, -90.0), [0.0, 1.0, 0.0]));
    }
}
