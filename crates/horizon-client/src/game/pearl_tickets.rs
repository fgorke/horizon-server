//! Thrown ender pearls load chunks while their owner is online (vanilla's
//! `ThrownEnderpearl.tick` and `ServerPlayer.registerAndUpdateEnderPearlTicket`), so a stasis
//! chamber keeps its chunk, and the farms beside it, running. A pearl finds its owner again by
//! UUID: after a restart or a rejoin its chunk is loaded before the owner's connection exists.
//!
//! Vanilla takes a leaving player's pearls out of the world and puts them back when they join
//! (`ServerPlayer.enderPearls`, saved as `ender_pearls`). Here a pearl whose owner is away stays
//! where it is but doesn't tick, so nothing can hit or discard it, and the chunks of the owner's
//! pearls are kept in their own player file, so those load again as soon as they join.
use super::{Dim, Game};
use horizon_client::lan::guest::Guest;
use horizon_client::lan::playerdata::uuid_ints;
use horizon_client::mobs::Who;
use horizon_client::projectiles::ProjectileKind;
use horizon_world::nbt::Tag;
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// The owner of the chunk tickets the pearls hold (`native_chunk_ticket`).
const OWNER: &str = "horizon:ender_pearl";
/// The player file's list of the chunks their pearls are in.
const SAVED_KEY: &str = "horizon:ender_pearl_chunks";

/// A chunk of a level: the level's id, then the chunk's x and z.
type Spot = (&'static str, i32, i32);

#[derive(Default)]
pub(super) struct PearlTickets {
    /// The chunks of each player's thrown pearls, by owner UUID, as last seen.
    chunks: BTreeMap<[i32; 4], BTreeSet<Spot>>,
    /// The chunks a ticket holds now.
    held: HashSet<Spot>,
    /// The online players whose player file was read.
    read: HashSet<[i32; 4]>,
}

fn level_id(id: &str) -> &'static str {
    Dim::from_id(id).id()
}

/// The chunks a player file lists.
fn saved_spots(saved: Option<&Tag>) -> BTreeSet<Spot> {
    let Some(Tag::List(list)) = saved.and_then(|t| t.get(SAVED_KEY)) else {
        return BTreeSet::new();
    };
    list.iter()
        .filter_map(|t| {
            let Some(Tag::String(dim)) = t.get("dimension") else {
                return None;
            };
            let (Some(Tag::Int(x)), Some(Tag::Int(z))) = (t.get("x"), t.get("z")) else {
                return None;
            };
            Some((level_id(&dim.to_string_lossy()), *x, *z))
        })
        .collect()
}

/// `spots` into a player file's compound; none removes the list.
fn save_spots(saved: &mut Option<Tag>, spots: Option<&BTreeSet<Spot>>) {
    let tag = saved.get_or_insert_with(|| Tag::Compound(Default::default()));
    let Tag::Compound(map) = tag else {
        return;
    };
    match spots.filter(|s| !s.is_empty()) {
        Some(spots) => {
            let list = spots
                .iter()
                .map(|&(dim, x, z)| {
                    Tag::compound([
                        ("dimension", Tag::String(dim.into())),
                        ("x", Tag::Int(x)),
                        ("z", Tag::Int(z)),
                    ])
                })
                .collect();
            map.insert(SAVED_KEY.into(), Tag::List(list));
        }
        None => {
            map.remove(&SAVED_KEY.into());
        }
    }
}

