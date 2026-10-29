//! The stats page of a dedicated server (`horizon_client::stats_web`), on
//! port 8100 (`HORIZON_STATS_PORT` to change it): started with the
//! server's first tick, and given each online guest's counters once a second.

use super::{Game, lan::LanHost};
use horizon_client::stats_web::{self, LivePlayer, StatsWeb};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};

static WEB: OnceLock<Option<StatsWeb>> = OnceLock::new();
/// Seconds without moving or looking around after which a player is away.
const AFK_AFTER: u64 = 180;
/// How often, in seconds, the AFK times are written.
const SAVE_EVERY: i64 = 60;
/// Each guest's (by connection) last place and look, and how many seconds
/// since it changed.
static STILL: Mutex<Option<HashMap<u32, ([f64; 3], f32, f32, u64)>>> = Mutex::new(None);
/// How often, in ticks, a guest's counters reach the page.
const EVERY: i64 = 20;

fn web(world: &std::path::Path, resources: &std::path::Path) -> Option<&'static StatsWeb> {
    WEB.get_or_init(|| {
        let port = std::env::var("HORIZON_STATS_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(stats_web::PORT);
        // The server's folder holds the world and usercache.json.
        let usercache = world.parent().unwrap_or(std::path::Path::new("")).join("usercache.json");
        let lang = resources.join("assets/minecraft/lang/de_de.json");
        match StatsWeb::start(port, world.to_path_buf(), usercache, &lang) {
            Ok(w) => Some(w),
            Err(e) => {
                log::warn!("stats page: port {port}: {e}");
                None
            }
        }
    })
    .as_ref()
}

/// Starts the page, once.
pub(super) fn start(world: &std::path::Path, resources: &std::path::Path) {
    web(world, resources);
}

/// A guest left: the page reads their saved file from now on.
pub(super) fn guest_left(uuid: u128) {
    if let Some(web) = WEB.get().and_then(Option::as_ref) {
        web.remove_player(uuid);
        web.save_afk();
    }
}

/// One more second of `conn` at `at` looking `yaw`/`pitch`: the seconds
/// to add to their AFK time (the whole still stretch once it reaches
/// [`AFK_AFTER`], one a second after).
fn afk_seconds(conn: u32, at: [f64; 3], yaw: f32, pitch: f32) -> u64 {
    let mut still = STILL.lock().unwrap_or_else(PoisonError::into_inner);
    let entry = still.get_or_insert_with(HashMap::new).entry(conn).or_insert((at, yaw, pitch, 0));
    if entry.0 != at || entry.1 != yaw || entry.2 != pitch {
        *entry = (at, yaw, pitch, 0);
        return 0;
    }
    entry.3 += 1;
    match entry.3 {
        n if n == AFK_AFTER => n,
        n if n > AFK_AFTER => 1,
        _ => 0,
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
        let Some(web) = web(&self.world_root, &self.options.resources) else {
            return;
        };
        let away = afk_seconds(conn, g.position, g.yaw, g.pitch);
        if away > 0 {
            web.add_afk(g.uuid, away);
        }
        if self.world.game_time % (EVERY * SAVE_EVERY) == 0 {
            web.save_afk();
        }
        web.set_player(
            g.uuid,
            LivePlayer {
                name: g.name.clone(),
                stats: play.stats.values.clone(),
            },
        );
    }
}
