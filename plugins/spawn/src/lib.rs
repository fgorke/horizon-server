//! Horizon Server plugin (`docs/plugins.md`): `/setspawn` (operators) sets the spawn spot where the
//! player stands, looking where they look; `/spawn` (everyone) takes a player there after
//! [`COUNTDOWN`] ticks standing still, the seconds left in the action bar. Moving cancels it,
//! looking around doesn't. The spot is kept in `storage` key `spawn`, as
//! `<dimension> <x> <y> <z> <yaw> <pitch>`. Its messages are [`MESSAGES`], worded in
//! `data/messages`.
use horizon_mod_sdk::{
    messages::Messages,
    server::{Args, Player, Registration, ServerMod, players},
    storage,
};
use std::collections::BTreeMap;

/// /spawn's wait, in ticks.
const COUNTDOWN: u32 = 100;
/// How far, in blocks along any axis, a player may drift during the wait.
const STILL: f64 = 0.1;
/// The permission level /setspawn needs: an operator's.
const OPERATOR: u8 = 2;
const KEY: &str = "spawn";
/// The messages and their defaults.
const MESSAGES: &[(&str, &str)] = &[
    ("countdown", "§eTeleport in {seconds} s, don't move"),
    ("cancelled", "§cTeleport cancelled: you moved."),
    ("teleported", "§aTeleported."),
    ("spawn_set", "Spawn set: {x} {y} {z}"),
    ("not_operator", "You need to be an operator for that."),
    (
        "no_spawn",
        "No spawn is set yet: an operator sets it with /setspawn",
    ),
    ("save_failed", "Could not save the spawn: {error}"),
];

/// A spot in a dimension, with the look to arrive with.
#[derive(Clone, Debug, PartialEq)]
struct Spot {
    dimension: String,
    at: [f64; 3],
    yaw: f32,
    pitch: f32,
}

impl Spot {
    fn parse(text: &str) -> Option<Spot> {
        let mut words = text.split_whitespace();
        let dimension = words.next()?.to_owned();
        let mut number = || words.next()?.parse::<f64>().ok();
        let at = [number()?, number()?, number()?];
        let (yaw, pitch) = (number()? as f32, number()? as f32);
        Some(Spot {
            dimension,
            at,
            yaw,
            pitch,
        })
    }

    fn line(&self) -> String {
        let [x, y, z] = self.at;
        format!("{} {x} {y} {z} {} {}", self.dimension, self.yaw, self.pitch)
    }
}

/// A player waiting for /spawn: the ticks left and where they stood when they asked.
struct Wait {
    left: u32,
    from: [f64; 3],
}

struct Spawn {
    spot: Option<Spot>,
    /// By player id.
    waiting: BTreeMap<String, Wait>,
    messages: Messages,
}

fn position(p: &Player) -> [f64; 3] {
    [p.x, p.y, p.z]
}

impl Spawn {
    fn set_spawn(&mut self, player: &Player) -> Result<Option<String>, String> {
        if players::permission(&player.id).unwrap_or(0) < OPERATOR {
            return Err(self.messages.get("not_operator", &[]));
        }
        let (yaw, pitch) = players::rotation(&player.id).unwrap_or((0.0, 0.0));
        let spot = Spot {
            dimension: player.dimension.clone(),
            at: position(player),
            yaw,
            pitch,
        };
        storage::set(KEY, spot.line().as_bytes())
            .map_err(|e| self.messages.get("save_failed", &[("error", &e)]))?;
        self.spot = Some(spot);
        let [x, y, z] = position(player).map(|c| format!("{c:.1}"));
        let set = self
            .messages
            .get("spawn_set", &[("x", &x), ("y", &y), ("z", &z)]);
        Ok(Some(set))
    }

    /// One tick of every wait: cancelled by a move, the seconds left each second, then the
    /// teleport.
    fn tick_waits(&mut self) {
        let online = players::online();
        let spot = self.spot.clone();
        let messages = &self.messages;
        self.waiting.retain(|id, wait| {
            let Some(p) = online.iter().find(|p| p.id == *id) else {
                return false;
            };
            let at = position(p);
            if (0..3).any(|k| (at[k] - wait.from[k]).abs() > STILL) {
                players::action_bar(id, &messages.get("cancelled", &[]));
                return false;
            }
            if wait.left > 0 {
                if wait.left % 20 == 0 {
                    let line = messages.get("countdown", &[("seconds", &(wait.left / 20))]);
                    players::action_bar(id, &line);
                }
                wait.left -= 1;
                return true;
            }
            if let Some(s) = &spot {
                let [x, y, z] = s.at;
                if players::teleport(id, &s.dimension, x, y, z, s.yaw, s.pitch) {
                    players::action_bar(id, &messages.get("teleported", &[]));
                }
            }
            false
        });
    }
}

impl ServerMod for Spawn {
    fn init() -> Result<(Self, Registration), String> {
        let spot = storage::get(KEY).and_then(|b| Spot::parse(&String::from_utf8_lossy(&b)));
        let hooks = Registration::new()
            .command_with("spawn", [])
            .command_with("setspawn", [])
            .on_tick();
        Ok((
            Spawn {
                spot,
                waiting: BTreeMap::new(),
                messages: Messages::load(MESSAGES),
            },
            hooks,
        ))
    }

    fn tick(&mut self, _tick: u64) {
        if !self.waiting.is_empty() {
            self.tick_waits();
        }
    }

    fn command(
        &mut self,
        player: &Player,
        name: &str,
        _args: &Args,
    ) -> Result<Option<String>, String> {
        match name {
            "setspawn" => self.set_spawn(player),
            _ if self.spot.is_none() => Err(self.messages.get("no_spawn", &[])),
            _ => {
                let wait = Wait {
                    left: COUNTDOWN,
                    from: position(player),
                };
                self.waiting.insert(player.id.clone(), wait);
                Ok(None)
            }
        }
    }
}

horizon_mod_sdk::export_server_mod!(Spawn);
