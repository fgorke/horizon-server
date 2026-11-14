//! Thrown ender pearls load chunks while their owner is online (vanilla's
//! `ThrownEnderpearl.tick` and `ServerPlayer.registerAndUpdateEnderPearlTicket`), so a stasis
//! chamber keeps its chunk, and the farms beside it, running. A pearl finds its owner again by
//! UUID: after a restart or a rejoin its chunk is loaded before the owner's connection exists.
//! Where each player's pearls are is kept in the world's `horizon-ender-pearls.txt`, so the
//! chunks load again as soon as the owner joins, in any dimension.
use super::{Dim, Game};
use horizon_client::lan::playerdata::uuid_ints;
use horizon_client::mobs::Who;
use horizon_client::projectiles::ProjectileKind;
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// The owner of the chunk tickets the pearls hold (`native_chunk_ticket`).
const OWNER: &str = "horizon:ender_pearl";
/// The file, in the world's folder.
const FILE: &str = "horizon-ender-pearls.txt";
/// Ticks between writes of a changed file.
const SAVE_EVERY: u32 = 100;

/// A chunk of a level: the level's id, then the chunk's x and z.
type Spot = (&'static str, i32, i32);

#[derive(Default)]
pub(super) struct PearlTickets {
    /// The chunks of each player's thrown pearls, by owner UUID, as last seen.
    chunks: BTreeMap<[i32; 4], BTreeSet<Spot>>,
    /// The chunks a ticket holds now.
    held: HashSet<Spot>,
    read: bool,
    dirty: bool,
    since_save: u32,
}

fn level_id(id: &str) -> &'static str {
    Dim::from_id(id).id()
}

impl PearlTickets {
    fn parse(text: &str) -> BTreeMap<[i32; 4], BTreeSet<Spot>> {
        let mut out: BTreeMap<[i32; 4], BTreeSet<Spot>> = BTreeMap::new();
        for line in text.lines() {
            let w: Vec<&str> = line.split_whitespace().collect();
            let [a, b, c, d, dim, x, z] = w[..] else {
                continue;
            };
            let n = |s: &str| s.parse::<i32>().ok();
            let (Some(a), Some(b), Some(c), Some(d), Some(x), Some(z)) =
                (n(a), n(b), n(c), n(d), n(x), n(z))
            else {
                continue;
            };
            out.entry([a, b, c, d])
                .or_default()
                .insert((level_id(dim), x, z));
        }
        out
    }

    fn text(&self) -> String {
        let mut out = String::new();
        for ([a, b, c, d], spots) in &self.chunks {
            for (dim, x, z) in spots {
                out += &format!("{a} {b} {c} {d} {dim} {x} {z}\n");
            }
        }
        out
    }
}

impl Game<'_> {
    /// The world's folder: the level's save is `dimensions/minecraft/<name>` in it.
    fn pearl_file(&self) -> Option<std::path::PathBuf> {
        self.save.root.ancestors().nth(3).map(|w| w.join(FILE))
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
        let online: Vec<([i32; 4], u32)> = host
            .hub
            .guests
            .iter()
            .filter(|g| g.placed)
            .map(|g| (uuid_ints(g.uuid), g.conn))
            .collect();
        if !self.pearls.read {
            self.pearls.read = true;
            if let Some(text) = self
                .pearl_file()
                .and_then(|f| std::fs::read_to_string(f).ok())
            {
                self.pearls.chunks = PearlTickets::parse(&text);
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
        // The chunks of this level: a loaded one holds what was seen in it, an unloaded one
        // keeps what it held.
        let chunks = &self.world.chunks;
        let pearls = &mut self.pearls;
        for (uuid, spots) in pearls.chunks.iter_mut() {
            let mine = seen.get(uuid);
            let before = spots.len();
            spots.retain(|&(d, x, z)| {
                d != dim
                    || !chunks.contains_key(&(x, z))
                    || mine.is_some_and(|m| m.contains(&(x, z)))
            });
            pearls.dirty |= spots.len() != before;
        }
        for (uuid, found) in seen {
            let spots = pearls.chunks.entry(uuid).or_default();
            for (x, z) in found {
                pearls.dirty |= spots.insert((dim, x, z));
            }
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
        self.pearls.since_save += 1;
        if self.pearls.dirty && self.pearls.since_save >= SAVE_EVERY {
            self.pearls.since_save = 0;
            self.pearls.dirty = false;
            if let Some(file) = self.pearl_file()
                && let Err(e) = std::fs::write(&file, self.pearls.text())
            {
                log::warn!("Horizon: could not save {}: {e}", file.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_keeps_each_owners_chunks() {
        let mut t = PearlTickets::default();
        t.chunks
            .entry([1, 2, 3, -4])
            .or_default()
            .insert(("minecraft:the_nether", -3, 7));
        t.chunks
            .entry([1, 2, 3, -4])
            .or_default()
            .insert(("minecraft:overworld", 0, 0));
        assert_eq!(PearlTickets::parse(&t.text()), t.chunks);
        assert!(PearlTickets::parse("garbage\n1 2 3\n").is_empty());
    }
}
