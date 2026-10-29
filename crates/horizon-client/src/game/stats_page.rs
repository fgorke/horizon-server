//! The stats page of a dedicated server (`horizon_client::stats_web`), on
//! port 8100 (`HORIZON_STATS_PORT` to change it): started with the first
//! guest tick, and given each online guest's counters once a second.

use super::{Game, lan::LanHost};
use horizon_client::stats_web::{self, LivePlayer, StatsWeb};
use std::sync::OnceLock;

static WEB: OnceLock<Option<StatsWeb>> = OnceLock::new();
/// How often, in ticks, a guest's counters reach the page.
const EVERY: i64 = 20;

fn web(world: &std::path::Path) -> Option<&'static StatsWeb> {
    WEB.get_or_init(|| {
        let port = std::env::var("HORIZON_STATS_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(stats_web::PORT);
        // The server's folder holds the world and usercache.json.
        let usercache = world.parent().unwrap_or(std::path::Path::new("")).join("usercache.json");
        match StatsWeb::start(port, world.to_path_buf(), usercache) {
            Ok(w) => Some(w),
            Err(e) => {
                log::warn!("stats page: port {port}: {e}");
                None
            }
        }
    })
    .as_ref()
}

/// A guest left: the page reads their saved file from now on.
pub(super) fn guest_left(uuid: u128) {
    if let Some(web) = WEB.get().and_then(Option::as_ref) {
        web.remove_player(uuid);
    }
}

impl Game<'_> {
    /// Once a tick for a guest of a dedicated server: once a second, their
    /// counters for the stats page.
    pub(super) fn stats_page_tick(&self, host: &LanHost, conn: u32) {
        if self.options.dedicated.is_none() || self.world.game_time % EVERY != 0 {
            return;
        }
        let (Some(g), Some(play)) = (host.hub.guest(conn), host.play.get(&conn)) else {
            return;
        };
        let Some(web) = web(&self.world_root) else {
            return;
        };
        web.set_player(
            g.uuid,
            LivePlayer {
                name: g.name.clone(),
                stats: play.stats.values.clone(),
            },
        );
    }
}
