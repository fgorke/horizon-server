//! Spawn elytra: within [`RADIUS`] blocks of the world spawn in the Overworld a
//! survival player may double-jump into a glide without an elytra, boost once
//! with the swap-hands key, and lands without fall damage. The glide goes on
//! past the radius until the player lands.

use super::{Dim, Game, lan::LanHost};
use horizon_core::geometry::Vec3;
use horizon_protocol::packets::play::{self, Clientbound as C};

/// How far from the world spawn, horizontally, a glide may start.
const RADIUS: f64 = 30.0;
/// The boost's speed along the look, in blocks per tick.
const BOOST: f64 = 2.0;

/// Whether `at` is within the radius of the spawn block `spawn`.
fn near_spawn(at: [f64; 3], spawn: [i32; 3]) -> bool {
    let dx = at[0] - (f64::from(spawn[0]) + 0.5);
    let dz = at[2] - (f64::from(spawn[2]) + 0.5);
    dx * dx + dz * dz <= RADIUS * RADIUS
}

/// The unit look vector of a yaw and pitch in degrees (Entity.calculateViewVector).
fn look(yaw: f32, pitch: f32) -> [f64; 3] {
    let (yaw, pitch) = (f64::from(yaw).to_radians(), f64::from(pitch).to_radians());
    [-yaw.sin() * pitch.cos(), -pitch.sin(), yaw.cos() * pitch.cos()]
}

impl Game<'_> {
    /// Once a tick for a guest, before its glide tick: starts a glide the
    /// double jump asked for, keeps the fall distance at zero while it lasts
    /// (and on the landing), ends it on landing, and lets the client fly-jump
    /// only while it is near the spawn and not gliding.
    pub(super) fn spawn_elytra_tick(&mut self, host: &mut LanHost, conn: u32) {
        let spawn = self.level.world_spawn.filter(|_| self.dim == Dim::Overworld);
        let Some(g) = host.hub.guest_mut(conn) else {
            return;
        };
        let survival = !g.creative() && !g.spectator();
        let near = survival && spawn.is_some_and(|s| near_spawn(g.position, s));
        let jumped = std::mem::take(&mut g.spawn_jump);
        let started = jumped && near && !g.on_ground && g.vehicle.is_none() && !g.fall_flying;
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
            let hint = horizon_client::text::Text::legacy("§bDrücke F für einen Boost");
            g.send(&C::SystemChat(play::SystemChat {
                content: horizon_client::lan::data::component_nbt(&hint.component()),
                overlay: true,
            }));
        }
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
    fn the_radius_is_horizontal_around_the_spawn_block() {
        let spawn = [0, 64, 0];
        assert!(near_spawn([0.5, 200.0, 0.5], spawn));
        assert!(near_spawn([30.0, 64.0, 0.5], spawn));
        assert!(!near_spawn([31.0, 64.0, 0.5], spawn));
        assert!(!near_spawn([22.0, 64.0, 22.0], spawn));
    }

    #[test]
    fn the_look_follows_yaw_and_pitch() {
        let close = |a: [f64; 3], b: [f64; 3]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9);
        assert!(close(look(0.0, 0.0), [0.0, 0.0, 1.0]));
        assert!(close(look(90.0, 0.0), [-1.0, 0.0, 0.0]));
        assert!(close(look(0.0, -90.0), [0.0, 1.0, 0.0]));
    }
}
