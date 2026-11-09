//! The server's side of capability `cinematic`: what one player alone sees. Blocks go out as
//! block updates (a section's in one packet), entities as the packets the server sends of its
//! own, built from their nbt as `native_client_entities` builds a native mod's, and kept in the
//! guest's `client_entities`; the camera is `ClientboundSetCameraPacket`, and the other players
//! are hidden through the guest's tracker. Nothing of it touches the world.
use super::run::uuid_string;
use crate::game::{Game, lan::LanHost, render_control::parse_state};
use horizon_client::{fast_hash::FxHashMap, lan::entity::EntityState};
use horizon_mods::CinematicAction;
use horizon_protocol::{
    buf::pack_degrees,
    entity_data::{Profile, Value, serializer},
    packets::play::{self, Clientbound as C},
};
use horizon_world::nbt::{Tag, parse_snbt};

/// `Avatar.DATA_PLAYER_MODE_CUSTOMISATION`, every skin layer shown; `Mannequin.DATA_PROFILE` and
/// `DATA_DESCRIPTION` (none: no "NPC" line under its name).
const SKIN_PARTS: u8 = 16;
const PROFILE: u8 = 17;
const DESCRIPTION: u8 = 19;
const ALL_LAYERS: i8 = 0x7F;

/// A spawned entity's UUID: its id under a prefix no player or server entity has.
fn entity_uuid(id: i32) -> u128 {
    0x6369_6e65_6d61_0000_0000_0000_0000_0000 | u128::from(id as u32)
}

/// An nbt `spawn` or `set` passed, as a compound; empty for none. None, logged, when it isn't
/// SNBT.
fn nbt_tag(nbt: &str) -> Option<Tag> {
    let text = nbt.trim();
    if text.is_empty() {
        return Some(Tag::Compound(Default::default()));
    }
    match parse_snbt(text) {
        Ok((tag @ Tag::Compound(_), _)) => Some(tag),
        Ok(_) => None,
        Err(e) => {
            log::warn!("a plugin's entity nbt {text}: {e}");
            None
        }
    }
}

