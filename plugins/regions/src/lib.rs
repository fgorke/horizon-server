//! Horizon Server plugin (`docs/plugins.md`): protected regions. An operator marks two corners
//! with `/region pos1` and `pos2`, names the box with `/region define`, and turns each
//! protection on or off with `/region flag`: breaking, placing, using blocks (doors, trapdoors,
//! fence gates, buttons...), containers, pvp, damage and hurting mobs. The server checks the
//! regions before anything happens (`world.protect`); members of a region, and operators who
//! turned on `/region bypass`, aren't stopped. The regions are kept in `storage` key
//! `regions`, one line each (see [`Region::line`]); the messages are [`MESSAGES`], worded in
//! `data/messages`.
use horizon_mod_sdk::{
    messages::Messages,
    server::{Args, Argument, Player, ProtectedAction, Registration, ServerMod, players, world},
    storage,
};
use std::collections::{BTreeMap, BTreeSet};

/// The permission level the commands need: an operator's.
const OPERATOR: u8 = 2;
const KEY: &str = "regions";
/// `full` regions reach from the bottom of every dimension to its top.
const FULL_HEIGHT: (i32, i32) = (-2048, 4095);
/// The protections, by the name `/region flag` takes, in the order they are listed.
const FLAGS: [(&str, ProtectedAction); 7] = [
    ("break", ProtectedAction::BreakBlocks),
    ("place", ProtectedAction::PlaceBlocks),
    ("use", ProtectedAction::UseBlocks),
    ("containers", ProtectedAction::Containers),
    ("pvp", ProtectedAction::Pvp),
    ("damage", ProtectedAction::Damage),
    ("mobs", ProtectedAction::HurtMobs),
];
/// What a new region protects.
const DEFAULT_FLAGS: [&str; 4] = ["break", "place", "use", "containers"];
/// The messages and their defaults.
const MESSAGES: &[(&str, &str)] = &[
    ("denied", "§cYou can't do that here."),
    ("not_operator", "You need to be an operator for that."),
    (
        "usage",
        "/region pos1|pos2 · define <name> [full] · remove <name> · flag <name> <flag> <on|off> · member <name> add|remove <player> · info [name] · list · bypass",
    ),
    ("corner_set", "{corner} set: {x} {y} {z}"),
    (
        "no_selection",
        "Set pos1 and pos2 first (/region pos1, /region pos2).",
    ),
    (
        "other_dimension",
        "pos1 and pos2 are in different dimensions.",
    ),
    ("bad_name", "A region's name is letters, digits, - and _."),
    ("defined", "Region {region} defined: {flags}"),
    ("exists", "There is a region {region} already."),
    ("removed", "Region {region} removed."),
    ("unknown_region", "There is no region {region}."),
    ("unknown_flag", "Flags: {flags}"),
    ("flag_set", "{region}: {flag} {state}"),
    ("member_added", "{player} is a member of {region}."),
    (
        "member_removed",
        "{player} is no longer a member of {region}.",
    ),
    (
        "info",
        "{region} ({dimension} {pos1} to {pos2}) · protects: {flags} · members: {members}",
    ),
    ("none_here", "No region here."),
    ("list", "Regions: {regions}"),
    ("no_regions", "No regions yet."),
    ("nothing", "nothing"),
    ("nobody", "nobody"),
    ("on", "on"),
    ("off", "off"),
    ("bypass_on", "Bypass on: regions don't stop you."),
    ("bypass_off", "Bypass off: regions stop you again."),
    ("save_failed", "Could not save the regions: {error}"),
];

/// A protected box.
#[derive(Clone, Debug, PartialEq)]
struct Region {
    dimension: String,
    pos1: [i32; 3],
    pos2: [i32; 3],
    /// Flag names from [`FLAGS`].
    flags: BTreeSet<String>,
    /// Members, by player id, with the name they had.
    members: BTreeMap<String, String>,
}

impl Region {
    /// `<dimension> x1 y1 z1 x2 y2 z2 <flag,flag|-> <id=name,...|->`.
    fn line(&self, name: &str) -> String {
        let [a, b] = [self.pos1, self.pos2];
        let list = |items: Vec<String>| {
            if items.is_empty() {
                "-".to_owned()
            } else {
                items.join(",")
            }
        };
        format!(
            "{name} {} {} {} {} {} {} {} {} {}",
            self.dimension,
            a[0],
            a[1],
            a[2],
            b[0],
            b[1],
            b[2],
            list(self.flags.iter().cloned().collect()),
            list(
                self.members
                    .iter()
                    .map(|(id, n)| format!("{id}={n}"))
                    .collect()
            ),
        )
    }

