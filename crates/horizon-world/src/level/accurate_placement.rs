//! Litematica's accurate placement protocol, version 3 (`PlacementHandler.applyPlacementProtocolV3`
//! in Litematica, which Servux servers run): Easy Place sends the state it wants in the click's x,
//! as `2 + value` blocks past the clicked block, and the server sets the placed block's facing and
//! a few other properties from `value`. Litematica picks version 3 on its own on a server that
//! sends it a `servux:` payload (the paster plugin's hello).
//!
//! `value`, lowest bit first: one unused bit; when the block has a `facing`, three bits, its
//! `Direction.get3DDataValue` (6: the opposite of what vanilla chose); then, by property name,
//! each allowed property's index among its sorted values, in as many bits as its value count needs.
use super::{
    LevelAccess, Sim,
    place::{PlacementContext, from_y_rot},
    states::PropKey,
    update::rel,
};
use crate::registry::BlockStateId;
use horizon_core::geometry::Direction;
use std::cell::Cell;

thread_local! {
    /// The value of the click being handled, if it carried one.
    static VALUE: Cell<Option<u32>> = const { Cell::new(None) };
}

/// Puts the value back when the click is done, even if it unwinds.
struct Restore(Option<u32>);

impl Drop for Restore {
    fn drop(&mut self) {
        VALUE.set(self.0);
    }
}

/// Runs `f` (a use of an item on a block) with the click's protocol `value`: the blocks it places
/// take the state the value asks for.
pub fn with_value<R>(value: Option<u32>, f: impl FnOnce() -> R) -> R {
    let _restore = Restore(VALUE.replace(value));
    f()
}

/// The protocol value in a click's x within the block (`ServerboundUseItemOnPacket`'s cursor), and
/// the x without it. A vanilla click's x is within the block and carries none.
pub fn split_cursor_x(x: f32) -> (f32, Option<u32>) {
    if !x.is_finite() || x < 2.0 {
        return (x, None);
    }
    let whole = x.floor();
    // Litematica's `(int) (hitVec.x - pos.x) - 2`; an f32 holds whole numbers to 2^24 exactly.
    let value = (whole as u64).saturating_sub(2).min(u64::from(u32::MAX)) as u32;
    (x - whole, Some(value))
}

/// Litematica's `WHITELISTED_PROPERTIES`, by name; where a name is shared, the values tell the
/// allowed property from the others (`half` of stairs, not of doors; `axis` of logs, not of
/// portals; `type` of chests and slabs, not of pistons; `mode` of comparators).
fn allowed(name: &str, values: &[&str]) -> bool {
    match name {
        "attachment" | "bites" | "copper_golem_pose" | "delay" | "face" | "facing" | "hinge"
        | "inverted" | "note" | "open" | "orientation" | "rotation" | "shape" => true,
        "axis" => values.len() == 3,
        "half" => values.contains(&"top"),
        "type" => values.contains(&"single") || values.contains(&"double"),
        "mode" => values.contains(&"subtract"),
        _ => false,
    }
}

/// `Mth.log2(Mth.smallestEncompassingPowerOfTwo(count))`.
fn bits_for(count: usize) -> u32 {
    count.max(1).next_power_of_two().trailing_zeros()
}

/// A property's values in Java's `Comparable` order: false before true, numbers ascending, enums
/// in declaration order (the order the state table has them in).
fn sorted_values(mut values: Vec<(&str, Option<i32>, Option<bool>)>) -> Vec<&str> {
    if values.iter().all(|v| v.2.is_some()) {
        values.sort_by_key(|v| v.2);
    } else if values.iter().all(|v| v.1.is_some()) {
        values.sort_by_key(|v| v.1);
    }
    values.into_iter().map(|v| v.0).collect()
}

