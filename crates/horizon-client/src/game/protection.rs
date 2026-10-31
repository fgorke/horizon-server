//! Plugins' protected regions (WIT `world.protect`): where a region denies it, a player not exempt
//! from it breaks, places and uses no blocks, opens no containers, hits no players or mobs, and
//! takes no damage. Checked here, on the server's thread, where vanilla checks spawn protection,
//! so nothing is undone afterwards: the client is told what really is, as for spawn protection.
use super::{Game, lan::LanHost};

/// What a region can deny (`world.protected-action`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Guard {
    Break,
    Place,
    Use,
    Containers,
    Pvp,
    /// Checked each tick, not on a packet: only with plugins.
    #[cfg_attr(not(feature = "plugins"), allow(dead_code))]
    Damage,
    HurtMobs,
}

/// What clicking a block does when the click isn't a placement: opens a container, uses the
/// block (doors, trapdoors, gates, buttons, levers...), or neither (`None`: the item in hand is
/// used on it, as a placement is).
fn block_use(name: &str) -> Option<Guard> {
    let id = name.strip_prefix("minecraft:").unwrap_or(name);
    let id = id.split('[').next().unwrap_or(id);
    const CONTAINERS: [&str; 19] = [
        "chest",
        "trapped_chest",
        "barrel",
        "furnace",
        "blast_furnace",
        "smoker",
        "hopper",
        "dropper",
        "dispenser",
        "brewing_stand",
        "crafter",
        "chiseled_bookshelf",
        "decorated_pot",
        "lectern",
        "jukebox",
        "beacon",
        "campfire",
        "soul_campfire",
        "vault",
    ];
    const USED: [&str; 14] = [
        "lever",
        "repeater",
        "comparator",
        "daylight_detector",
        "note_block",
        "bell",
        "respawn_anchor",
        "cake",
        "flower_pot",
        "composter",
        "cauldron",
        "redstone_wire",
        "dragon_egg",
        "candle",
    ];
    const USED_ENDINGS: [&str; 10] = [
        "_door",
        "_trapdoor",
        "_fence_gate",
        "_button",
        "_bed",
        "_candle",
        "_cauldron",
        "_sign",
        "_shelf",
        "_candle_cake",
    ];
    if CONTAINERS.contains(&id) || id.ends_with("shulker_box") {
        Some(Guard::Containers)
    } else if USED.contains(&id)
        || id.starts_with("potted_")
        || USED_ENDINGS.iter().any(|e| id.ends_with(e))
    {
        Some(Guard::Use)
    } else {
        None
    }
}

/// What hitting or clicking an entity of this type is: decorations count as blocks.
fn entity_guard(kind: &str, attack: bool) -> Option<Guard> {
    let id = kind.strip_prefix("minecraft:").unwrap_or(kind);
    let decoration = matches!(
        id,
        "item_frame"
            | "glow_item_frame"
            | "painting"
            | "armor_stand"
            | "leash_knot"
            | "end_crystal"
    );
    match (attack, id) {
        (true, "player") => Some(Guard::Pvp),
        (true, _) if decoration => Some(Guard::Break),
        (true, _) => Some(Guard::HurtMobs),
        (false, _) if decoration => Some(Guard::Use),
        (false, _) => None,
    }
}

fn block_of(at: [f64; 3]) -> [i32; 3] {
    at.map(|c| c.floor() as i32)
}

#[cfg(feature = "plugins")]
mod regions {
    use super::{Game, Guard, LanHost, block_of};
    use horizon_client::text::Text;
    use horizon_mods::{ProtectedAction, Region};
    use horizon_protocol::packets::play::{self, Clientbound as C};

