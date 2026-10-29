//! A small read-only web page with every player's statistics, live for the
//! players online: `GET /` is the page (`stats_web.html`), `GET /api/stats`
//! the players and their `stats/<uuid>.json` statistics, `GET /skin/head/<uuid>`
//! and `/skin/body/<uuid>` their skins from mc-heads.net (kept an hour, so
//! the page's many heads are one fetch each). The game hands the
//! online players' counters over once a second ([`StatsWeb::set_live`]);
//! everyone else's come from the files.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The page's port unless `HORIZON_STATS_PORT` says otherwise.
pub const PORT: u16 = 8100;
const PAGE: &str = include_str!("stats_web.html");
const MAX_REQUEST: usize = 8 * 1024;
/// How long a fetched skin is served before it is fetched again.
const SKIN_TTL: Duration = Duration::from_secs(3600);
const SKIN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SKIN: u64 = 512 * 1024;

/// What the game knows of an online player.
#[derive(Clone, Debug)]
pub struct LivePlayer {
    pub name: String,
    /// `stats[type][key]`, as in the stats file.
    pub stats: BTreeMap<String, BTreeMap<String, i32>>,
}

#[derive(Default)]
struct State {
    live: BTreeMap<String, LivePlayer>,
    /// Seconds each player (dashed UUID) has been away from the keyboard.
    afk: BTreeMap<String, u64>,
    /// Where `afk` is kept.
    afk_path: PathBuf,
    /// `{"world": <the world folder's name>}`.
    info: String,
    /// The block, item, entity and statistic names of de_de.json, as JSON.
    lang: String,
    /// (`head`/`body`, uuid): when fetched, the PNG.
    skins: HashMap<(&'static str, String), (Instant, Vec<u8>)>,
}

pub struct StatsWeb {
    state: Arc<Mutex<State>>,
}

impl StatsWeb {
    /// Binds the port and serves `world`'s statistics, with the names from
    /// `usercache`.
    pub fn start(port: u16, world: PathBuf, usercache: PathBuf, lang: &Path) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        let afk_path = world.join("horizon-afk.json");
        let state = Arc::new(Mutex::new(State {
            afk: std::fs::read(&afk_path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default(),
            afk_path,
            lang: names_of(lang),
            info: json!({ "world": world.file_name().map(|n| n.to_string_lossy().into_owned()) }).to_string(),
            ..State::default()
        }));
        let shared = Arc::clone(&state);
        std::thread::Builder::new().name("stats web".into()).spawn(move || {
            let (world, usercache) = (Arc::new(world), Arc::new(usercache));
            for stream in listener.incoming() {
                let s = match stream {
                    Ok(s) => s,
                    Err(e) => {
                        log::debug!("stats web: accept: {e}");
                        continue;
                    }
                };
                // A skin fetch takes a while: the other requests don't wait for it.
                let (shared, world, usercache) = (Arc::clone(&shared), Arc::clone(&world), Arc::clone(&usercache));
                let spawned = std::thread::Builder::new().name("stats web request".into()).spawn(move || {
                    if let Err(e) = serve(s, &shared, &world, &usercache) {
                        log::debug!("stats web: {e}");
                    }
                });
                if let Err(e) = spawned {
                    log::warn!("stats web: no thread for a request: {e}");
                }
            }
        })?;
        log::info!("Stats page at port {port}");
        Ok(StatsWeb { state })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// An online player's counters now; the others are read from disk.
    pub fn set_player(&self, uuid: u128, player: LivePlayer) {
        self.lock().live.insert(dashed(uuid), player);
    }

    /// Seconds a player has been away, added up.
    pub fn add_afk(&self, uuid: u128, seconds: u64) {
        *self.lock().afk.entry(dashed(uuid)).or_default() += seconds;
    }

    /// Writes the AFK times to their file.
    pub fn save_afk(&self) {
        let state = self.lock();
        let text = serde_json::to_string_pretty(&state.afk).unwrap_or_default();
        if let Err(e) = std::fs::write(&state.afk_path, text) {
            log::warn!("stats page: {}: {e}", state.afk_path.display());
        }
    }

    /// The player left: their file is up to date again.
    pub fn remove_player(&self, uuid: u128) {
        self.lock().live.remove(&dashed(uuid));
    }
}

/// The names the page shows from a language file: blocks, items,
/// entities and statistics (`{}` without the file).
fn names_of(lang: &Path) -> String {
    const KINDS: [&str; 4] = ["block.minecraft.", "item.minecraft.", "entity.minecraft.", "stat.minecraft."];
    let all: BTreeMap<String, Value> = std::fs::read(lang)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let names: serde_json::Map<String, Value> = all
        .into_iter()
        // `block.minecraft.x` and not its sub-keys (`.desc`, `.beds`).
        .filter(|(k, v)| {
            v.is_string()
                && KINDS.iter().any(|p| k.strip_prefix(p).is_some_and(|rest| !rest.contains('.')))
        })
        .collect();
    Value::Object(names).to_string()
}

/// A dashed lowercase UUID, as the stats files are named.
pub fn dashed(uuid: u128) -> String {
    let h = format!("{uuid:032x}");
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() }
        })
}

