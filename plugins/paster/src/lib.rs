//! Horizon Server plugin (`docs/plugins.md`): the server side of the
//! [Litematica Server Paster](https://github.com/Fallen-Breath/litematica-server-paster) mod, which
//! pastes a schematic's block entity and entity NBT through `/setblock` and `/summon` lines longer
//! than chat allows.
//!
//! The mod's server side runs a line as the player who sent it. A plugin's commands run as the
//! console, so this one holds that back itself: only operators, only `/setblock` and `/summon`
//! (what the mod sends), and in the sender's dimension and at their position, as their own command
//! would be.
mod protocol;

use horizon_mod_sdk::{
    messages::Messages,
    server::{Player, Registration, ServerMod, channels, command, players},
    warn,
};
use protocol::{CHANNEL, Packet};
use std::collections::{BTreeMap, BTreeSet};

/// The permission level `/setblock` and `/summon` need: an operator's.
const OPERATOR: u8 = 2;
/// The commands a line may run, without a namespace.
const COMMANDS: [&str; 2] = ["setblock", "summon"];
/// UTF-16 units a long command may gather. Under what the host passes on (`execute ...` goes in
/// front of it): larger than any block entity that fits a chunk's saved form in practice.
const MAX_LINE: usize = 1_000_000;
const MESSAGES: &[(&str, &str)] = &[
    (
        "not_operator",
        "§cLitematica paste refused: you need to be an operator.",
    ),
    (
        "refused_command",
        "§cLitematica paste refused: only /setblock and /summon are allowed.",
    ),
    (
        "too_long",
        "§cLitematica paste refused: a line is too long.",
    ),
];

/// Why a line isn't run.
#[derive(Debug, PartialEq)]
enum Refusal {
    Command,
}

/// The console line for `line` from a player at `at` in `dimension`.
fn console_line(line: &str, dimension: &str, at: [f64; 3]) -> Result<String, Refusal> {
    let line = line.strip_prefix('/').unwrap_or(line);
    let name = line.split(' ').next().unwrap_or_default();
    let name = name.strip_prefix("minecraft:").unwrap_or(name);
    if !COMMANDS.contains(&name) || line.contains(char::is_control) {
        return Err(Refusal::Command);
    }
    let [x, y, z] = at;
    Ok(format!(
        "execute in {dimension} positioned {x} {y} {z} run {line}"
    ))
}

struct Paster {
    /// The long command each player is sending, by player id.
    long: BTreeMap<String, Vec<u16>>,
    /// Players already told why they are refused: a paste sends thousands of lines.
    told: BTreeSet<String>,
    messages: Messages,
}

impl Paster {
    fn operator(player: &Player) -> bool {
        players::permission(&player.id).unwrap_or(0) >= OPERATOR
    }

    /// Tells a refused player once, until they leave or a line of theirs goes through.
    fn refuse(&mut self, player: &Player, key: &str) {
        if self.told.insert(player.id.clone()) {
            warn(&format!("{}: {key}", player.name));
            players::tell(&player.id, &self.messages.get(key, &[]));
        }
    }

    fn run(&mut self, player: &Player, units: &[u16]) {
        if units.is_empty() {
            return;
        }
        if !Self::operator(player) {
            self.refuse(player, "not_operator");
            return;
        }
        let line = String::from_utf16_lossy(units);
        match console_line(&line, &player.dimension, [player.x, player.y, player.z]) {
            Ok(line) => {
                self.told.remove(&player.id);
                command::run_quiet(&[line]);
            }
            Err(Refusal::Command) => self.refuse(player, "refused_command"),
        }
    }
}

impl ServerMod for Paster {
    fn init() -> Result<(Self, Registration), String> {
        channels::listen(CHANNEL)?;
        let paster = Paster {
            long: BTreeMap::new(),
            told: BTreeSet::new(),
            messages: Messages::load(MESSAGES),
        };
        Ok((paster, Registration::new().on_joins()))
    }

    fn player_left(&mut self, player: &Player) {
        self.long.remove(&player.id);
        self.told.remove(&player.id);
    }

    fn payload(&mut self, player: &Player, channel: &str, data: &[u8]) {
        if channel != CHANNEL {
            return;
        }
        let Some(packet) = protocol::decode(data) else {
            warn(&format!("{}: an unreadable paste packet", player.name));
            return;
        };
        match packet {
            Packet::Hi => {
                let version = env!("CARGO_PKG_VERSION");
                channels::send(&player.id, CHANNEL, &protocol::hi(version));
                channels::send(&player.id, CHANNEL, &protocol::accept_packets());
            }
            Packet::Chat(units) => self.run(player, &units),
            // Nobody but an operator gets to fill memory.
            Packet::LongStart => {
                if Self::operator(player) {
                    self.long.insert(player.id.clone(), Vec::new());
                } else {
                    self.refuse(player, "not_operator");
                }
            }
            Packet::LongContent(units) => {
                let Some(line) = self.long.get_mut(&player.id) else {
                    return;
                };
                if line.len() + units.len() > MAX_LINE {
                    self.long.remove(&player.id);
                    self.refuse(player, "too_long");
                } else {
                    line.extend_from_slice(&units);
                }
            }
            Packet::LongEnd => {
                if let Some(line) = self.long.remove(&player.id) {
                    self.run(player, &line);
                }
            }
            Packet::Unknown => {}
        }
    }
}

horizon_mod_sdk::export_server_mod!(Paster);

#[cfg(test)]
mod tests {
    use super::*;

    const AT: [f64; 3] = [0.5, 64.0, -2.25];

    #[test]
    fn runs_in_the_players_dimension_and_place() {
        assert_eq!(
            console_line("/setblock 1 2 3 chest{Lock:\"x\"}", "minecraft:the_nether", AT),
            Ok(
                "execute in minecraft:the_nether positioned 0.5 64 -2.25 run setblock 1 2 3 chest{Lock:\"x\"}"
                    .to_owned()
            )
        );
        assert!(console_line("summon pig 1 2 3 {}", "minecraft:overworld", AT).is_ok());
        assert!(console_line("minecraft:setblock 1 2 3 stone", "minecraft:overworld", AT).is_ok());
    }

    #[test]
    fn nothing_else_runs_as_the_console() {
        for line in [
            "/op Alex",
            "/execute run op Alex",
            "/setblockx 1 2 3 stone",
            "/stop",
            "setblock 1 2 3 stone\nop Alex",
            "",
        ] {
            assert_eq!(
                console_line(line, "minecraft:overworld", AT),
                Err(Refusal::Command),
                "{line}"
            );
        }
    }
}
