//! Horizon Server plugin (`docs/plugins.md`): a head database after
//! [HeadDB](https://github.com/SilentDevelopment/HeadDB). `/heads` opens a chest screen with the
//! catalog's categories; a category or a search (`/heads <words>`) lists its heads 45 a page,
//! and a click gives one (shift: 64). `/heads spieler <name>` gives a player's head. The catalog
//! (over 100 000 heads, `db`) is built into the plugin; its screens are capability `menus`.
use horizon_mod_sdk::{
    command,
    server::{Args, MenuInput, Player, Registration, ServerMod, menus, players, random},
};
use std::collections::BTreeMap;

mod db;
use db::{Db, Head, texture_value};

/// A screen's rows, and the heads a page lists (the rows above the bottom one).
const ROWS: u8 = 6;
const PAGE: usize = 45;
/// The bottom row's buttons.
const PREVIOUS: i32 = 45;
const RANDOM: i32 = 47;
const BACK: i32 = 48;
const PAGE_INFO: i32 = 49;
const SEARCH: i32 = 50;
const OWN_HEAD: i32 = 51;
const CLOSE: i32 = 52;
const NEXT: i32 = 53;
/// Where the main screen shows the categories: three rows of five.
const CATEGORY_SLOTS: [i32; 15] = [11, 12, 13, 14, 15, 20, 21, 22, 23, 24, 29, 30, 31, 32, 33];

/// Each category's name, by catalog id.
fn category_name(id: &str) -> &str {
    match id {
        "alphabet" => "Buchstaben",
        "animals" => "Tiere",
        "blocks" => "Blöcke",
        "decoration" => "Deko",
        "emojis" => "Emojis",
        "flags" => "Flaggen",
        "food-and-drinks" => "Essen & Trinken",
        "helmets" => "Helme",
        "humanoid" => "Fantasy",
        "humans" => "Menschen",
        "icons" => "Symbole",
        "logos" => "Logos",
        "miscellaneous" => "Sonstiges",
        "monsters" => "Monster",
        "plants" => "Pflanzen",
        other => other,
    }
}

/// `text` inside SNBT's double quotes.
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// An item in vanilla's item syntax: `id` named `name` (not italic), lore lines (text, colour).
fn item(id: &str, extra: &str, name: &str, lore: &[(&str, &str)]) -> String {
    let lore: Vec<String> = lore
        .iter()
        .map(|(text, color)| format!("{{text:{},color:\"{color}\",italic:false}}", quoted(text)))
        .collect();
    let mut components = vec![format!("item_name={}", quoted(name))];
    if !extra.is_empty() {
        components.insert(0, extra.to_owned());
    }
    if !lore.is_empty() {
        components.push(format!("lore=[{}]", lore.join(",")));
    }
    format!("{id}[{}]", components.join(","))
}

/// A head's skin, as `profile` takes it.
fn profile(texture: &str) -> String {
    format!(
        "profile={{properties:[{{name:\"textures\",value:\"{}\"}}]}}",
        texture_value(texture)
    )
}

/// The background of the bottom row.
fn filler() -> String {
    "minecraft:gray_stained_glass_pane[tooltip_display={hide_tooltip:true}]".to_owned()
}

/// What a player's screen shows.
enum View {
    Main,
    /// Heads in a list: a category's or a search's, with the screen's title.
    List {
        ids: Vec<u32>,
        page: usize,
    },
}

struct Session {
    menu: u32,
    view: View,
}

struct Heads {
    db: Db,
    /// By player id.
    sessions: BTreeMap<String, Session>,
}

impl Heads {
    fn category_label(&self, c: usize) -> &str {
        category_name(&self.db.categories[c])
    }

    /// The head as a menu shows it.
    fn head_item(&self, h: &Head<'_>, hints: bool) -> String {
        let category = self.category_label(h.category);
        let mut lore = vec![(category, "dark_gray")];
        let tags: Vec<&str> = h
            .tags
            .iter()
            .take(3)
            .filter_map(|&t| self.db.tags.get(t).map(String::as_str))
            .collect();
        let tags = tags.join(", ");
        if !tags.is_empty() {
            lore.push((tags.as_str(), "gray"));
        }
        if hints {
            lore.push(("Klick: 1 Kopf", "yellow"));
            lore.push(("Shift-Klick: 64 Köpfe", "yellow"));
        }
        item("minecraft:player_head", &profile(&h.texture), h.name, &lore)
    }

    fn main_items(&self) -> Vec<String> {
        let mut items = vec![String::new(); usize::from(ROWS) * 9];
        for (c, &slot) in CATEGORY_SLOTS.iter().enumerate() {
            let Some(heads) = self.db.by_category.get(c) else {
                continue;
            };
            let Some(icon) = heads.first().and_then(|&i| self.db.head(i)) else {
                continue;
            };
            let count = format!("{} Köpfe", heads.len());
            items[slot as usize] = item(
                "minecraft:player_head",
                &profile(&icon.texture),
                self.category_label(c),
                &[(&count, "gray"), ("Klick: öffnen", "yellow")],
            );
        }
        self.bottom_row(&mut items, None);
        items
    }