fn names(usercache: &Path) -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read(usercache) else {
        return BTreeMap::new();
    };
    let Ok(Value::Array(entries)) = serde_json::from_slice::<Value>(&text) else {
        return BTreeMap::new();
    };
    entries
        .iter()
        .filter_map(|e| Some((e["uuid"].as_str()?.to_lowercase(), e["name"].as_str()?.to_owned())))
        .collect()
}

/// Every player with a stats file or online: UUID, name, online, statistics.
fn players(state: &State, world: &Path, usercache: &Path) -> Value {
    let names = names(usercache);
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    if let Ok(dir) = std::fs::read_dir(world.join("players").join("stats")) {
        for entry in dir.flatten() {
            let path = entry.path();
            let Some(uuid) = path.file_stem().and_then(|s| s.to_str()).map(str::to_lowercase) else {
                continue;
            };
            if !is_uuid(&uuid) || state.live.contains_key(&uuid) {
                continue;
            }
            let stats = std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .map(|v| v["stats"].clone())
                .unwrap_or_else(|| json!({}));
            let name = names.get(&uuid).cloned().unwrap_or_else(|| uuid[..8].to_owned());
            out.insert(uuid.clone(), json!({ "uuid": uuid, "name": name, "online": false, "stats": stats, "afk": state.afk.get(&uuid).copied().unwrap_or(0) }));
        }
    }
    for (uuid, p) in &state.live {
        out.insert(
            uuid.clone(),
            json!({ "uuid": uuid, "name": p.name, "online": true, "stats": p.stats, "afk": state.afk.get(uuid).copied().unwrap_or(0) }),
        );
    }
    Value::Array(out.into_values().collect())
}

/// A skin as PNG: kept, or fetched from mc-heads.net (`kind` is `head` or
/// `body`).
fn skin(state: &Mutex<State>, kind: &'static str, uuid: &str) -> Option<Vec<u8>> {
    let key = (kind, uuid.to_owned());
    if let Some((at, png)) = state.lock().unwrap_or_else(PoisonError::into_inner).skins.get(&key)
        && at.elapsed() < SKIN_TTL
    {
        return Some(png.clone());
    }
    let url = match kind {
        "head" => format!("https://mc-heads.net/avatar/{uuid}/64"),
        _ => format!("https://mc-heads.net/body/{uuid}/240"),
    };
    let fetched = ureq::AgentBuilder::new()
        .timeout(SKIN_TIMEOUT)
        .build()
        .get(&url)
        .call()
        .map_err(|e| e.to_string())
        .and_then(|r| {
            let mut png = Vec::new();
            r.into_reader()
                .take(MAX_SKIN)
                .read_to_end(&mut png)
                .map_err(|e| e.to_string())?;
            Ok(png)
        });
    let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
    match fetched {
        Ok(png) if png.starts_with(b"\x89PNG") => {
            state.skins.insert(key, (Instant::now(), png.clone()));
            Some(png)
        }
        other => {
            if let Err(e) = other {
                log::debug!("stats web: skin {url}: {e}");
            }
            // An old copy beats none.
            state.skins.get(&key).map(|(_, png)| png.clone())
        }
    }
}

