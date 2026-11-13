//! Horizon Server plugin (`docs/plugins.md`): statuses players pick for themselves, shown before
//! their name in the tab list and in chat. The server owner lists them in `data/statuses`, one
//! `name = prefix` a line, the prefix in `&` colour codes (`&6`, `&l`, `&#RRGGBB`); `/status
//! reload` (operators, level [`RELOAD_LEVEL`]) reads the file again. `/status <name>` sets one,
//! `/status off` takes it away, `/status list` lists them; the chat box completes the names.
//!
//! Each status is a scoreboard team `status_<name>` whose prefix is the status, so the game shows
//! it wherever vanilla shows a team's prefix. The teams this plugin made are kept in `data/teams`,
//! so a status taken out of the file loses its team on the next reload.
use horizon_mod_sdk::{
    messages::Messages,
    server::{Args, Argument, Player, Registration, ServerMod, command, players},
    storage,
};
use serde_json::{Value, json};

const STATUSES_KEY: &str = "statuses";
const TEAMS_KEY: &str = "teams";
const TEAM_PREFIX: &str = "status_";
/// The permission level `/status reload` needs.
const RELOAD_LEVEL: u8 = 2;
/// The words `/status` takes besides the statuses' names.
const OFF: &str = "off";
const LIST: &str = "list";
const RELOAD: &str = "reload";
/// The most characters of a status's name and of its prefix as written.
const NAME_CHARS: usize = 32;
const PREFIX_CHARS: usize = 128;

/// What `data/statuses` holds the first time.
const DEFAULT_STATUSES: &str = "\
# One status a line: name = prefix. Players pick one with /status <name>.
# The prefix is shown before the name in the tab list and in chat, a space after it.
# Colours: &0-&f, &l bold, &o italic, &n underlined, &m struck, &r reset, &#RRGGBB.
# Names: letters, digits and _ - . + (not off, list or reload). /status reload after a change.
afk = &7[AFK]
building = &6[Building]
farming = &a[Farming]
exploring = &b[Exploring]
";

/// The messages and their defaults, worded in `data/messages`.
const MESSAGES: &[(&str, &str)] = &[
    ("set", "&7Your status: {status}"),
    ("off", "&7Your status is off."),
    (
        "unknown",
        "There is no status {name}: /status list shows them.",
    ),
    (
        "list",
        "&7Statuses: {statuses} &8(/status <name>, /status off)",
    ),
    (
        "none",
        "&7There are no statuses: an operator lists them in plugins/status/data/statuses.",
    ),
    ("reload_denied", "Only operators may reload the statuses."),
    ("reloaded", "&7{count} statuses read."),
];

/// The vanilla colour names of the legacy codes `0` to `f`.
const COLOURS: [&str; 16] = [
    "black",
    "dark_blue",
    "dark_green",
    "dark_aqua",
    "dark_red",
    "dark_purple",
    "gold",
    "gray",
    "dark_gray",
    "blue",
    "green",
    "aqua",
    "red",
    "light_purple",
    "yellow",
    "white",
];

/// A status: its name and its prefix as written.
#[derive(Clone, Debug, PartialEq)]
struct Status {
    name: String,
    prefix: String,
}

/// A name `/status` can take: a brigadier word, not one of its own words.
fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= NAME_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+'))
        && ![OFF, LIST, RELOAD].contains(&name)
}

/// The statuses of `data/statuses`, in order; lines that aren't one are skipped and named.
fn parse_statuses(text: &str) -> (Vec<Status>, Vec<String>) {
    let mut statuses: Vec<Status> = Vec::new();
    let mut skipped = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, prefix)) = line.split_once('=') else {
            skipped.push(line.to_owned());
            continue;
        };
        let name = name.trim().to_lowercase();
        let prefix = prefix.trim();
        let prefix = prefix
            .strip_prefix('"')
            .and_then(|p| p.strip_suffix('"'))
            .unwrap_or(prefix);
        if !is_name(&name)
            || prefix.chars().count() > PREFIX_CHARS
            || statuses.iter().any(|s| s.name == name)
        {
            skipped.push(line.to_owned());
            continue;
        }
        statuses.push(Status {
            name,
            prefix: prefix.to_owned(),
        });
    }
    (statuses, skipped)
}

/// One run of a prefix's characters in one style.
#[derive(Default, Clone)]
struct Run {
    text: String,
    color: Option<String>,
    bold: bool,
    italic: bool,
    underlined: bool,
    strikethrough: bool,
    obfuscated: bool,
}