    fn parse(line: &str) -> Option<(String, Region)> {
        let w: Vec<&str> = line.split_whitespace().collect();
        let [name, dimension, rest @ ..] = w.as_slice() else {
            return None;
        };
        let n: Vec<i32> = rest
            .iter()
            .take(6)
            .map(|s| s.parse().ok())
            .collect::<Option<_>>()?;
        let [x1, y1, z1, x2, y2, z2] = n.try_into().ok()?;
        let items = |s: Option<&&str>| -> Vec<String> {
            match s {
                Some(&"-") | None => Vec::new(),
                Some(s) => s.split(',').map(str::to_owned).collect(),
            }
        };
        let flags = items(rest.get(6))
            .into_iter()
            .filter(|f| flag(f).is_some())
            .collect();
        let members = items(rest.get(7))
            .into_iter()
            .filter_map(|m| {
                m.split_once('=')
                    .map(|(id, n)| (id.to_owned(), n.to_owned()))
            })
            .collect();
        Some((
            (*name).to_owned(),
            Region {
                dimension: (*dimension).to_owned(),
                pos1: [x1, y1, z1],
                pos2: [x2, y2, z2],
                flags,
                members,
            },
        ))
    }

    fn contains(&self, dimension: &str, at: [i32; 3]) -> bool {
        self.dimension == dimension
            && (0..3).all(|k| {
                (self.pos1[k].min(self.pos2[k])..=self.pos1[k].max(self.pos2[k])).contains(&at[k])
            })
    }
}

fn flag(name: &str) -> Option<ProtectedAction> {
    FLAGS.iter().find(|(n, _)| *n == name).map(|(_, a)| *a)
}

fn block(p: &Player) -> [i32; 3] {
    [p.x, p.y, p.z].map(|c| c.floor() as i32)
}

fn valid_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

struct Regions {
    regions: BTreeMap<String, Region>,
    /// Operators who turned on `/region bypass`, by id.
    bypass: BTreeSet<String>,
    /// Corners being set, by player id: (dimension, pos1, pos2).
    selections: BTreeMap<String, (String, Option<[i32; 3]>, Option<[i32; 3]>)>,
    messages: Messages,
}

impl Regions {
    fn text(&self) -> String {
        let mut out: String = self.regions.iter().map(|(n, r)| r.line(n) + "\n").collect();
        for id in &self.bypass {
            out += &format!("bypass {id}\n");
        }
        out
    }

    fn load(text: &str) -> (BTreeMap<String, Region>, BTreeSet<String>) {
        let (mut regions, mut bypass) = (BTreeMap::new(), BTreeSet::new());
        for line in text.lines() {
            if let Some(id) = line.strip_prefix("bypass ") {
                bypass.insert(id.trim().to_owned());
            } else if let Some((name, r)) = Region::parse(line) {
                regions.insert(name, r);
            }
        }
        (regions, bypass)
    }

    /// Saves the regions and gives the server the new ones.
    fn save(&self) -> Result<(), String> {
        storage::set(KEY, self.text().as_bytes())
            .map_err(|e| self.messages.get("save_failed", &[("error", &e)]))?;
        self.protect()
    }

    fn protect(&self) -> Result<(), String> {
        let regions: Vec<world::Region> = self
            .regions
            .iter()
            .map(|(name, r)| world::Region {
                dimension: r.dimension.clone(),
                pos1: (r.pos1[0], r.pos1[1], r.pos1[2]),
                pos2: (r.pos2[0], r.pos2[1], r.pos2[2]),
                deny: FLAGS
                    .iter()
                    .filter(|(n, _)| r.flags.contains(*n))
                    .map(|(_, a)| *a)
                    .collect(),
                exempt: r.members.keys().chain(&self.bypass).cloned().collect(),
                message: self.messages.get("denied", &[("region", name)]),
            })
            .collect();
        world::protect(&regions)
    }

    fn flags_text(&self, r: &Region) -> String {
        let on: Vec<&str> = FLAGS
            .iter()
            .map(|(n, _)| *n)
            .filter(|n| r.flags.contains(*n))
            .collect();
        if on.is_empty() {
            self.messages.get("nothing", &[])
        } else {
            on.join(", ")
        }
    }