fn respond(s: &mut TcpStream, status: &str, kind: &str, body: &[u8]) -> std::io::Result<()> {
    let cache = if kind == "image/png" { "public, max-age=3600" } else { "no-store" };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\
         Cache-Control: {cache}\r\nX-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(body)
}

fn serve(mut s: TcpStream, state: &Mutex<State>, world: &Path, usercache: &Path) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = s.read(&mut chunk)?;
        if n == 0 || buf.len() + n > MAX_REQUEST {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let line = buf.split(|b| *b == b'\r').next().unwrap_or_default();
    let line = String::from_utf8_lossy(line);
    let mut parts = line.split(' ');
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if method != "GET" {
        return respond(&mut s, "405 Method Not Allowed", "text/plain", b"GET only");
    }
    let path = path.split('?').next().unwrap_or("");
    for kind in ["head", "body"] {
        if let Some(uuid) = path.strip_prefix("/skin/").and_then(|p| p.strip_prefix(kind)).and_then(|p| p.strip_prefix('/')) {
            let uuid = uuid.to_lowercase();
            if !is_uuid(&uuid) {
                return respond(&mut s, "404 Not Found", "text/plain", b"not found");
            }
            return match skin(state, kind, &uuid) {
                Some(png) => respond(&mut s, "200 OK", "image/png", &png),
                None => respond(&mut s, "502 Bad Gateway", "text/plain", b"no skin"),
            };
        }
    }
    match path {
        "/" | "/index.html" => respond(&mut s, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()),
        "/api/info" => {
            let body = state.lock().unwrap_or_else(PoisonError::into_inner).info.clone();
            respond(&mut s, "200 OK", "application/json", body.as_bytes())
        }
        "/api/lang" => {
            let body = state.lock().unwrap_or_else(PoisonError::into_inner).lang.clone();
            respond(&mut s, "200 OK", "application/json", body.as_bytes())
        }
        "/api/stats" => {
            let body = {
                let state = state.lock().unwrap_or_else(PoisonError::into_inner);
                players(&state, world, usercache)
            };
            respond(&mut s, "200 OK", "application/json", body.to_string().as_bytes())
        }
        _ => respond(&mut s, "404 Not Found", "text/plain", b"not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_dashed_and_checked() {
        let u = 0x65bb83c4_f8ad_4be6_9953_12d6c353d9de_u128;
        assert_eq!(dashed(u), "65bb83c4-f8ad-4be6-9953-12d6c353d9de");
        assert!(is_uuid(&dashed(u)));
        assert!(!is_uuid("../../etc/passwd"));
        assert!(!is_uuid("65bb83c4-f8ad-4be6-9953-12d6c353d9dz"));
    }

    #[test]
    fn the_live_players_win_over_their_files() {
        let dir = std::env::temp_dir().join(format!("hz-stats-{}", std::process::id()));
        let stats = dir.join("players").join("stats");
        std::fs::create_dir_all(&stats).unwrap();
        let (a, b) = (dashed(1), dashed(2));
        let file = json!({ "stats": { "minecraft:custom": { "minecraft:deaths": 3 } } });
        for u in [&a, &b] {
            std::fs::write(stats.join(format!("{u}.json")), file.to_string()).unwrap();
        }
        std::fs::write(stats.join("notes.json"), "{}").unwrap();
        let cache = dir.join("usercache.json");
        std::fs::write(&cache, json!([{ "uuid": a, "name": "Ann" }]).to_string()).unwrap();
        let mut state = State::default();
        let mut live = BTreeMap::new();
        live.insert("minecraft:custom".to_owned(), BTreeMap::from([("minecraft:deaths".to_owned(), 5)]));
        state.live.insert(b.clone(), LivePlayer { name: "Bob".into(), stats: live });
        let got = players(&state, &dir, &cache);
        let got = got.as_array().unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["name"], "Ann");
        assert_eq!(got[0]["online"], false);
        assert_eq!(got[0]["stats"]["minecraft:custom"]["minecraft:deaths"], 3);
        assert_eq!(got[1]["name"], "Bob");
        assert_eq!(got[1]["online"], true);
        assert_eq!(got[1]["stats"]["minecraft:custom"]["minecraft:deaths"], 5);
        std::fs::remove_dir_all(&dir).ok();
    }
}
