//! Horizon Server plugin (`docs/plugins.md`): every player's tab list gets the header and footer of
//! `data/messages`, markup with `\n` for a new line, `{player}` and `{online}` filled in. With a
//! `data/pack.zip`, the plugin serves it at `/pack.zip` on port 8101 (or the number in `data/port`),
//! for `resource-pack` in `server.properties`: a pack's font is how a logo gets into the header.
use horizon_mod_sdk::{
    messages::Messages,
    server::{Player, Registration, Request, Response, ServerMod, network, players},
    storage,
};

const PORT: u16 = 8101;
const PACK: &str = "pack.zip";
/// How often every player is sent their lists again, in ticks.
const RESEND_TICKS: u64 = 100;
/// The lists sent each tick: `set-tab-list` counts toward a call's 8 messages.
const PER_TICK: usize = 8;
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
}

impl TabList {
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
        Ok((
            TabList {
                messages: Messages::load(MESSAGES),
                pack,
                pending: Vec::new(),
                stale: true,
            },
            Registration::new().on_tick().on_joins(),
        ))
    }

    fn tick(&mut self, tick: u64) {
        let online = players::online();
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
    fn a_message_breaks_at_its_new_lines() {
        assert_eq!(lines("a\\n\\nb"), "a\n\nb");
        assert_eq!(lines("plain"), "plain");
    }
}
