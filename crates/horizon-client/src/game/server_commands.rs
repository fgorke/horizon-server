//! How Horizon Server's own commands answer (`/tps`, `/dimension`, `/plugins`): a title in a
//! turquoise-to-blue gradient between two rules, then one `»` row per reading, in the plugins'
//! markup (`horizon_client_core::markup`).
use super::{Game, lan::LanHost, lan_actions::GuestCommand};
use horizon_client::text::Text;

/// A rule either side of a title: struck-through spaces.
const RULE: &str = "&8&m              &r";
/// The width of a [`bar`], in strokes.
const BAR: usize = 20;

/// A title line: `name` between two rules.
pub(super) fn title(name: &str) -> String {
    format!("{RULE} <gradient:#40E0D0:#1E90FF>&l{name}</gradient> {RULE}")
}

/// A reading: `» label: value`.
pub(super) fn row(label: &str, value: &str) -> String {
    format!("\n &8» &7{label}: {value}")
}

/// `fraction` (0 to 1) of [`BAR`] strokes in `colour` (a legacy code), the rest dark.
fn bar(fraction: f64, colour: char) -> String {
    let filled = ((fraction.clamp(0.0, 1.0) * BAR as f64).round() as usize).min(BAR);
    format!(
        "&{colour}{}&8{}",
        "|".repeat(filled),
        "|".repeat(BAR - filled)
    )
}

/// Green, yellow or red: `value` against what is good (`good`) and what is still fine (`fine`),
/// higher being better.
fn grade(value: f64, good: f64, fine: f64) -> char {
    if value >= good {
        'a'
    } else if value >= fine {
        'e'
    } else {
        'c'
    }
}

pub(super) fn not_operator() -> Text {
    Text::markup("&c✖ &7You need to be an operator for that.")
}

impl Game<'_> {
    /// `/tps`: the tick rate against its target, the milliseconds per tick (average, then the
    /// percentiles `/tick query` prints), how much of the tick's budget they use, players and
    /// loaded chunks.
    pub(super) fn tps_reply(&self, host: &LanHost) -> Text {
        let millis = |nanos: u64| nanos as f64 / 1e6;
        let target = f64::from(self.clock.rate());
        let average = millis(self.clock.average_nanos());
        let budget = millis(self.clock.tick_duration().as_nanos() as u64);
        let tps = if average > budget {
            1000.0 / average
        } else {
            target
        };
        let samples = self.clock.sorted_samples();
        let at = |q: f64| {
            let i = ((samples.len() as f64 * q) as usize).min(samples.len().saturating_sub(1));
            samples.get(i).copied().map_or(0.0, millis)
        };
        let load = if budget > 0.0 { average / budget } else { 0.0 };
        let tps_grade = grade(tps / target, 0.975, 0.75);
        let load_grade = grade(1.0 - load, 0.4, 0.0);
        let players = host.hub.guests.iter().filter(|g| g.placed).count();
        let max = host.hub.lan.shared.max_players();
        let mut out = title("Server");
        out += &row(
            "TPS",
            &format!(
                "&{tps_grade}{tps:.1} &8/ {target:.0}  {}",
                bar(tps / target, tps_grade)
            ),
        );
        out += &row(
            "MSPT",
            &format!(
                "&{load_grade}{average:.2} ms &8(P50 {:.2} · P95 {:.2} · P99 {:.2})",
                at(0.5),
                at(0.95),
                at(0.99)
            ),
        );
        out += &row(
            "Load",
            &format!(
                "&{load_grade}{:.0} % &8of {budget:.0} ms  {}",
                load * 100.0,
                bar(load, load_grade)
            ),
        );
        out += &row(
            "Players",
            &format!(
                "&f{players}&8/{max}   &7Chunks: &f{}",
                self.world.chunks.len()
            ),
        );
        Text::markup(&out)
    }

    /// `/dimension`: whether the Nether and the End are open, for everyone; `/dimension
    /// <nether|end> <on|off>` (operators) opens or closes one.
    pub(super) fn dimension_reply(&mut self, host: &mut LanHost, conn: u32, args: &[&str]) -> Text {
        let usage = "\n &8/dimension <nether|end> <on|off>";
        let state = |open: bool| if open { "&aopen" } else { "&cclosed" };
        match args {
            [] => {
                let nether = self.ui.rules.bool("allow_entering_nether_using_portals");
                let end = super::dimensions::end_open(&self.world_root);
                let mut out = title("Dimensions");
                out += &row("Nether", state(nether));
                out += &row("End", state(end));
                out += usage;
                Text::markup(&out)
            }
            [dim @ ("nether" | "end"), switch @ ("on" | "off")] => {
                if host.hub.guest(conn).is_none_or(|g| g.permission < 2) {
                    return not_operator();
                }
                let open = *switch == "on";
                let saved = if *dim == "nether" {
                    host.commands.push(GuestCommand {
                        conn,
                        command: format!("gamerule allow_entering_nether_using_portals {open}"),
                        permission: 4,
                        integrated: false,
                        quiet: true,
                    });
                    true
                } else {
                    super::dimensions::set_end_open(&self.world_root, open)
                };
                let name = if *dim == "nether" { "Nether" } else { "End" };
                Text::markup(&if !saved {
                    "&c✖ &7Could not save the setting.".to_owned()
                } else if open {
                    format!("&a✔ &7The &f{name} &7is now {}&7.", state(true))
                } else {
                    format!("&c✔ &7The &f{name} &7is now {}&7.", state(false))
                })
            }
            _ => Text::markup(&format!("&7Usage:{usage}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bar_fills_its_share_and_stays_its_width() {
        let half = "|".repeat(BAR / 2);
        assert_eq!(bar(0.5, 'a'), format!("&a{half}&8{half}"));
        assert_eq!(bar(2.0, 'c'), format!("&c{}&8", "|".repeat(BAR)));
        assert_eq!(Text::markup(&bar(-1.0, 'a')).plain(), "|".repeat(BAR));
    }

    #[test]
    fn a_title_reads_plainly_between_its_rules() {
        let plain = Text::markup(&title("Server")).plain();
        assert!(plain.trim().starts_with("Server") || plain.contains(" Server "));
    }

    #[test]
    fn grades_go_green_yellow_red() {
        assert_eq!(grade(1.0, 0.975, 0.75), 'a');
        assert_eq!(grade(0.8, 0.975, 0.75), 'e');
        assert_eq!(grade(0.5, 0.975, 0.75), 'c');
    }
}
