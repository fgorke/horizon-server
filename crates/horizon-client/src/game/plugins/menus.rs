//! The server's side of capability `menus`: a plugin's chest screens (game/lan_plugin_menu.rs),
//! their items read from vanilla's item syntax as `/give` reads it.
use super::run::uuid_string;
use crate::game::{Game, lan::LanHost};
use horizon_client::{
    item_data::{command_components, decode_components},
    items::ItemStack,
    lan::data::component_nbt,
    text::Text,
};
use horizon_mods::MenuAction;

/// `minecraft:stone`, `stone` or `minecraft:player_head[profile={name:"Steve"}]`, as `/give`
/// reads it, one of it; None for "" and, logged, for an item `known` doesn't know or
/// components that don't parse.
fn menu_stack(text: &str, known: impl Fn(&str) -> bool) -> Option<ItemStack> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (id, components) = match text.find('[') {
        Some(at) => (&text[..at], Some(&text[at..])),
        None => (text, None),
    };
    let id = if id.contains(':') {
        id.to_owned()
    } else {
        ["minecraft:", id].concat()
    };
    if !known(&id) {
        log::warn!("a plugin's menu shows {id}, an item this server doesn't know");
        return None;
    }
    let (damage, extra) = match components.map(command_components) {
        Some(Ok(tag)) => decode_components(Some(&tag)),
        Some(Err(e)) => {
            log::warn!("a plugin's menu shows {text}: {e}");
            return None;
        }
        None => (0, None),
    };
    Some(ItemStack {
        item: id.as_str().into(),
        count: 1,
        damage,
        extra,
    })
}

impl Game<'_> {
    /// Makes a plugin's `menus` call for the player with this id; one no longer on is skipped.
    pub(super) fn apply_plugin_menu(
        &mut self,
        host: &mut LanHost,
        player: &str,
        action: MenuAction,
    ) {
        let Some(conn) = host
            .hub
            .guests
            .iter()
            .find(|g| g.placed && uuid_string(g.uuid) == player)
            .map(|g| g.conn)
        else {
            return;
        };
        let stacks = |host: &LanHost, items: &[String]| -> Vec<Option<ItemStack>> {
            let known = |id: &str| host.hub.registry_id("minecraft:item", id).is_some();
            items.iter().map(|s| menu_stack(s, known)).collect()
        };
        match action {
            MenuAction::Open {
                id,
                rows,
                title,
                items,
            } => {
                let title = component_nbt(&Text::markup(&title).component());
                let items = stacks(host, &items);
                self.lan_open_plugin_menu(host, conn, id, rows, title, items);
            }
            MenuAction::Set { id, items } => {
                let items = stacks(host, &items);
                self.lan_set_plugin_menu(host, conn, id, items);
            }
            MenuAction::Close { id } => self.lan_close_plugin_menu(host, conn, id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_read_as_give_reads_them() {
        let any = |_: &str| true;
        assert!(menu_stack("", any).is_none());
        let stone = menu_stack("stone", any).expect("stone");
        assert_eq!(stone.item.as_ref(), "minecraft:stone");
        assert_eq!(stone.count, 1);
        let head = menu_stack(
            "minecraft:player_head[profile={properties:[{name:\"textures\",value:\"e30=\"}]},item_name=\"Melon\"]",
            any,
        )
        .expect("head");
        assert_eq!(head.item.as_ref(), "minecraft:player_head");
        assert!(head.extra.is_some(), "its components are kept");
        assert!(menu_stack("minecraft:stone[item_name=", any).is_none());
        assert!(menu_stack("minecraft:nothing", |_| false).is_none());
    }
}