impl<L: LevelAccess + ?Sized> Sim<'_, L> {
    /// `state`, the block's own choice for `ctx`, as the click's protocol value (if any) asks for
    /// it; None when a bed's head would not fit where the value turns it.
    pub(super) fn accurate_placement(
        &self,
        state: BlockStateId,
        ctx: &PlacementContext,
    ) -> Option<BlockStateId> {
        match VALUE.get() {
            Some(value) => self.apply_value(state, ctx, value),
            None => Some(state),
        }
    }

    fn apply_value(
        &self,
        mut state: BlockStateId,
        ctx: &PlacementContext,
        mut value: u32,
    ) -> Option<BlockStateId> {
        let table = &self.b.table;
        let block = table.block(state);
        let mut keys: Vec<(PropKey, &str)> = table.property_keys(block).collect();
        keys.sort_by(|a, b| a.1.cmp(b.1));
        // BlockUtils.getFirstDirectionProperty: the first by name whose values are directions,
        // `facing` (pointed dripstone's `vertical_direction` is skipped).
        let facing = keys
            .iter()
            .find(|(key, _)| table.values(block, *key).all(|v| v.direction.is_some()))
            .filter(|(_, name)| *name == "facing")
            .map(|(key, _)| *key);
        if let Some(key) = facing {
            state = self.apply_facing(state, ctx, key, value)?;
            value >>= 3;
        }
        value >>= 1;
        for (key, name) in keys {
            if Some(key) == facing {
                continue;
            }
            let values = sorted_values(
                table
                    .values(block, key)
                    .map(|v| (&*v.text, v.int, v.boolean))
                    .collect(),
            );
            if !allowed(name, &values) {
                continue;
            }
            let bits = bits_for(values.len());
            let index = (value & ((1 << bits) - 1)) as usize;
            let Some(&wanted) = values.get(index) else {
                continue;
            };
            // No double slab from one slab.
            if table.text(state, key) != wanted && !(name == "type" && wanted == "double") {
                state = table.set_text(state, key, wanted);
            }
            value >>= bits;
        }
        Some(state)
    }

    /// `PlacementHandler.applyDirectionProperty`.
    fn apply_facing(
        &self,
        state: BlockStateId,
        ctx: &PlacementContext,
        key: PropKey,
        value: u32,
    ) -> Option<BlockStateId> {
        let table = &self.b.table;
        let block = table.block(state);
        let possible = |d: Direction| table.values(block, key).any(|v| v.direction == Some(d));
        let Some(current) = table.dir(state, key) else {
            return Some(state);
        };
        let wanted = match (value & 0xF) >> 1 {
            6 => current.opposite(),
            index @ 0..=5 => {
                let d = Direction::ALL[index as usize];
                if possible(d) {
                    d
                } else {
                    from_y_rot(f64::from(ctx.yaw)).opposite()
                }
            }
            _ => current,
        };
        if wanted == current || !possible(wanted) {
            return Some(state);
        }
        // A bed turned another way needs room for its head there.
        if table.name(state).ends_with("_bed") {
            let head = rel(ctx.pos, wanted);
            if !self.can_be_replaced(self.state(head), ctx) {
                return None;
            }
        }
        Some(table.set_dir(state, key, wanted))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_value_sits_past_two_blocks() {
        assert_eq!(split_cursor_x(0.25), (0.25, None));
        assert_eq!(split_cursor_x(1.0), (1.0, None));
        assert_eq!(split_cursor_x(2.5), (0.5, Some(0)));
        assert_eq!(split_cursor_x(2.0 + 37.75), (0.75, Some(37)));
        assert_eq!(split_cursor_x(f32::NAN).1, None);
        assert_eq!(split_cursor_x(-3.0), (-3.0, None));
    }

    #[test]
    fn values_sort_as_java_compares_them() {
        let bools = vec![("true", None, Some(true)), ("false", None, Some(false))];
        assert_eq!(sorted_values(bools), ["false", "true"]);
        let ints = vec![
            ("3", Some(3), None),
            ("1", Some(1), None),
            ("2", Some(2), None),
        ];
        assert_eq!(sorted_values(ints), ["1", "2", "3"]);
        let enums = vec![("top", None, None), ("bottom", None, None)];
        assert_eq!(sorted_values(enums), ["top", "bottom"]);
    }

    #[test]
    fn bits_cover_the_values() {
        assert_eq!(bits_for(1), 0);
        assert_eq!(bits_for(2), 1);
        assert_eq!(bits_for(3), 2);
        assert_eq!(bits_for(4), 2);
        assert_eq!(bits_for(10), 4);
        assert_eq!(bits_for(16), 4);
    }

    #[test]
    fn shared_names_allow_only_litematicas_properties() {
        assert!(allowed("half", &["top", "bottom"]));
        assert!(!allowed("half", &["upper", "lower"]));
        assert!(allowed("axis", &["x", "y", "z"]));
        assert!(!allowed("axis", &["x", "z"]));
        assert!(allowed("type", &["top", "bottom", "double"]));
        assert!(allowed("type", &["single", "left", "right"]));
        assert!(!allowed("type", &["normal", "sticky"]));
        assert!(allowed("mode", &["compare", "subtract"]));
        assert!(!allowed("mode", &["save", "load", "corner", "data"]));
        assert!(!allowed("waterlogged", &["false", "true"]));
        assert!(!allowed("powered", &["false", "true"]));
    }
}
