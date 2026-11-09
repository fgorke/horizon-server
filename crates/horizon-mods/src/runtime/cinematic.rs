//! Capability `cinematic` (server plugins): what one player alone sees, for cutscenes. Blocks
//! that aren't there, entities nobody else sees (displays, mannequins wearing a player's skin),
//! the camera on one of them, the other players hidden. The server's world stays as it is and
//! everything is vanilla packets, so players need no mod. The calls queue an
//! [`Output::Cinematic`] each, sent after the call returns.
use super::{Output, State};
use std::sync::atomic::{AtomicI32, Ordering};

/// What a plugin shows one player.
#[derive(Clone, Debug, PartialEq)]
pub enum CinematicAction {
    /// `show-blocks`: `(x, y, z, state)` in place of the real blocks.
    Blocks(Vec<([i32; 3], String)>),
    /// `spawn`: a new entity `id` of `kind` (`minecraft:block_display`...), with `nbt` (SNBT).
    Spawn {
        id: i32,
        kind: String,
        at: [f64; 3],
        yaw: f32,
        pitch: f32,
        nbt: String,
    },
    /// `set`: entity `id`'s data, in place of its own.
    Set { id: i32, nbt: String },
    /// `move`: entity `id` to a position.
    Move {
        id: i32,
        at: [f64; 3],
        yaw: f32,
        pitch: f32,
    },
    /// `remove`.
    Remove(Vec<i32>),
    /// `view`: the camera on entity `id`, or the player's own eyes again.
    View(Option<i32>),
    /// `hide-players`.
    HidePlayers(bool),
}

/// The kinds `spawn` makes: none of them moves, falls or takes a player list entry.
const KINDS: [&str; 5] = [
    "minecraft:block_display",
    "minecraft:item_display",
    "minecraft:text_display",
    "minecraft:mannequin",
    "minecraft:armor_stand",
];
/// Blocks `show-blocks` takes per call, entity calls per call, and an nbt's length.
pub(super) const BLOCKS_PER_CALL: usize = 8192;
pub(super) const ENTITY_CALLS: usize = 4096;
const MAX_NBT: usize = 4096;
const MAX_STATE: usize = 256;

/// Ids of the entities `spawn` makes, from where the server's own never reach.
static NEXT_ID: AtomicI32 = AtomicI32::new(0x4000_0000);

fn namespaced(kind: &str) -> String {
    if kind.contains(':') {
        kind.to_owned()
    } else {
        ["minecraft:", kind].concat()
    }
}

/// An nbt `spawn` and `set` take: empty, or one compound in SNBT.
fn check_nbt(nbt: &str) -> Result<(), String> {
    let t = nbt.trim();
    if t.len() > MAX_NBT {
        return Err(format!(
            "an nbt of {} characters: at most {MAX_NBT}",
            t.len()
        ));
    }
    if !t.is_empty() && !(t.starts_with('{') && t.ends_with('}')) {
        return Err("the nbt is one compound, {...}".into());
    }
    Ok(())
}

impl State {
    fn is_on(&self, player: &str) -> bool {
        self.snapshot.players.iter().any(|p| p.id == player)
    }
    /// Queues an entity call unless the call made [`ENTITY_CALLS`] already.
    fn cinematic(&mut self, player: String, action: CinematicAction) -> bool {
        if self.io.cinematic_calls >= ENTITY_CALLS || !self.is_on(&player) {
            return false;
        }
        self.io.cinematic_calls += 1;
        self.out.push(Output::Cinematic { player, action });
        true
    }
}

impl super::plugin_io::cinematic::Host for State {
    fn show_blocks(
        &mut self,
        player: String,
        blocks: Vec<super::plugin_io::cinematic::GhostBlock>,
    ) -> bool {
        let room = BLOCKS_PER_CALL.saturating_sub(self.io.cinematic_blocks);
        if blocks.len() > room || !self.is_on(&player) {
            return false;
        }
        if blocks.is_empty() {
            return true;
        }
        if blocks
            .iter()
            .any(|b| b.state.is_empty() || b.state.len() > MAX_STATE)
        {
            return false;
        }
        self.io.cinematic_blocks += blocks.len();
        let blocks = blocks
            .into_iter()
            .map(|b| ([b.x, b.y, b.z], b.state))
            .collect();
        self.out.push(Output::Cinematic {
            player,
            action: CinematicAction::Blocks(blocks),
        });
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn(
        &mut self,
        player: String,
        kind: String,
        x: f64,
        y: f64,
        z: f64,
        yaw: f32,
        pitch: f32,
        nbt: String,
    ) -> Result<u32, String> {
        let kind = namespaced(&kind);
        if !KINDS.contains(&kind.as_str()) {
            return Err(format!("{kind}: spawn makes {}", KINDS.join(", ")));
        }
        check_nbt(&nbt)?;
        if ![x, y, z].iter().all(|v| v.is_finite()) || !yaw.is_finite() || !pitch.is_finite() {
            return Err("a position or rotation that isn't finite".into());
        }
        if !self.is_on(&player) {
            return Err(format!("no player {player} on"));
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let action = CinematicAction::Spawn {
            id,
            kind,
            at: [x, y, z],
            yaw,
            pitch,
            nbt,
        };
        if !self.cinematic(player, action) {
            return Err(format!("at most {ENTITY_CALLS} entity calls per call"));
        }
        Ok(id as u32)
    }

    fn set(&mut self, player: String, entity: u32, nbt: String) -> bool {
        check_nbt(&nbt).is_ok()
            && self.cinematic(
                player,
                CinematicAction::Set {
                    id: entity as i32,
                    nbt,
                },
            )
    }

    #[allow(clippy::too_many_arguments)]
    fn move_to(
        &mut self,
        player: String,
        entity: u32,
        x: f64,
        y: f64,
        z: f64,
        yaw: f32,
        pitch: f32,
    ) -> bool {
        [x, y, z].iter().all(|v| v.is_finite())
            && yaw.is_finite()
            && pitch.is_finite()
            && self.cinematic(
                player,
                CinematicAction::Move {
                    id: entity as i32,
                    at: [x, y, z],
                    yaw,
                    pitch,
                },
            )
    }

    fn remove(&mut self, player: String, entities: Vec<u32>) -> bool {
        if entities.len() > ENTITY_CALLS {
            return false;
        }
        let ids = entities.into_iter().map(|e| e as i32).collect();
        self.cinematic(player, CinematicAction::Remove(ids))
    }

    fn view(&mut self, player: String, entity: Option<u32>) -> bool {
        self.cinematic(player, CinematicAction::View(entity.map(|e| e as i32)))
    }

    fn hide_players(&mut self, player: String, hidden: bool) -> bool {
        self.cinematic(player, CinematicAction::HidePlayers(hidden))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nbt_is_one_compound() {
        assert!(check_nbt("").is_ok());
        assert!(check_nbt("{block_state:{Name:\"minecraft:stone\"}}").is_ok());
        assert!(check_nbt("block_state:1").is_err());
        assert!(check_nbt(&format!("{{a:\"{}\"}}", "x".repeat(MAX_NBT))).is_err());
    }

    #[test]
    fn kinds_take_their_namespace() {
        assert_eq!(namespaced("mannequin"), "minecraft:mannequin");
        assert_eq!(
            namespaced("minecraft:block_display"),
            "minecraft:block_display"
        );
        assert!(KINDS.contains(&namespaced("item_display").as_str()));
        assert!(!KINDS.contains(&namespaced("zombie").as_str()));
    }
}
