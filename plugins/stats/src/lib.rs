//! Horizon Server plugin (`docs/plugins.md`): a read-only web page with every player's
//! statistics, live for the players online, on port 8100 (or the number in `storage` key
//! `port`). `GET /` is the page, `/api/info` the world and language, `/api/lang` the block,
//! item, entity and statistic names of the language in `storage` key `lang` (`en_us`), and
//! `/api/stats` the players. The page shows each player's time away from the keyboard too:
//! once still for [`AFK_AFTER`] seconds, every second counts, kept in `storage` key `afk`.
use horizon_mod_sdk::{
    server::{Registration, Request, Response, ServerMod, network, players, statistics},
    storage,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const PAGE: &str = include_str!("page.html");
const PORT: u16 = 8100;
/// Seconds without moving or looking around after which a player is away.
const AFK_AFTER: u64 = 180;
/// How often, in seconds, the AFK times are saved.
const SAVE_EVERY: u64 = 60;
const AFK_KEY: &str = "afk";
const NAME_KINDS: [&str; 4] = [
    "block.minecraft.",
    "item.minecraft.",
    "entity.minecraft.",
    "stat.minecraft.",
];

/// Where a player stood and looked, and for how many seconds.
struct Still {
    at: [f64; 3],
    look: (f32, f32),
    seconds: u64,
}

struct Stats {
    language: String,
    /// Seconds each player (by UUID) has been away, added up.
    afk: BTreeMap<String, u64>,
    still: BTreeMap<String, Still>,
    seconds: u64,
}

/// The seconds to add to a player's time away after one more second at `at` looking `look`:
/// the whole still stretch once it reaches [`AFK_AFTER`], one a second after.
fn afk_seconds(still: &mut Still, at: [f64; 3], look: (f32, f32)) -> u64 {
    if still.at != at || still.look != look {
        *still = Still {
            at,
            look,
            seconds: 0,
        };
        return 0;
    }
    still.seconds += 1;
    match still.seconds {
        n if n == AFK_AFTER => n,
        n if n > AFK_AFTER => 1,
        _ => 0,
    }
}

/// A language code as the game names its files: `en_us`.
fn is_language(code: &str) -> bool {
    !code.is_empty() && code.chars().all(|c| c.is_ascii_lowercase() || c == '_')
}

/// The names the page shows: `block.minecraft.x` and the like, not their sub-keys (`.desc`).
fn names_of(entries: Vec<(String, String)>) -> Value {
    let names: serde_json::Map<String, Value> = entries
        .into_iter()
        .filter(|(k, _)| {
            NAME_KINDS
                .iter()
                .any(|p| k.strip_prefix(p).is_some_and(|rest| !rest.contains('.')))
        })
        .map(|(k, v)| (k, Value::String(v)))
        .collect();
    Value::Object(names)
}

impl Stats {
    fn save_afk(&self) {
        let text = serde_json::to_string_pretty(&self.afk).unwrap_or_default();
        if let Err(e) = storage::set(AFK_KEY, text.as_bytes()) {
            horizon_mod_sdk::warn(&format!("could not save the AFK times: {e}"));
        }
    }

    /// Every player with statistics or online: UUID, name, online, statistics, time away.
    fn players(&self) -> Value {
        let names: BTreeMap<String, String> = statistics::names().into_iter().collect();
        let online: BTreeMap<String, String> = players::online()
            .into_iter()
            .map(|p| (p.id, p.name))
            .collect();
        let list = statistics::players()
            .into_iter()
            .map(|uuid| {
                let stats = statistics::of(&uuid)
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                    .unwrap_or_else(|| json!({}));
                let name = online
                    .get(&uuid)
                    .or_else(|| names.get(&uuid))
                    .cloned()
                    .unwrap_or_else(|| uuid.chars().take(8).collect());
                json!({
                    "uuid": uuid,
                    "name": name,
                    "online": online.contains_key(&uuid),
                    "stats": stats,
                    "afk": self.afk.get(&uuid).copied().unwrap_or(0),
                })
            })
            .collect();
        Value::Array(list)
    }

    fn json(value: &Value) -> Response {
        Response::new(200, "application/json", value.to_string())
    }
}

impl ServerMod for Stats {
    fn init() -> Result<(Self, Registration), String> {
        let port = storage::get("port")
            .and_then(|b| String::from_utf8_lossy(&b).trim().parse().ok())
            .unwrap_or(PORT);
        let language = storage::get("lang")
            .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
            .filter(|c| is_language(c))
            .unwrap_or_else(|| "en_us".to_owned());
        let afk = storage::get(AFK_KEY)
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        network::http_listen(port)?;
        horizon_mod_sdk::info(&format!("Stats page at port {port}"));
        let stats = Stats {
            language,
            afk,
            still: BTreeMap::new(),
            seconds: 0,
        };
        Ok((stats, Registration::new().on_tick().on_joins()))
    }

    /// Once a second: who stands still.
    fn tick(&mut self, tick: u64) {
        if !tick.is_multiple_of(20) {
            return;
        }
        self.seconds += 1;
        for p in players::online() {
            let at = [p.x, p.y, p.z];
            let look = players::rotation(&p.id).unwrap_or((0.0, 0.0));
            let still = self.still.entry(p.id.clone()).or_insert(Still {
                at,
                look,
                seconds: 0,
            });
            let away = afk_seconds(still, at, look);
            if away > 0 {
                *self.afk.entry(p.id).or_default() += away;
            }
        }
        if self.seconds.is_multiple_of(SAVE_EVERY) {
            self.save_afk();
        }
    }

    fn player_left(&mut self, player: &horizon_mod_sdk::server::Player) {
        self.still.remove(&player.id);
        self.save_afk();
    }

    fn http(&mut self, request: &Request) -> Response {
        if request.method != "GET" {
            return Response::new(405, "text/plain; charset=utf-8", "GET only");
        }
        match request.path.split('?').next().unwrap_or("") {
            "/" | "/index.html" => Response::new(200, "text/html; charset=utf-8", PAGE),
            "/api/info" => Self::json(&json!({
                "world": statistics::world_name(),
                "lang": self.language,
            })),
            "/api/lang" => {
                let prefixes = NAME_KINDS.map(str::to_owned).to_vec();
                let entries = statistics::translations(&self.language, &prefixes);
                Self::json(&names_of(entries.unwrap_or_default())).kept(3600)
            }
            "/api/stats" => Self::json(&self.players()),
            _ => Response::not_found(),
        }
    }
}

horizon_mod_sdk::export_server_mod!(Stats);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn away_counts_after_three_still_minutes() {
        let mut s = Still {
            at: [0.0; 3],
            look: (0.0, 0.0),
            seconds: 0,
        };
        let total: u64 = (0..AFK_AFTER + 5)
            .map(|_| afk_seconds(&mut s, [0.0; 3], (0.0, 0.0)))
            .sum();
        assert_eq!(total, AFK_AFTER + 5);
        assert_eq!(afk_seconds(&mut s, [0.0; 3], (1.0, 0.0)), 0);
        assert_eq!(s.seconds, 0);
    }

    #[test]
    fn only_names_without_sub_keys_are_shown() {
        let got = names_of(vec![
            ("block.minecraft.stone".into(), "Stone".into()),
            ("block.minecraft.bed.occupied".into(), "x".into()),
            ("gui.done".into(), "Done".into()),
            ("stat.minecraft.jump".into(), "Jumps".into()),
        ]);
        assert_eq!(
            got,
            json!({"block.minecraft.stone": "Stone", "stat.minecraft.jump": "Jumps"})
        );
        assert!(is_language("de_de") && !is_language("../x"));
    }
}