    /// The bottom row: on a list with its page and pages, the page buttons.
    fn bottom_row(&self, items: &mut [String], page: Option<(usize, usize)>) {
        for slot in 45..54 {
            items[slot] = filler();
        }
        let mut set = |slot: i32, value: String| items[slot as usize] = value;
        set(
            SEARCH,
            item(
                "minecraft:oak_sign",
                "",
                "Suchen",
                &[("Tippe /heads <Suchwort>", "gray")],
            ),
        );
        set(
            OWN_HEAD,
            item(
                "minecraft:player_head",
                "",
                "Spielerkopf",
                &[
                    ("Klick: dein eigener Kopf", "gray"),
                    ("/heads spieler <Name>", "gray"),
                ],
            ),
        );
        set(CLOSE, item("minecraft:barrier", "", "Schließen", &[]));
        let Some((page, pages)) = page else {
            set(
                RANDOM,
                item(
                    "minecraft:ender_eye",
                    "",
                    "Zufällige Köpfe",
                    &[(&format!("{} Köpfe im Katalog", self.db.len()), "gray")],
                ),
            );
            return;
        };
        set(
            BACK,
            item(
                "minecraft:book",
                "",
                "Zurück",
                &[("Zu den Kategorien", "gray")],
            ),
        );
        set(
            PAGE_INFO,
            item(
                "minecraft:paper",
                "",
                &format!("Seite {}/{pages}", page + 1),
                &[],
            ),
        );
        if page > 0 {
            set(
                PREVIOUS,
                item("minecraft:arrow", "", "« Vorherige Seite", &[]),
            );
        }
        if page + 1 < pages {
            set(NEXT, item("minecraft:arrow", "", "Nächste Seite »", &[]));
        }
    }

    fn list_items(&self, ids: &[u32], page: usize) -> Vec<String> {
        let mut items = vec![String::new(); usize::from(ROWS) * 9];
        for (slot, &i) in ids.iter().skip(page * PAGE).take(PAGE).enumerate() {
            if let Some(h) = self.db.head(i) {
                items[slot] = self.head_item(&h, true);
            }
        }
        let pages = ids.len().div_ceil(PAGE).max(1);
        self.bottom_row(&mut items, Some((page, pages)));
        items
    }

    fn open_main(&mut self, player: &Player) -> Result<(), String> {
        let menu = menus::open(&player.id, ROWS, "§8Köpfe", &self.main_items())?;
        self.sessions.insert(
            player.id.clone(),
            Session {
                menu,
                view: View::Main,
            },
        );
        Ok(())
    }

    fn open_list(&mut self, player: &Player, title: &str, ids: Vec<u32>) -> Result<(), String> {
        let title = format!("§8{title} §7({})", ids.len());
        let menu = menus::open(&player.id, ROWS, &title, &self.list_items(&ids, 0))?;
        self.sessions.insert(
            player.id.clone(),
            Session {
                menu,
                view: View::List { ids, page: 0 },
            },
        );
        Ok(())
    }

    /// Heads `query` finds, listed; an error when none.
    fn search(&mut self, player: &Player, query: &str) -> Result<(), String> {
        let ids = self.db.search(query);
        if ids.is_empty() {
            return Err(format!("Keine Köpfe für „{query}“ gefunden."));
        }
        self.open_list(player, &format!("Suche: {query}"), ids)
    }