impl Run {
    fn styled_like(&self) -> Run {
        Run {
            text: String::new(),
            ..self.clone()
        }
    }

    fn json(&self) -> Value {
        let mut o = serde_json::Map::new();
        o.insert("text".into(), self.text.clone().into());
        if let Some(c) = &self.color {
            o.insert("color".into(), c.clone().into());
        }
        for (key, on) in [
            ("bold", self.bold),
            ("italic", self.italic),
            ("underlined", self.underlined),
            ("strikethrough", self.strikethrough),
            ("obfuscated", self.obfuscated),
        ] {
            if on {
                o.insert(key.into(), true.into());
            }
        }
        Value::Object(o)
    }
}

/// A prefix's `&` codes as a text component, a space after it: a colour starts a fresh style, as
/// vanilla's codes do; anything that isn't a code stays text.
fn prefix_component(prefix: &str) -> Value {
    let mut runs: Vec<Run> = vec![Run::default()];
    let mut chars = prefix.chars().peekable();
    while let Some(c) = chars.next() {
        let current = runs.last().cloned().unwrap_or_default();
        if c == '&' || c == '§' {
            let rest: String = chars.clone().take(7).collect();
            if let Some(hex) = rest
                .strip_prefix('#')
                .filter(|h| h.len() == 6 && h.chars().all(|c| c.is_ascii_hexdigit()))
            {
                runs.push(Run {
                    color: Some(format!("#{}", hex.to_uppercase())),
                    ..Run::default()
                });
                chars.nth(6);
                continue;
            }
            let code = chars.peek().map(|k| k.to_ascii_lowercase());
            let next = match code {
                Some(k @ ('0'..='9' | 'a'..='f')) => Some(Run {
                    color: Some(COLOURS[k.to_digit(16).unwrap_or(15) as usize].to_owned()),
                    ..Run::default()
                }),
                Some('l') => Some(Run {
                    bold: true,
                    ..current.styled_like()
                }),
                Some('o') => Some(Run {
                    italic: true,
                    ..current.styled_like()
                }),
                Some('n') => Some(Run {
                    underlined: true,
                    ..current.styled_like()
                }),
                Some('m') => Some(Run {
                    strikethrough: true,
                    ..current.styled_like()
                }),
                Some('k') => Some(Run {
                    obfuscated: true,
                    ..current.styled_like()
                }),
                Some('r') => Some(Run::default()),
                _ => None,
            };
            if let Some(run) = next {
                chars.next();
                runs.push(run);
                continue;
            }
        }
        if let Some(run) = runs.last_mut() {
            run.text.push(c);
        }
    }
    let mut extra: Vec<Value> = runs
        .iter()
        .filter(|r| !r.text.is_empty())
        .map(Run::json)
        .collect();
    extra.push(json!({"text": " "}));
    json!({"text": "", "extra": extra})
}

/// The team of status `name`.
fn team(name: &str) -> String {
    format!("{TEAM_PREFIX}{name}")
}

struct StatusPlugin {
    statuses: Vec<Status>,
    messages: Messages,
}

impl StatusPlugin {
    /// `data/statuses`, written with the defaults the first time.
    fn read_statuses() -> Vec<Status> {
        let text = match storage::get(STATUSES_KEY) {
            Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            None => {
                if let Err(e) = storage::set(STATUSES_KEY, DEFAULT_STATUSES.as_bytes()) {
                    horizon_mod_sdk::warn(&format!("could not write the default statuses: {e}"));
                }
                DEFAULT_STATUSES.to_owned()
            }
        };
        let (statuses, skipped) = parse_statuses(&text);
        for line in skipped {
            horizon_mod_sdk::warn(&format!("data/statuses: skipped \"{line}\""));
        }
        statuses
    }