    /// `UUID.toString()`, as plugins name players.
    fn uuid_text(u: u128) -> String {
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

    fn action(g: Guard) -> ProtectedAction {
        match g {
            Guard::Break => ProtectedAction::BreakBlocks,
            Guard::Place => ProtectedAction::PlaceBlocks,
            Guard::Use => ProtectedAction::UseBlocks,
            Guard::Containers => ProtectedAction::Containers,
            Guard::Pvp => ProtectedAction::Pvp,
            Guard::Damage => ProtectedAction::Damage,
            Guard::HurtMobs => ProtectedAction::HurtMobs,
        }
    }

    fn contains(r: &Region, dimension: &str, pos: [i32; 3]) -> bool {
        let (a, b) = (r.pos1, r.pos2);
        let (a, b) = ([a.0, a.1, a.2], [b.0, b.1, b.2]);
        r.dimension == dimension
            && (0..3).all(|k| (a[k].min(b[k])..=a[k].max(b[k])).contains(&pos[k]))
    }

    /// The first region that denies `guard` at `pos` to the player `uuid`.
    fn denying<'a>(
        regions: &'a [(String, Region)],
        uuid: &str,
        dimension: &str,
        pos: [i32; 3],
        guard: Guard,
    ) -> Option<&'a Region> {
        let want = action(guard);
        regions.iter().map(|(_, r)| r).find(|r| {
            r.deny.contains(&want)
                && !r.exempt.iter().any(|e| e == uuid)
                && contains(r, dimension, pos)
        })
    }

    impl Game<'_> {
        /// True when a region denies `guard` at `pos` to the guest `gi`, who is then told the
        /// region's message on the action bar.
        pub(in crate::game) fn guarded(
            &self,
            host: &LanHost,
            gi: usize,
            pos: [i32; 3],
            guard: Guard,
        ) -> bool {
            let regions = &self.plugins.regions;
            if regions.is_empty() {
                return false;
            }
            let g = &host.hub.guests[gi];
            let uuid = uuid_text(g.uuid);
            let Some(r) = denying(regions, &uuid, &g.dimension, pos, guard) else {
                return false;
            };
            if !r.message.is_empty() {
                let content =
                    horizon_client::lan::data::component_nbt(&Text::markup(&r.message).component());
                host.hub.send_to(
                    g.conn,
                    &C::SystemChat(play::SystemChat {
                        content,
                        overlay: true,
                    }),
                );
            }
            true
        }

        /// Each tick: a survival or adventure guest standing in a region that denies damage
        /// takes none (`Abilities.invulnerable`, as in creative), and gets it back on leaving.
        pub(in crate::game) fn shield_guests(&mut self, host: &mut LanHost) {
            let regions = &self.plugins.regions;
            let shielded = &mut self.plugins.shielded;
            for g in host.hub.guests.iter_mut() {
                if g.creative() || g.spectator() {
                    shielded.remove(&g.uuid);
                    continue;
                }
                let uuid = uuid_text(g.uuid);
                let want = denying(
                    regions,
                    &uuid,
                    &g.dimension,
                    block_of(g.position),
                    Guard::Damage,
                )
                .is_some();
                let Some(m) = g.motion.as_mut() else {
                    continue;
                };
                if want {
                    m.invulnerable = true;
                    shielded.insert(g.uuid);
                } else if shielded.remove(&g.uuid) {
                    m.invulnerable = false;
                }
            }
        }
    }
}