    /// 45 heads picked at random.
    fn random(&mut self, player: &Player) -> Result<(), String> {
        let len = self.db.len() as u32;
        let bytes = random::bytes(PAGE as u32 * 4);
        let ids = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_le_bytes(*b) % len.max(1))
            .collect();
        self.open_list(player, "Zufällig", ids)
    }

    fn give(player: &Player, item: &str, count: u32, what: &str) {
        command::run_quiet(&[format!("give {} {item} {count}", player.name)]);
        players::action_bar(&player.id, &format!("§aErhalten: §f{what}"));
    }

    fn give_head(&self, player: &Player, i: u32, count: u32) {
        if let Some(h) = self.db.head(i) {
            Self::give(player, &self.head_item(&h, false), count, h.name);
        }
    }

    fn give_player_head(player: &Player, name: &str) -> Result<(), String> {
        let ok = (1..=16).contains(&name.len())
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !ok {
            return Err(format!("{name} ist kein Spielername."));
        }
        let head = format!("minecraft:player_head[profile={{name:\"{name}\"}}]");
        Self::give(player, &head, 1, &format!("Kopf von {name}"));
        Ok(())
    }

    fn prompt_search(player: &Player) {
        players::tell(
            &player.id,
            "Tippe §e/heads <Suchwort>§r, z. B. §e/heads apfel",
        );
    }

    fn click(&mut self, player: &Player, click: &MenuInput) -> Result<(), String> {
        enum Act {
            Nothing,
            Random,
            Category(usize),
            Main,
            Page(usize),
            Give(u32),
        }
        let Some(session) = self.sessions.get(&player.id) else {
            return Ok(());
        };
        if session.menu != click.menu || click.slot < 0 || click.slot >= i32::from(ROWS) * 9 {
            return Ok(());
        }
        let menu = session.menu;
        match click.slot {
            SEARCH => {
                menus::close(&player.id, menu);
                Self::prompt_search(player);
                return Ok(());
            }
            OWN_HEAD => return Self::give_player_head(player, &player.name),
            CLOSE => {
                menus::close(&player.id, menu);
                return Ok(());
            }
            _ => {}
        }
        let act = match &session.view {
            View::Main if click.slot == RANDOM => Act::Random,
            View::Main => CATEGORY_SLOTS
                .iter()
                .position(|&s| s == click.slot)
                .map_or(Act::Nothing, Act::Category),
            View::List { ids, page } => {
                let pages = ids.len().div_ceil(PAGE).max(1);
                match click.slot {
                    BACK => Act::Main,
                    NEXT if *page + 1 < pages => Act::Page(*page + 1),
                    PREVIOUS if *page > 0 => Act::Page(*page - 1),
                    slot if slot < PAGE as i32 => ids
                        .get(*page * PAGE + slot as usize)
                        .map_or(Act::Nothing, |&i| Act::Give(i)),
                    _ => Act::Nothing,
                }
            }
        };
        match act {
            Act::Nothing => Ok(()),
            Act::Random => self.random(player),
            Act::Main => self.open_main(player),
            Act::Category(c) => {
                let ids = self.db.by_category.get(c).cloned().unwrap_or_default();
                let title = self.category_label(c).to_owned();
                self.open_list(player, &title, ids)
            }
            Act::Page(next) => {
                if let Some(Session {
                    view: View::List { page, .. },
                    ..
                }) = self.sessions.get_mut(&player.id)
                {
                    *page = next;
                }
                if let Some(Session {
                    view: View::List { ids, .. },
                    ..
                }) = self.sessions.get(&player.id)
                {
                    menus::set_items(&player.id, menu, &self.list_items(ids, next));
                }
                Ok(())
            }
            Act::Give(i) => {
                let count = if click.shift { 64 } else { 1 };
                self.give_head(player, i, count);
                Ok(())
            }
        }
    }
}

impl ServerMod for Heads {
    fn init() -> Result<(Self, Registration), String> {
        let db = Db::read(db::CATALOG).ok_or("the head catalog doesn't read")?;
        let heads = Heads {
            db,
            sessions: BTreeMap::new(),
        };
        let registration = Registration::new()
            .command("heads")
            .command("hdb")
            .on_joins();
        Ok((heads, registration))
    }

    fn player_left(&mut self, player: &Player) {
        self.sessions.remove(&player.id);
    }

    fn command(
        &mut self,
        player: &Player,
        _name: &str,
        args: &Args,
    ) -> Result<Option<String>, String> {
        let text = args.string("args").unwrap_or("").trim().to_owned();
        let mut words = text.split_whitespace();
        match words.next() {
            None => self.open_main(player)?,
            Some(w) if w.eq_ignore_ascii_case("spieler") || w.eq_ignore_ascii_case("player") => {
                let name = words.next().ok_or("/heads spieler <Name>")?;
                Self::give_player_head(player, name)?;
            }
            Some(_) => self.search(player, &text)?,
        }
        Ok(None)
    }

    fn menu_click(&mut self, player: &Player, click: &MenuInput) {
        if let Err(e) = self.click(player, click) {
            players::tell(&player.id, &format!("§c{e}"));
        }
    }

    fn menu_closed(&mut self, player: &Player, menu: u32) {
        if self
            .sessions
            .get(&player.id)
            .is_some_and(|s| s.menu == menu)
        {
            self.sessions.remove(&player.id);
        }
    }
}

horizon_mod_sdk::export_server_mod!(Heads);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_quoted() {
        assert_eq!(quoted(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }

    #[test]
    fn items_carry_name_and_lore() {
        assert_eq!(
            item("minecraft:paper", "", "Seite 1/2", &[("x", "gray")]),
            "minecraft:paper[item_name=\"Seite 1/2\",lore=[{text:\"x\",color:\"gray\",italic:false}]]"
        );
        assert!(
            item("minecraft:player_head", "profile={name:\"A\"}", "A", &[])
                .starts_with("minecraft:player_head[profile={name:\"A\"},item_name=\"A\"]")
        );
    }

    #[test]
    fn every_category_has_a_name_and_a_slot() {
        let db = Db::read(db::CATALOG).expect("catalog");
        assert_eq!(db.categories.len(), CATEGORY_SLOTS.len());
        for id in &db.categories {
            assert_ne!(category_name(id), id.as_str(), "{id} has a German name");
        }
    }
}
