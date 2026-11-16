//! A server plugin's chest screen (capability `menus`, game/plugins/menus.rs): a `generic_9xN`
//! menu over items the plugin chose. Nothing in it moves: after each click the client is sent
//! what the server has (the menu's items, the player's inventory and the cursor), and the
//! plugin hears which slot was clicked at the tick's end. The menu closing, or another screen
//! opening in its place, is the plugin's to hear as well.
use super::{Game, lan::LanHost};
use horizon_client::items::ItemStack;
use horizon_protocol::packets::play::{self, Clientbound as C};
use horizon_world::nbt::Tag;

/// The `ContainerInput`s a plugin hears: pickup, quick move, a hotbar key, clone and throw. A
/// drag (quick craft) and a double click (pickup all) follow a click it heard already.
const HEARD_INPUTS: [i32; 5] = [0, 1, 2, 3, 4];

/// A guest's open plugin menu.
pub(super) struct GuestPluginMenu {
    window: i32,
    /// The plugin's id for it.
    id: u32,
    /// Its slots, the menu's rows of 9.
    items: Vec<Option<ItemStack>>,
    state: i32,
}

impl Game<'_> {
    /// Opens plugin menu `id` for guest `conn`: `rows` rows of 9 holding `items`, in place of
    /// the menu that was open.
    #[cfg_attr(not(feature = "plugins"), allow(dead_code))]
    pub(super) fn lan_open_plugin_menu(
        &mut self,
        host: &mut LanHost,
        conn: u32,
        id: u32,
        rows: u8,
        title: Tag,
        mut items: Vec<Option<ItemStack>>,
    ) {
        let rows = rows.clamp(1, 6);
        // ServerPlayer.openMenu closes the menu that was open.
        self.guest_close_all(host, conn);
        let menu = format!("minecraft:generic_9x{rows}");
        let Some(window) = host.hub.open_window(conn, &menu, title) else {
            return;
        };
        items.resize(usize::from(rows) * 9, None);
        host.play.entry(conn).or_default().plugin_menu = Some(GuestPluginMenu {
            window,
            id,
            items,
            state: 0,
        });
        self.lan_send_plugin_menu(host, conn);
    }

    /// Plugin menu `id`'s new slots, when it is the one guest `conn` has open.
    #[cfg_attr(not(feature = "plugins"), allow(dead_code))]
    pub(super) fn lan_set_plugin_menu(
        &mut self,
        host: &mut LanHost,
        conn: u32,
        id: u32,
        items: Vec<Option<ItemStack>>,
    ) {
        let Some(m) = host
            .play
            .get_mut(&conn)
            .and_then(|p| p.plugin_menu.as_mut())
            .filter(|m| m.id == id)
        else {
            return;
        };
        let slots = m.items.len();
        m.items = items;
        m.items.resize(slots, None);
        self.lan_send_plugin_menu(host, conn);
    }

    /// Closes plugin menu `id`, when it is the one guest `conn` has open, as the client's
    /// `ContainerClose` does.
    #[cfg_attr(not(feature = "plugins"), allow(dead_code))]
    pub(super) fn lan_close_plugin_menu(&mut self, host: &mut LanHost, conn: u32, id: u32) {
        let Some(window) = host
            .play
            .get(&conn)
            .and_then(|p| p.plugin_menu.as_ref())
            .filter(|m| m.id == id)
            .map(|m| m.window)
        else {
            return;
        };
        if self.lan_plugin_menu_closed(host, conn, Some(window)) {
            if let Some(gi) = host.hub.guests.iter().position(|g| g.conn == conn) {
                self.guest_carried_back(host, gi);
            }
            host.hub.send_to(
                conn,
                &C::ContainerClose(play::ContainerClose { container: window }),
            );
        }
    }

    /// The menu's slots, the guest's inventory and hotbar after them, and the cursor stack.
    fn lan_send_plugin_menu(&mut self, host: &mut LanHost, conn: u32) {
        let Some(g) = host.hub.guests.iter().find(|g| g.conn == conn) else {
            return;
        };
        let inventory: Vec<Option<ItemStack>> = (9..36)
            .chain(0..9)
            .map(|i| g.inventory[i].clone())
            .collect();
        let carried = host.hub.stack(&g.carried.clone());
        let Some(m) = host
            .play
            .get_mut(&conn)
            .and_then(|p| p.plugin_menu.as_mut())
        else {
            return;
        };
        m.state = (m.state + 1) & 0x7FFF;
        let items = m
            .items
            .iter()
            .chain(&inventory)
            .map(|s| host.hub.stack(s))
            .collect();
        let content = C::ContainerSetContent(play::ContainerSetContent {
            container: m.window,
            state: m.state,
            items,
            carried,
        });
        host.hub.send_to(conn, &content);
    }

    /// A click in the guest's plugin menu; false for other windows. Nothing moves, whatever
    /// the client predicted: it is told what the server has.
    pub(super) fn lan_plugin_menu_click(
        &mut self,
        host: &mut LanHost,
        conn: u32,
        p: &play::ContainerClick,
    ) -> bool {
        let Some(id) = host
            .play
            .get(&conn)
            .and_then(|pl| pl.plugin_menu.as_ref())
            .filter(|m| m.window == p.container)
            .map(|m| m.id)
        else {
            return false;
        };
        self.lan_send_plugin_menu(host, conn);
        if HEARD_INPUTS.contains(&p.input) {
            let button = u8::try_from(p.button).unwrap_or(0);
            self.plugin_menu_click(conn, id, i32::from(p.slot), button, p.input == 1);
        }
        true
    }

    /// The guest's plugin menu (`window`; None closes it whichever it is) closed: its plugin
    /// hears it. False when no such window is open.
    pub(super) fn lan_plugin_menu_closed(
        &mut self,
        host: &mut LanHost,
        conn: u32,
        window: Option<i32>,
    ) -> bool {
        let Some(m) = host.play.get_mut(&conn).and_then(|p| {
            p.plugin_menu
                .take_if(|m| window.is_none_or(|w| m.window == w))
        }) else {
            return false;
        };
        self.plugin_menu_closed(conn, m.id);
        true
    }
}