impl Game<'_> {
    /// `UseItemOn` at `pos` from its `face`: a region's denial of what the click does (opening
    /// a container, using the block, or else placing at the face or using the item on the block).
    pub(super) fn guarded_use(
        &self,
        host: &LanHost,
        gi: usize,
        pos: [i32; 3],
        face: [i32; 3],
    ) -> bool {
        let g = &host.hub.guests[gi];
        let holds = g.inventory.get(g.selected).is_some_and(Option::is_some) || g.offhand.is_some();
        // UseItemOn: a sneaking player holding something uses the item, not the block.
        let used = if g.sneaking && holds {
            None
        } else {
            block_use(&self.block_name(pos))
        };
        match used {
            Some(guard) => self.guarded(host, gi, pos, guard),
            None => {
                let next = [pos[0] + face[0], pos[1] + face[1], pos[2] + face[2]];
                self.guarded(host, gi, pos, Guard::Place)
                    || self.guarded(host, gi, next, Guard::Place)
            }
        }
    }

    /// `UseItem` with a bucket (water, lava, a fish, powder snow or an empty one): denied where a
    /// region denies placing on the blocks the player looks at within reach; the blocks and the
    /// inventory are sent back as they are.
    pub(super) fn guarded_bucket(&self, host: &mut LanHost, gi: usize, off_hand: bool) -> bool {
        let g = &host.hub.guests[gi];
        let held = if off_hand {
            g.offhand.as_ref()
        } else {
            g.inventory.get(g.selected).and_then(Option::as_ref)
        };
        if !held.is_some_and(|s| s.item.ends_with("bucket")) {
            return false;
        }
        let eyes = [
            g.position[0],
            g.position[1] + if g.sneaking { 1.27 } else { 1.62 },
            g.position[2],
        ];
        let (yaw, pitch) = (
            f64::from(g.yaw).to_radians(),
            f64::from(g.pitch).to_radians(),
        );
        let look = [
            -yaw.sin() * pitch.cos(),
            -pitch.sin(),
            yaw.cos() * pitch.cos(),
        ];
        let mut seen: Vec<[i32; 3]> = Vec::new();
        for step in 0..=10 {
            let d = f64::from(step) * 0.5;
            let at = block_of([
                eyes[0] + look[0] * d,
                eyes[1] + look[1] * d,
                eyes[2] + look[2] * d,
            ]);
            if !seen.contains(&at) {
                seen.push(at);
            }
        }
        if !seen
            .iter()
            .any(|&at| self.guarded(host, gi, at, Guard::Place))
        {
            return false;
        }
        let conn = host.hub.guests[gi].conn;
        for pos in seen {
            let state = self.world.block(pos[0], pos[1], pos[2]).0 as i32;
            host.hub.send_to(
                conn,
                &horizon_protocol::packets::play::Clientbound::BlockUpdate(
                    horizon_protocol::packets::play::BlockUpdate { pos, state },
                ),
            );
        }
        host.hub.send_inventory(gi);
        true
    }

    /// A blow at an entity of type `kind` standing at `at`: a player (pvp: where either of them
    /// stands), a decoration (break) or a mob (hurt-mobs).
    pub(super) fn guarded_attack(
        &self,
        host: &LanHost,
        gi: usize,
        kind: &str,
        at: [f64; 3],
    ) -> bool {
        let Some(guard) = entity_guard(kind, true) else {
            return false;
        };
        let me = block_of(host.hub.guests[gi].position);
        self.guarded(host, gi, block_of(at), guard)
            || (guard == Guard::Pvp && self.guarded(host, gi, me, guard))
    }

    /// A click on an entity of type `kind` at `at`: item frames and armor stands count as blocks used.
    pub(super) fn guarded_entity_use(
        &self,
        host: &LanHost,
        gi: usize,
        kind: &str,
        at: [f64; 3],
    ) -> bool {
        entity_guard(kind, false).is_some_and(|guard| self.guarded(host, gi, block_of(at), guard))
    }
}

#[cfg(not(feature = "plugins"))]
impl Game<'_> {
    pub(super) fn guarded(
        &self,
        _host: &LanHost,
        _gi: usize,
        _pos: [i32; 3],
        _guard: Guard,
    ) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_on_blocks_are_containers_uses_or_placements() {
        assert_eq!(
            block_use("minecraft:chest[facing=north]"),
            Some(Guard::Containers)
        );
        assert_eq!(
            block_use("minecraft:red_shulker_box"),
            Some(Guard::Containers)
        );
        assert_eq!(
            block_use("minecraft:oak_door[half=lower]"),
            Some(Guard::Use)
        );
        assert_eq!(block_use("minecraft:spruce_trapdoor"), Some(Guard::Use));
        assert_eq!(block_use("minecraft:birch_fence_gate"), Some(Guard::Use));
        assert_eq!(block_use("minecraft:stone_button"), Some(Guard::Use));
        assert_eq!(block_use("minecraft:potted_poppy"), Some(Guard::Use));
        assert_eq!(block_use("minecraft:stone"), None);
        assert_eq!(block_use("minecraft:oak_fence"), None);
    }

    #[test]
    fn entities_are_players_decorations_or_mobs() {
        assert_eq!(entity_guard("minecraft:player", true), Some(Guard::Pvp));
        assert_eq!(
            entity_guard("minecraft:item_frame", true),
            Some(Guard::Break)
        );
        assert_eq!(entity_guard("minecraft:cow", true), Some(Guard::HurtMobs));
        assert_eq!(
            entity_guard("minecraft:armor_stand", false),
            Some(Guard::Use)
        );
        assert_eq!(entity_guard("minecraft:villager", false), None);
    }
}
