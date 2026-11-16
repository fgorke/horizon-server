//! Capability `menus` (server plugins): chest screens a plugin fills. Nothing in one moves; the
//! plugin hears each click (`plugin-events.menu-click`) and the menu closing (`menu-closed`).
//! The calls queue an [`Output::Menu`] each, sent after the call returns. A plugin acts only on
//! the menus it opened and hasn't heard closed.
use super::{Output, State};
use std::sync::atomic::{AtomicU32, Ordering};

/// What a plugin does with a player's chest screen.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuAction {
    /// `open`: menu `id`, `rows` rows of 9, its title (markup) and its slots' items in
    /// vanilla's item syntax ("" for an empty slot).
    Open {
        id: u32,
        rows: u8,
        title: String,
        items: Vec<String>,
    },
    /// `set-items`: menu `id`'s slots in place of the ones it showed.
    Set { id: u32, items: Vec<String> },
    /// `close`.
    Close { id: u32 },
}

/// An item's and a title's length, and the menu calls a hook call makes.
const MAX_ITEM: usize = 8192;
const MAX_TITLE: usize = 256;
pub(super) const CALLS: usize = 64;

/// Menu ids, unique among every plugin's.
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// `items` fit a menu of `rows` rows.
fn check_items(rows: u8, items: &[String]) -> Result<(), String> {
    let slots = usize::from(rows) * 9;
    if items.len() > slots {
        return Err(format!("{} items for {slots} slots", items.len()));
    }
    match items.iter().position(|s| s.len() > MAX_ITEM) {
        Some(i) => Err(format!("item {i} is over {MAX_ITEM} characters")),
        None => Ok(()),
    }
}

impl State {
    /// Queues a menu call unless the call made [`CALLS`] already.
    fn menu(&mut self, player: String, action: MenuAction) -> bool {
        if self.io.menu_calls >= CALLS {
            return false;
        }
        self.io.menu_calls += 1;
        self.out.push(Output::Menu { player, action });
        true
    }
    /// The rows of `menu`, when it is this plugin's and open for `player`.
    fn menu_rows(&self, player: &str, menu: u32) -> Option<u8> {
        self.io
            .menus
            .get(&menu)
            .filter(|(owner, _)| owner == player)
            .map(|(_, rows)| *rows)
    }
}

impl super::plugin_io::menus::Host for State {
    fn open(
        &mut self,
        player: String,
        rows: u8,
        title: String,
        items: Vec<String>,
    ) -> Result<u32, String> {
        if !(1..=6).contains(&rows) {
            return Err(format!("{rows} rows: a menu has 1 to 6"));
        }
        if title.len() > MAX_TITLE {
            return Err(format!("a title over {MAX_TITLE} characters"));
        }
        check_items(rows, &items)?;
        if !self.snapshot.players.iter().any(|p| p.id == player) {
            return Err(format!("no player {player} on"));
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let action = MenuAction::Open {
            id,
            rows,
            title,
            items,
        };
        if !self.menu(player.clone(), action) {
            return Err(format!("at most {CALLS} menu calls per call"));
        }
        self.io.menus.insert(id, (player, rows));
        Ok(id)
    }

    fn set_items(&mut self, player: String, menu: u32, items: Vec<String>) -> bool {
        let Some(rows) = self.menu_rows(&player, menu) else {
            return false;
        };
        check_items(rows, &items).is_ok() && self.menu(player, MenuAction::Set { id: menu, items })
    }

    fn close(&mut self, player: String, menu: u32) -> bool {
        self.menu_rows(&player, menu).is_some() && self.menu(player, MenuAction::Close { id: menu })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_fit_the_rows() {
        assert!(check_items(1, &vec![String::new(); 9]).is_ok());
        assert!(check_items(1, &vec![String::new(); 10]).is_err());
        assert!(check_items(6, &vec!["minecraft:stone".into(); 54]).is_ok());
        assert!(check_items(2, &["x".repeat(MAX_ITEM + 1)]).is_err());
    }

    #[test]
    fn ids_differ() {
        let a = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let b = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        assert_ne!(a, b);
    }
}