    fn region<'a>(&'a mut self, name: Option<&str>) -> Result<(String, &'a mut Region), String> {
        let name = name.unwrap_or_default();
        if !self.regions.contains_key(name) {
            return Err(self.messages.get("unknown_region", &[("region", &name)]));
        }
        let r = self.regions.get_mut(name).ok_or_else(String::new)?;
        Ok((name.to_owned(), r))
    }

    fn run(&mut self, p: &Player, args: &Args) -> Result<Option<String>, String> {
        if players::permission(&p.id).unwrap_or(0) < OPERATOR {
            return Err(self.messages.get("not_operator", &[]));
        }
        let name = args.string("name");
        let (what, value) = (args.string("what"), args.string("value"));
        let ok = |s: String| Ok(Some(s));
        match args.string("action") {
            Some(corner @ ("pos1" | "pos2")) => {
                let at = block(p);
                let s = self.selections.entry(p.id.clone()).or_insert((
                    p.dimension.clone(),
                    None,
                    None,
                ));
                if s.0 != p.dimension {
                    *s = (p.dimension.clone(), None, None);
                }
                if corner == "pos1" {
                    s.1 = Some(at);
                } else {
                    s.2 = Some(at);
                }
                let [x, y, z] = at;
                let a: [(&str, &dyn std::fmt::Display); 4] =
                    [("corner", &corner), ("x", &x), ("y", &y), ("z", &z)];
                ok(self.messages.get("corner_set", &a))
            }
            Some("define") => self.define(p, name, what == Some("full")),
            Some("remove") => {
                let (name, _) = self.region(name)?;
                self.regions.remove(&name);
                self.save()?;
                ok(self.messages.get("removed", &[("region", &name)]))
            }
            Some("flag") => {
                let names: Vec<&str> = FLAGS.iter().map(|(n, _)| *n).collect();
                let unknown = self
                    .messages
                    .get("unknown_flag", &[("flags", &names.join(", "))]);
                let (Some(f), Some(state @ ("on" | "off"))) =
                    (what.filter(|f| flag(f).is_some()), value)
                else {
                    self.region(name)?;
                    return Err(unknown);
                };
                let (rname, r) = self.region(name)?;
                if state == "on" {
                    r.flags.insert(f.to_owned());
                } else {
                    r.flags.remove(f);
                }
                self.save()?;
                let state = self.messages.get(state, &[]);
                let a: [(&str, &dyn std::fmt::Display); 3] =
                    [("region", &rname), ("flag", &f), ("state", &state)];
                ok(self.messages.get("flag_set", &a))
            }
            Some("member") => self.member(name, what, value),
            Some("info") => ok(self.info(p, name)?),
            Some("list") if self.regions.is_empty() => ok(self.messages.get("no_regions", &[])),
            Some("list") => {
                let names: Vec<&str> = self.regions.keys().map(String::as_str).collect();
                ok(self.messages.get("list", &[("regions", &names.join(", "))]))
            }
            Some("bypass") => {
                let on = !self.bypass.remove(&p.id);
                if on {
                    self.bypass.insert(p.id.clone());
                }
                self.save()?;
                ok(self
                    .messages
                    .get(if on { "bypass_on" } else { "bypass_off" }, &[]))
            }
            _ => ok(self.messages.get("usage", &[])),
        }
    }

    fn define(
        &mut self,
        p: &Player,
        name: Option<&str>,
        full: bool,
    ) -> Result<Option<String>, String> {
        let name = name.unwrap_or_default();
        if !valid_name(name) {
            return Err(self.messages.get("bad_name", &[]));
        }
        if self.regions.contains_key(name) {
            return Err(self.messages.get("exists", &[("region", &name)]));
        }
        let Some((dimension, Some(mut pos1), Some(mut pos2))) = self.selections.get(&p.id).cloned()
        else {
            return Err(self.messages.get("no_selection", &[]));
        };
        if dimension != p.dimension {
            return Err(self.messages.get("other_dimension", &[]));
        }
        if full {
            (pos1[1], pos2[1]) = FULL_HEIGHT;
        }
        let region = Region {
            dimension,
            pos1,
            pos2,
            flags: DEFAULT_FLAGS.iter().map(|f| (*f).to_owned()).collect(),
            members: BTreeMap::new(),
        };
        let flags = self.flags_text(&region);
        self.regions.insert(name.to_owned(), region);
        self.save()?;
        let a: [(&str, &dyn std::fmt::Display); 2] = [("region", &name), ("flags", &flags)];
        Ok(Some(self.messages.get("defined", &a)))
    }

    fn member(
        &mut self,
        name: Option<&str>,
        what: Option<&str>,
        player: Option<&str>,
    ) -> Result<Option<String>, String> {
        let usage = self.messages.get("usage", &[]);
        let (Some(add @ ("add" | "remove")), Some(player)) = (what, player) else {
            self.region(name)?;
            return Err(usage);
        };
        let online = players::online();
        let (rname, r) = self.region(name)?;
        let added = add == "add";
        if added {
            // A name online, else one the region has: its id is what the server knows.
            let Some(p) = online.iter().find(|p| p.name.eq_ignore_ascii_case(player)) else {
                return Err(format!("{player}?"));
            };
            r.members.insert(p.id.clone(), p.name.clone());
        } else {
            r.members.retain(|_, n| !n.eq_ignore_ascii_case(player));
        }
        self.save()?;
        let key = if added {
            "member_added"
        } else {
            "member_removed"
        };
        let a: [(&str, &dyn std::fmt::Display); 2] = [("player", &player), ("region", &rname)];
        Ok(Some(self.messages.get(key, &a)))
    }

    fn info(&self, p: &Player, name: Option<&str>) -> Result<String, String> {
        let here: Vec<(&String, &Region)> = match name {
            Some(n) => match self.regions.get_key_value(n) {
                Some(r) => vec![r],
                None => return Err(self.messages.get("unknown_region", &[("region", &n)])),
            },
            None => self
                .regions
                .iter()
                .filter(|(_, r)| r.contains(&p.dimension, block(p)))
                .collect(),
        };
        if here.is_empty() {
            return Ok(self.messages.get("none_here", &[]));
        }
        let lines: Vec<String> = here
            .into_iter()
            .map(|(n, r)| {
                let at = |c: [i32; 3]| format!("{} {} {}", c[0], c[1], c[2]);
                let (pos1, pos2) = (at(r.pos1), at(r.pos2));
                let members: Vec<&str> = r.members.values().map(String::as_str).collect();
                let members = if members.is_empty() {
                    self.messages.get("nobody", &[])
                } else {
                    members.join(", ")
                };
                let flags = self.flags_text(r);
                let a: [(&str, &dyn std::fmt::Display); 6] = [
                    ("region", n),
                    ("dimension", &r.dimension),
                    ("pos1", &pos1),
                    ("pos2", &pos2),
                    ("flags", &flags),
                    ("members", &members),
                ];
                self.messages.get("info", &a)
            })
            .collect();
        Ok(lines.join("\n"))
    }
}