impl Game<'_> {
    /// Makes a plugin's `cinematic` call for the player with this id; one no longer on is
    /// skipped.
    pub(super) fn apply_plugin_cinematic(
        &mut self,
        host: &mut LanHost,
        player: &str,
        action: CinematicAction,
    ) {
        let Some(at) = host
            .hub
            .guests
            .iter()
            .position(|g| g.placed && uuid_string(g.uuid) == player)
        else {
            return;
        };
        match action {
            CinematicAction::Blocks(blocks) => self.send_ghost_blocks(host, at, &blocks),
            CinematicAction::Spawn {
                id,
                kind,
                at: pos,
                yaw,
                pitch,
                nbt,
            } => self.spawn_cinematic_entity(host, at, id, &kind, pos, [yaw, pitch], &nbt),
            CinematicAction::Set { id, nbt } => self.set_cinematic_entity(host, at, id, &nbt),
            CinematicAction::Move {
                id,
                at: pos,
                yaw,
                pitch,
            } => {
                let g = &host.hub.guests[at];
                let Some(kind) = g.client_entities.get(&id) else {
                    return;
                };
                g.send(&C::TeleportEntity(play::TeleportEntity {
                    id,
                    position: pos,
                    delta: [0.0; 3],
                    yaw,
                    pitch,
                    relatives: 0,
                    on_ground: false,
                }));
                if kind == "minecraft:mannequin" || kind == "minecraft:armor_stand" {
                    g.send(&C::RotateHead(play::RotateHead {
                        id,
                        head_yaw: pack_degrees(yaw),
                    }));
                }
            }
            CinematicAction::Remove(ids) => {
                let g = &mut host.hub.guests[at];
                let ids: Vec<i32> = ids
                    .into_iter()
                    .filter(|id| g.client_entities.remove(id).is_some())
                    .collect();
                if !ids.is_empty() {
                    g.send(&C::RemoveEntities(play::RemoveEntities { ids }));
                }
            }
            CinematicAction::View(id) => {
                let g = &mut host.hub.guests[at];
                let camera_id = match id {
                    Some(id) if g.client_entities.contains_key(&id) => id,
                    Some(_) => return,
                    None => g.entity_id,
                };
                g.send(&C::SetCamera(play::SetCamera { camera_id }));
            }
            CinematicAction::HidePlayers(hidden) => {
                let others: Vec<i32> = host
                    .hub
                    .guests
                    .iter()
                    .enumerate()
                    .filter(|(i, o)| *i != at && o.placed)
                    .map(|(_, o)| o.entity_id)
                    .collect();
                let g = &mut host.hub.guests[at];
                for id in others {
                    if hidden {
                        g.tracker.hidden.insert(id);
                    } else {
                        g.tracker.hidden.remove(&id);
                    }
                }
                // Its pairs are checked again on the next tick.
                g.moved = true;
            }
        }
    }

    /// Block updates to one guest, a section's changes in one packet.
    fn send_ghost_blocks(&self, host: &LanHost, at: usize, blocks: &[([i32; 3], String)]) {
        let registry = self.pack.assets.registry;
        let mut sections: FxHashMap<[i32; 3], Vec<i64>> = FxHashMap::default();
        for ([x, y, z], state) in blocks {
            let state = match parse_state(registry, state) {
                Ok(state) => state,
                Err(e) => {
                    log::warn!("a plugin showed {state}: {e}");
                    continue;
                }
            };
            sections.entry([x >> 4, y >> 4, z >> 4]).or_default().push(
                i64::from(state.0) << 12
                    | i64::from(x & 15) << 8
                    | i64::from(z & 15) << 4
                    | i64::from(y & 15),
            );
        }
        let g = &host.hub.guests[at];
        for (section, changes) in sections {
            g.send(&C::SectionBlocksUpdate(play::SectionBlocksUpdate {
                section,
                changes,
            }));
        }
    }

    /// The synched data `tag` gives an entity of `kind`, as `/summon` reads it, and for a
    /// mannequin its skin: `profile` names an online player (their skin) or any player (the
    /// client looks it up).
    fn cinematic_entity_data(
        &self,
        host: &mut LanHost,
        kind: &str,
        tag: &Tag,
        all: bool,
    ) -> Vec<(u8, i32, Value)> {
        let mut e = EntityState {
            kind: kind.to_owned(),
            ..Default::default()
        };
        self.lan_display_data(tag, &mut host.hub, &mut e);
        let (meta, defaults) = crate::game::native_client_entities::entity_data(tag);
        e.meta.extend(meta);
        if all {
            // What the nbt leaves out goes back to its default, and a display's
            // `start_interpolation` goes with its transformation: it eases.
            let mut values = e.defaults;
            values.extend(defaults);
            for v in &mut values {
                if let Some(m) = e.meta.iter().find(|m| m.0 == v.0) {
                    v.clone_from(m);
                }
            }
            let rest: Vec<_> = e
                .meta
                .iter()
                .filter(|m| !values.iter().any(|v| v.0 == m.0))
                .cloned()
                .collect();
            values.extend(rest);
            e.meta = values;
        }
        if kind == "minecraft:mannequin" {
            e.meta
                .push((SKIN_PARTS, serializer::BYTE, Value::Byte(ALL_LAYERS)));
            e.meta.push((
                DESCRIPTION,
                serializer::OPTIONAL_COMPONENT,
                Value::OptText(None),
            ));
            if let Some(Tag::String(name)) = tag.get("profile") {
                let name = name.to_string_lossy().to_string();
                let online = host
                    .hub
                    .guests
                    .iter()
                    .find(|g| g.placed && g.name.eq_ignore_ascii_case(&name));
                let profile = match online {
                    Some(g) => Profile {
                        name: Some(g.name.clone()),
                        uuid: Some(g.uuid),
                        properties: g.properties.clone(),
                        ..Profile::default()
                    },
                    None => Profile {
                        name: Some(name),
                        ..Profile::default()
                    },
                };
                e.meta.push((
                    PROFILE,
                    serializer::RESOLVABLE_PROFILE,
                    Value::Profile(profile),
                ));
            }
        }
        e.meta
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_cinematic_entity(
        &mut self,
        host: &mut LanHost,
        at: usize,
        id: i32,
        kind: &str,
        pos: [f64; 3],
        [yaw, pitch]: [f32; 2],
        nbt: &str,
    ) {
        let Some(type_id) = host.hub.registry_id("minecraft:entity_type", kind) else {
            log::warn!("a plugin spawned {kind}, which this server has no id for");
            return;
        };
        let Some(tag) = nbt_tag(nbt) else {
            return;
        };
        let values = self.cinematic_entity_data(host, kind, &tag, false);
        let g = &mut host.hub.guests[at];
        g.client_entities.insert(id, kind.to_owned());
        g.send(&C::AddEntity(play::AddEntity {
            id,
            uuid: entity_uuid(id),
            kind: type_id,
            position: pos,
            velocity: [0.0; 3],
            pitch: pack_degrees(pitch),
            yaw: pack_degrees(yaw),
            head_yaw: pack_degrees(yaw),
            data: 0,
        }));
        if !values.is_empty() {
            g.send(&C::SetEntityData(play::SetEntityData { id, values }));
        }
    }

    fn set_cinematic_entity(&mut self, host: &mut LanHost, at: usize, id: i32, nbt: &str) {
        let Some(kind) = host.hub.guests[at].client_entities.get(&id).cloned() else {
            return;
        };
        let Some(tag) = nbt_tag(nbt) else {
            return;
        };
        let values = self.cinematic_entity_data(host, &kind, &tag, true);
        if !values.is_empty() {
            host.hub.guests[at].send(&C::SetEntityData(play::SetEntityData { id, values }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nbt_is_a_compound_or_nothing() {
        assert!(matches!(nbt_tag(""), Some(Tag::Compound(_))));
        assert!(matches!(
            nbt_tag("{block_state:{Name:\"minecraft:stone\"},teleport_duration:3}"),
            Some(Tag::Compound(_))
        ));
        assert!(nbt_tag("{oops").is_none());
        assert!(nbt_tag("3").is_none());
    }

    #[test]
    fn entity_uuids_differ_by_id() {
        assert_ne!(entity_uuid(0x4000_0000), entity_uuid(0x4000_0001));
        assert_eq!(entity_uuid(7) as u32, 7);
    }
}