    /// The teams the statuses need, with their prefixes, and the old statuses' teams gone; the
    /// chat box's completions follow.
    fn apply(&self) {
        let before: Vec<String> = storage::get(TEAMS_KEY)
            .map(|b| {
                String::from_utf8_lossy(&b)
                    .lines()
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let now: Vec<String> = self.statuses.iter().map(|s| team(&s.name)).collect();
        let mut lines: Vec<String> = before
            .iter()
            .filter(|t| t.starts_with(TEAM_PREFIX) && !now.contains(t))
            .map(|t| format!("team remove {t}"))
            .collect();
        for s in &self.statuses {
            let t = team(&s.name);
            lines.push(format!("team add {t}"));
            lines.push(format!(
                "team modify {t} prefix {}",
                prefix_component(&s.prefix)
            ));
        }
        if !lines.is_empty() {
            command::run_quiet(&lines);
        }
        if let Err(e) = storage::set(TEAMS_KEY, now.join("\n").as_bytes()) {
            horizon_mod_sdk::warn(&format!("could not save the teams: {e}"));
        }
        let mut words: Vec<String> = self.statuses.iter().map(|s| s.name.clone()).collect();
        words.extend([OFF, LIST, RELOAD].map(str::to_owned));
        players::set_suggestions("status", "status", &words);
    }

    fn list(&self) -> String {
        if self.statuses.is_empty() {
            return self.messages.get("none", &[]);
        }
        let all = self
            .statuses
            .iter()
            .map(|s| format!("{}&r &7({})", s.prefix, s.name))
            .collect::<Vec<_>>()
            .join("&8, ");
        let args: [(&str, &dyn std::fmt::Display); 1] = [("statuses", &all)];
        self.messages.get("list", &args)
    }
}

impl ServerMod for StatusPlugin {
    fn init() -> Result<(Self, Registration), String> {
        let plugin = StatusPlugin {
            statuses: Self::read_statuses(),
            messages: Messages::load(MESSAGES),
        };
        plugin.apply();
        let registration =
            Registration::new().command_with("status", [Argument::word("status").optional()]);
        Ok((plugin, registration))
    }

    fn command(
        &mut self,
        player: &Player,
        _name: &str,
        args: &Args,
    ) -> Result<Option<String>, String> {
        let word = args.string("status").map(str::to_lowercase);
        match word.as_deref() {
            None | Some(LIST) => Ok(Some(self.list())),
            Some(OFF) => {
                command::run_quiet(&[format!("team leave {}", player.name)]);
                Ok(Some(self.messages.get("off", &[])))
            }
            Some(RELOAD) => {
                if players::permission(&player.id).unwrap_or(0) < RELOAD_LEVEL {
                    return Err(self.messages.get("reload_denied", &[]));
                }
                self.statuses = Self::read_statuses();
                self.apply();
                let count = self.statuses.len();
                let args: [(&str, &dyn std::fmt::Display); 1] = [("count", &count)];
                Ok(Some(self.messages.get("reloaded", &args)))
            }
            Some(name) => {
                let Some(status) = self.statuses.iter().find(|s| s.name == name) else {
                    let args: [(&str, &dyn std::fmt::Display); 1] = [("name", &name)];
                    return Err(self.messages.get("unknown", &args));
                };
                command::run_quiet(&[format!("team join {} {}", team(&status.name), player.name)]);
                let args: [(&str, &dyn std::fmt::Display); 1] = [("status", &status.prefix)];
                Ok(Some(self.messages.get("set", &args)))
            }
        }
    }
}

horizon_mod_sdk::export_server_mod!(StatusPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_are_read_line_by_line() {
        let (statuses, skipped) = parse_statuses(
            "# comment\n\nAFK = &7[AFK]\nbuild = \"&6[Bau] \"\noff = &c[x]\nno equals\nafk = again\n",
        );
        assert_eq!(
            statuses,
            vec![
                Status {
                    name: "afk".into(),
                    prefix: "&7[AFK]".into()
                },
                Status {
                    name: "build".into(),
                    prefix: "&6[Bau] ".into()
                },
            ]
        );
        assert_eq!(skipped, ["off = &c[x]", "no equals", "afk = again"]);
        assert!(!is_name("has space") && !is_name("reload") && is_name("pvp-2"));
    }

    #[test]
    fn a_prefix_becomes_a_component_with_a_space() {
        assert_eq!(
            prefix_component("&7[&lAFK&7]"),
            json!({"text": "", "extra": [
                {"text": "[", "color": "gray"},
                {"text": "AFK", "color": "gray", "bold": true},
                {"text": "]", "color": "gray"},
                {"text": " "},
            ]})
        );
        assert_eq!(
            prefix_component("&#ff8800Bau & mehr"),
            json!({"text": "", "extra": [
                {"text": "Bau & mehr", "color": "#FF8800"},
                {"text": " "},
            ]})
        );
    }
}