impl ServerMod for Regions {
    fn init() -> Result<(Self, Registration), String> {
        let (regions, bypass) = storage::get(KEY)
            .map(|b| Self::load(&String::from_utf8_lossy(&b)))
            .unwrap_or_default();
        let args = || {
            [
                Argument::word("action").optional(),
                Argument::word("name").optional(),
                Argument::word("what").optional(),
                Argument::word("value").optional(),
            ]
        };
        let hooks = Registration::new()
            .command_with("region", args())
            .command_with("rg", args());
        let me = Regions {
            regions,
            bypass,
            selections: BTreeMap::new(),
            messages: Messages::load(MESSAGES),
        };
        me.protect()?;
        Ok((me, hooks))
    }

    fn command(
        &mut self,
        player: &Player,
        _name: &str,
        args: &Args,
    ) -> Result<Option<String>, String> {
        self.run(player, args)
    }

    fn player_left(&mut self, player: &Player) {
        self.selections.remove(&player.id);
    }
}

horizon_mod_sdk::export_server_mod!(Regions);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_region_line_reads_back() {
        let r = Region {
            dimension: "minecraft:overworld".into(),
            pos1: [10, -64, -5],
            pos2: [-10, 320, 5],
            flags: ["break", "pvp"].map(String::from).into(),
            members: [("id-1".to_owned(), "Alex".to_owned())].into(),
        };
        assert_eq!(
            Region::parse(&r.line("spawn")),
            Some(("spawn".into(), r.clone()))
        );
        let bare = Region {
            flags: BTreeSet::new(),
            members: BTreeMap::new(),
            ..r
        };
        assert_eq!(Region::parse(&bare.line("a")), Some(("a".into(), bare)));
    }

    #[test]
    fn a_region_takes_both_corners_in_its_dimension() {
        let (_, r) = Region::parse("s minecraft:overworld 10 60 -5 -10 70 5 break -").unwrap();
        assert!(r.contains("minecraft:overworld", [10, 70, 5]));
        assert!(r.contains("minecraft:overworld", [-10, 60, -5]));
        assert!(!r.contains("minecraft:overworld", [11, 65, 0]));
        assert!(!r.contains("minecraft:the_nether", [0, 65, 0]));
    }

    #[test]
    fn names_are_words() {
        assert!(valid_name("spawn_1"));
        assert!(!valid_name("a b"));
        assert!(!valid_name(""));
    }
}
