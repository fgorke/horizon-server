//! Horizon Server plugin (`docs/plugins.md`): every player's tab list gets the header and footer of
//! `data/messages`, markup with `\n` for a new line, `{player}` and `{online}` filled in. With a
//! `data/pack.zip`, the plugin serves it at `/pack.zip` on port 8101 (or the number in `data/port`),
//! for `resource-pack` in `server.properties`: a pack's font is how a logo gets into the header.
//!
//! Beside each name the list shows the player's deaths (vanilla's statistic
//! `minecraft:deaths`), as the scoreboard objective [`OBJECTIVE`] in the list's slot; `off` in
//! `data/deaths` leaves the slot alone.
use horizon_mod_sdk::{
    messages::Messages,
    server::{
        Player, Registration, Request, Response, ServerMod, command, network, players, statistics,
    },
    storage,
};
use std::collections::BTreeMap;

const PORT: u16 = 8101;
const PACK: &str = "pack.zip";
/// How often every player is sent their lists again, in ticks.
const RESEND_TICKS: u64 = 100;
/// The lists sent each tick: `set-tab-list` counts toward a call's 8 messages.
const PER_TICK: usize = 8;
/// The scoreboard objective the list shows beside the names.
const OBJECTIVE: &str = "deaths";
/// How often the deaths are read again, in ticks.
const DEATHS_TICKS: u64 = 40;

/// A player's deaths in their statistics' JSON (`statistics.of`).
fn deaths_in(stats: &str) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_str(stats).ok()?;
    Some(
        v.get("minecraft:custom")
            .and_then(|c| c.get("minecraft:deaths"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
    )
}

/// The commands that make the objective and put it beside the names in the list.
fn deaths_setup() -> Vec<String> {
    vec![
        format!("scoreboard objectives add {OBJECTIVE} dummy {{\"text\":\"Deaths\"}}"),
        format!("scoreboard objectives setdisplay list {OBJECTIVE}"),
    ]
}
/// The messages and their defaults.
const MESSAGES: &[(&str, &str)] = &[
    (
        "header",
        "\\n<gradient:#7DD3FC:#2563EB>&lServer</gradient>\\n",
    ),
    ("footer", "\\n&7Online: &f{online}\\n"),
];

/// A message's text with its `\n`s as new lines.
fn lines(message: &str) -> String {
    message.replace("\\n", "\n")
}

struct TabList {
    messages: Messages,
    pack: Option<Vec<u8>>,
    /// The ids of the players still to send to.
    pending: Vec<String>,
    /// Someone joined or left: everyone's `{online}` changed.
    stale: bool,
    /// The list shows deaths beside the names.
    deaths: bool,
    /// The deaths last set, by player name.
    shown: BTreeMap<String, i64>,
}

/// Whether the list shows deaths: unless `data/deaths` says `off`.
fn deaths_on() -> bool {
    storage::get("deaths").is_none_or(|b| String::from_utf8_lossy(&b).trim() != "off")
}

impl TabList {
    /// The deaths of the players online that changed since they were last set.
    fn update_deaths(&mut self, online: &[Player]) {
        let lines: Vec<String> = online
            .iter()
            .filter_map(|p| {
                let deaths = statistics::of(&p.id)
                    .and_then(|s| deaths_in(&s))
                    .unwrap_or(0);
                (self.shown.get(&p.name) != Some(&deaths)).then(|| {
                    self.shown.insert(p.name.clone(), deaths);
                    format!("scoreboard players set {} {OBJECTIVE} {deaths}", p.name)
                })
            })
            .collect();
        if !lines.is_empty() {
            command::run_quiet(&lines);
        }
    }

    fn send(&self, player: &Player, online: usize) {
        let values: [(&str, &dyn std::fmt::Display); 2] =
            [("player", &player.name), ("online", &online)];
        players::set_tab_list(
            &player.id,
            &lines(&self.messages.get("header", &values)),
            &lines(&self.messages.get("footer", &values)),
        );
    }
}

impl ServerMod for TabList {
    fn init() -> Result<(Self, Registration), String> {
        let pack = storage::get(PACK);
        // No pack, no port: nothing else is served.
        if pack.is_some() {
            let port = storage::get("port")
                .and_then(|b| String::from_utf8_lossy(&b).trim().parse().ok())
                .unwrap_or(PORT);
            network::http_listen(port)?;
            horizon_mod_sdk::info(&format!("Resource pack at port {port}, /{PACK}"));
        }
        if deaths_on() {
            command::run_quiet(&deaths_setup());
        }
        Ok((
            TabList {
                messages: Messages::load(MESSAGES),
                pack,
                pending: Vec::new(),
                stale: true,
                deaths: deaths_on(),
                shown: BTreeMap::new(),
            },
            Registration::new().on_tick().on_joins(),
        ))
    }

    fn tick(&mut self, tick: u64) {
        let online = players::online();
        if self.deaths && tick.is_multiple_of(DEATHS_TICKS) {
            self.update_deaths(&online);
        }
        if self.stale || tick.is_multiple_of(RESEND_TICKS) {
            self.stale = false;
            self.pending = online.iter().map(|p| p.id.clone()).collect();
        }
        let now = self.pending.len().min(PER_TICK);
        let due: Vec<String> = self.pending.drain(..now).collect();
        for player in online.iter().filter(|p| due.contains(&p.id)) {
            self.send(player, online.len());
        }
    }

    fn player_joined(&mut self, _player: &Player) {
        self.stale = true;
    }

    fn player_left(&mut self, _player: &Player) {
        self.stale = true;
    }

    fn http(&mut self, request: &Request) -> Response {
        match (&self.pack, request.path.split('?').next()) {
            (Some(pack), Some("/pack.zip")) if request.method == "GET" => {
                Response::new(200, "application/zip", pack.clone())
            }
            _ => Response::not_found(),
        }
    }
}

horizon_mod_sdk::export_server_mod!(TabList);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deaths_come_from_the_custom_statistics() {
        let stats = r#"{"minecraft:custom":{"minecraft:deaths":3,"minecraft:jump":9}}"#;
        assert_eq!(deaths_in(stats), Some(3));
        assert_eq!(deaths_in("{}"), Some(0));
        assert_eq!(deaths_in("not json"), None);
    }

    #[test]
    fn a_message_breaks_at_its_new_lines() {
        assert_eq!(lines("a\\n\\nb"), "a\n\nb");
        assert_eq!(lines("plain"), "plain");
    }
}
