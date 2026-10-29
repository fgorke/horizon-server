//! A small read-only web page with every player's statistics, live for the
//! players online: `GET /` is the page (`stats_web.html`), `GET /api/stats`
//! the players and their `stats/<uuid>.json` statistics. The game hands the
//! online players' counters over once a second ([`StatsWeb::set_live`]);
//! everyone else's come from the files.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

/// The page's port unless `HORIZON_STATS_PORT` says otherwise.
pub const PORT: u16 = 8100;
const PAGE: &str = include_str!("stats_web.html");
const MAX_REQUEST: usize = 8 * 1024;

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
}

pub struct StatsWeb {
    state: Arc<Mutex<State>>,
}

impl StatsWeb {
    /// Binds the port and serves `world`'s statistics, with the names from
    /// `usercache`.
    pub fn start(port: u16, world: PathBuf, usercache: PathBuf) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        std::thread::Builder::new().name("stats web".into()).spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => {
                        if let Err(e) = serve(s, &shared, &world, &usercache) {
                            log::debug!("stats web: {e}");
                        }
                    }
                    Err(e) => log::debug!("stats web: accept: {e}"),
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

    /// The player left: their file is up to date again.
    pub fn remove_player(&self, uuid: u128) {
        self.lock().live.remove(&dashed(uuid));
    }
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
            out.insert(uuid.clone(), json!({ "uuid": uuid, "name": name, "online": false, "stats": stats }));
        }
    }
    for (uuid, p) in &state.live {
        out.insert(
            uuid.clone(),
            json!({ "uuid": uuid, "name": p.name, "online": true, "stats": p.stats }),
        );
    }
    Value::Array(out.into_values().collect())
}

fn respond(s: &mut TcpStream, status: &str, kind: &str, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
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
    match path.split('?').next().unwrap_or("") {
        "/" | "/index.html" => respond(&mut s, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()),
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