impl Game<'_> {
    /// The UUIDs and connections of the players in the game.
    fn pearl_owners_online(&self) -> Vec<([i32; 4], u32)> {
        self.lan.as_ref().map_or_else(Vec::new, |host| {
            host.hub
                .guests
                .iter()
                .filter(|g| g.placed)
                .map(|g| (uuid_ints(g.uuid), g.conn))
                .collect()
        })
    }

    /// The pearls of the level in place that wait for their owner: thrown by a player who is
    /// away, on a server. They don't tick until the owner is back.
    pub(super) fn pearls_waiting(&self) -> HashSet<u32> {
        if self.options.dedicated.is_none() || self.lan.is_none() {
            return HashSet::new();
        }
        let online = self.pearl_owners_online();
        self.creatures
            .projectiles
            .iter()
            .filter(|p| matches!(p.kind, ProjectileKind::EnderPearl))
            .filter(|p| {
                p.owner_uuid
                    .is_some_and(|u| !online.iter().any(|o| o.0 == u))
            })
            .map(|p| p.id)
            .collect()
    }

    /// A leaving (or saved) player's pearl chunks into their player file.
    pub(super) fn pearl_chunks_into(&self, guest: &mut Guest) {
        let spots = self.pearls.chunks.get(&uuid_ints(guest.uuid));
        save_spots(&mut guest.saved, spots);
    }

    /// Once a tick for the level in place, after its projectiles: each pearl's owner by UUID,
    /// the chunks of every player's pearls, and a chunk ticket on each of an online owner's.
    pub(super) fn tick_ender_pearls(&mut self) {
        if self.options.dedicated.is_none() {
            return;
        }
        let Some(host) = self.lan.as_ref() else {
            return;
        };
        let online = self.pearl_owners_online();
        // A joining player's pearl chunks, from their player file.
        let pearls = &mut self.pearls;
        pearls.read.retain(|u| online.iter().any(|o| o.0 == *u));
        for g in host.hub.guests.iter().filter(|g| g.placed) {
            let uuid = uuid_ints(g.uuid);
            if pearls.read.insert(uuid) {
                let saved = saved_spots(g.saved.as_ref());
                if !saved.is_empty() {
                    pearls.chunks.entry(uuid).or_default().extend(saved);
                }
            }
        }
        let dim = self.dim.id();
        // Each pearl's owner: the online guest of its UUID, or nobody while they're away.
        let mut seen: BTreeMap<[i32; 4], BTreeSet<(i32, i32)>> = BTreeMap::new();
        for p in self.creatures.projectiles.iter_mut() {
            if !matches!(p.kind, ProjectileKind::EnderPearl) {
                continue;
            }
            if p.owner_uuid.is_none()
                && let Some(conn) = p.guest
            {
                p.owner_uuid = online.iter().find(|o| o.1 == conn).map(|o| o.0);
            }
            let Some(uuid) = p.owner_uuid else {
                continue;
            };
            match online.iter().find(|o| o.0 == uuid) {
                Some(&(_, conn)) => {
                    if p.guest != Some(conn) {
                        (p.owner, p.guest) = (Some(Who::Player), Some(conn));
                    }
                }
                None => {
                    if p.guest.take().is_some() && p.owner == Some(Who::Player) {
                        p.owner = None;
                    }
                }
            }
            let chunk = (
                (p.position.x.floor() as i32) >> 4,
                (p.position.z.floor() as i32) >> 4,
            );
            seen.entry(uuid).or_default().insert(chunk);
        }
        // The chunks of this level: one whose entities tick holds what was seen in it; any other
        // keeps what it held (a chunk going away saves and drops its entities first, and the
        // pearl is still there, saved with them).
        let ticking = self.entity_ticking_area();
        let live: HashSet<(i32, i32)> = self
            .pearls
            .chunks
            .values()
            .flatten()
            .filter(|&&(d, x, z)| d == dim && ticking(x * 16 + 8, z * 16 + 8))
            .map(|&(_, x, z)| (x, z))
            .collect();
        drop(ticking);
        let pearls = &mut self.pearls;
        for (uuid, spots) in pearls.chunks.iter_mut() {
            let mine = seen.get(uuid);
            spots.retain(|&(d, x, z)| {
                d != dim || !live.contains(&(x, z)) || mine.is_some_and(|m| m.contains(&(x, z)))
            });
        }
        for (uuid, found) in seen {
            let spots = pearls.chunks.entry(uuid).or_default();
            spots.extend(found.into_iter().map(|(x, z)| (dim, x, z)));
        }
        pearls.chunks.retain(|_, s| !s.is_empty());
        // A ticket on each chunk of an online owner's pearls.
        let want: HashSet<Spot> = pearls
            .chunks
            .iter()
            .filter(|(uuid, _)| online.iter().any(|o| o.0 == **uuid))
            .flat_map(|(_, s)| s.iter().copied())
            .collect();
        let gone: Vec<Spot> = pearls.held.difference(&want).copied().collect();
        let new: Vec<Spot> = want.difference(&pearls.held).copied().collect();
        pearls.held = want;
        for (d, x, z) in gone {
            self.native_chunk_ticket(Dim::from_id(d), OWNER, [x, z], false);
        }
        for (d, x, z) in new {
            self.native_chunk_ticket(Dim::from_id(d), OWNER, [x, z], true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_player_file_keeps_their_pearl_chunks() {
        let mut spots = BTreeSet::new();
        spots.insert(("minecraft:the_nether", -3, 7));
        spots.insert(("minecraft:overworld", 0, 0));
        let mut saved = Some(Tag::compound([("Health", Tag::Float(0))]));
        save_spots(&mut saved, Some(&spots));
        assert_eq!(saved_spots(saved.as_ref()), spots);
        assert!(saved.as_ref().unwrap().get("Health").is_some());
        save_spots(&mut saved, None);
        assert!(saved.as_ref().unwrap().get(SAVED_KEY).is_none());
        assert!(saved_spots(None).is_empty());
    }
}
