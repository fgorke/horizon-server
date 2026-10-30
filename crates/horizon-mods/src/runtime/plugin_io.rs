//! Server plugins' newer imports and hooks (WIT `movement`, `channels`, `network`, `random`,
//! `statistics` and the `plugin-events` export). A plugin built before them imports none and
//! exports no `plugin-events`: it loads as before and hears none of these hooks.
//!
//! The sockets of capability `network` are served by threads of their own, which only queue
//! what arrives; the game hands it to the plugin on its own thread with [`Mods::pump_network`],
//! between ticks. A plugin that stops drops its queues, and its threads end with them.
use super::{CHAT_PER_CALL, FUEL_EVENT, Mods, Output, ServerPlayer, State, Store};
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError, Weak,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../horizon-mod-sdk/wit",
        world: "plugin-host",
        with: {
            "horizon:mods/players": super::super::server_bindings::horizon::mods::players,
        },
    });
}
pub(super) use bindings::PluginHost;
pub use bindings::exports::horizon::mods::plugin_events::{Datagram, Request, Response};
use bindings::horizon::mods::{channels, movement, network, random, statistics};

/// `channels.listen` prefixes per plugin, and `channels.send` payloads per call and their size.
const LISTENS: usize = 16;
const PAYLOADS_PER_CALL: usize = 256;
const MAX_PAYLOAD: usize = 1 << 20;
/// `network.udp-send` datagrams per call, and their size.
const DATAGRAMS_PER_CALL: usize = 4096;
const MAX_DATAGRAM: usize = 4096;
/// Datagrams kept for the plugin between pumps; more are dropped.
const UDP_INBOX: usize = 8192;
/// A request's head and body, the requests kept between pumps, connections served at once,
/// how long one may wait for the plugin, and a response's size.
const MAX_HEAD: usize = 8 << 10;
const MAX_BODY: usize = 64 << 10;
const HTTP_INBOX: usize = 256;
const HTTP_CONNECTIONS: usize = 32;
const HTTP_WAIT: Duration = Duration::from_secs(10);
const MAX_RESPONSE: usize = 16 << 20;
/// `random.bytes` per call.
const MAX_RANDOM: u32 = 4096;
/// `statistics.translations` prefixes.
const PREFIXES: usize = 16;
/// How long a socket thread waits before it looks whether its plugin still runs.
const POLL: Duration = Duration::from_millis(100);

/// A player's motion, beside [`ServerPlayer`] (`movement.motion-of`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayerMotion {
    pub id: String,
    pub on_ground: bool,
    pub gliding: bool,
    pub fall_distance: f64,
    pub riding: bool,
}

/// What capability `statistics` reads: the world's folder (its `players/stats/`), the
/// server's `usercache.json`, the game's resources (`assets/minecraft/lang/`) and the
/// statistics of the players online now, as JSON by UUID.
#[derive(Clone, Debug, Default)]
pub struct ServerFiles {
    pub world: PathBuf,
    pub usercache: PathBuf,
    pub resources: PathBuf,
    pub live: HashMap<String, String>,
}

/// Shared by every plugin's store.
pub(super) type FilesSlot = Arc<Mutex<ServerFiles>>;

type HttpInbox = Mutex<VecDeque<(Request, mpsc::Sender<Response>)>>;

/// One plugin's channels and sockets.
#[derive(Default)]
pub(super) struct Io {
    listens: Vec<String>,
    udp: Option<Udp>,
    http: Option<Arc<HttpInbox>>,
    /// Sockets are recorded, not opened (`cargo horizon-mod test`): sent datagrams are
    /// [`Output::UdpSent`].
    simulated: bool,
    /// This call's `channels.send` payloads, `udp-send` datagrams and `push`es.
    payloads: usize,
    datagrams: usize,
    pushes: usize,
    files: FilesSlot,
}

struct Udp {
    /// None when simulated.
    socket: Option<Arc<UdpSocket>>,
    inbox: Arc<Mutex<VecDeque<Datagram>>>,
}

impl Io {
    pub(super) fn new(simulated: bool, files: FilesSlot) -> Self {
        Io {
            simulated,
            files,
            ..Io::default()
        }
    }
    /// A hook call starts: the per-call counts start over.
    pub(super) fn new_call(&mut self) {
        self.payloads = 0;
        self.datagrams = 0;
        self.pushes = 0;
    }
    fn hears(&self, channel: &str) -> bool {
        self.listens.iter().any(|p| channel.starts_with(p.as_str()))
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `namespace:path`, as vanilla's resource locations spell them.
fn channel_ok(channel: &str) -> bool {
    let plain = |t: &str, extra: &[u8]| {
        !t.is_empty()
            && t.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b"_.-".contains(&b)
                    || extra.contains(&b)
            })
    };
    channel.len() <= 256
        && channel
            .split_once(':')
            .is_some_and(|(ns, path)| plain(ns, b"") && plain(path, b"/"))
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, lowercase: a stats file's name, never a path.
fn uuid_ok(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_digit() || (b'a'..=b'f').contains(&c)
            }
        })
}

fn port_ok(port: u16) -> Result<(), String> {
    (port >= 1024)
        .then_some(())
        .ok_or_else(|| format!("port {port}: a plugin's port is 1024 or above"))
}

impl State {
    fn online(&self, player: &str) -> bool {
        self.snapshot.players.iter().any(|p| p.id == player)
    }
}

impl movement::Host for State {
    fn motion_of(&mut self, player: String) -> Option<movement::Motion> {
        let m = self.snapshot.motions.iter().find(|m| m.id == player)?;
        Some(movement::Motion {
            on_ground: m.on_ground,
            gliding: m.gliding,
            fall_distance: m.fall_distance,
            riding: m.riding,
        })
    }
    fn glide(&mut self, player: String) -> bool {
        if !self.online(&player) {
            return false;
        }
        self.out.push(Output::Glide { player });
        true
    }
    fn allow_double_jump(&mut self, player: String, allow: bool) -> bool {
        if !self.online(&player) {
            return false;
        }
        self.out.push(Output::AllowDoubleJump { player, allow });
        true
    }
    fn push(&mut self, player: String, x: f64, y: f64, z: f64) -> bool {
        if self.io.pushes >= CHAT_PER_CALL
            || ![x, y, z].iter().all(|v| v.is_finite())
            || !self.online(&player)
        {
            return false;
        }
        self.io.pushes += 1;
        self.out.push(Output::Push {
            player,
            velocity: [x, y, z],
        });
        true
    }
}

impl channels::Host for State {
    fn listen(&mut self, prefix: String) -> Result<(), String> {
        let namespaced = prefix
            .split_once(':')
            .is_some_and(|(ns, _)| channel_ok(&format!("{ns}:x")));
        if !namespaced || prefix.starts_with("minecraft:") {
            return Err(format!(
                "{prefix:?}: a prefix is `namespace:` or longer, and not minecraft:"
            ));
        }
        if self.io.listens.contains(&prefix) {
            return Ok(());
        }
        if self.io.listens.len() >= LISTENS {
            return Err(format!("a plugin listens to up to {LISTENS} prefixes"));
        }
        self.io.listens.push(prefix);
        Ok(())
    }
    fn send(&mut self, player: String, channel: String, data: Vec<u8>) -> bool {
        if self.io.payloads >= PAYLOADS_PER_CALL
            || data.len() > MAX_PAYLOAD
            || !channel_ok(&channel)
            || !self.online(&player)
        {
            return false;
        }
        self.io.payloads += 1;
        self.out.push(Output::Payload {
            player,
            channel,
            data,
        });
        true
    }
}

impl network::Host for State {
    fn udp_bind(&mut self, port: u16) -> Result<(), String> {
        port_ok(port)?;
        if self.io.udp.is_some() {
            return Err("the plugin's UDP socket is bound already".into());
        }
        let inbox = Arc::new(Mutex::new(VecDeque::new()));
        let socket = if self.io.simulated {
            None
        } else {
            let socket =
                UdpSocket::bind(("0.0.0.0", port)).map_err(|e| format!("port {port}: {e}"))?;
            socket
                .set_read_timeout(Some(POLL))
                .map_err(|e| e.to_string())?;
            let socket = Arc::new(socket);
            serve_udp(Arc::clone(&socket), Arc::downgrade(&inbox), &self.id)?;
            Some(socket)
        };
        log::info!("Horizon: plugin {} listens on UDP port {port}", self.id);
        self.io.udp = Some(Udp { socket, inbox });
        Ok(())
    }
    fn udp_send(&mut self, address: String, data: Vec<u8>) -> bool {
        let (Some(udp), Ok(to)) = (&self.io.udp, address.parse::<SocketAddr>()) else {
            return false;
        };
        if self.io.datagrams >= DATAGRAMS_PER_CALL || data.len() > MAX_DATAGRAM {
            return false;
        }
        self.io.datagrams += 1;
        match &udp.socket {
            Some(socket) => {
                if let Err(e) = socket.send_to(&data, to) {
                    log::debug!("plugin {}: UDP to {to}: {e}", self.id);
                }
            }
            None => self.out.push(Output::UdpSent {
                address: to.to_string(),
                data,
            }),
        }
        true
    }
    fn http_listen(&mut self, port: u16) -> Result<(), String> {
        port_ok(port)?;
        if self.io.http.is_some() {
            return Err("the plugin serves HTTP already".into());
        }
        let inbox = Arc::new(Mutex::new(VecDeque::new()));
        if !self.io.simulated {
            let listener =
                TcpListener::bind(("0.0.0.0", port)).map_err(|e| format!("port {port}: {e}"))?;
            listener.set_nonblocking(true).map_err(|e| e.to_string())?;
            serve_http(listener, Arc::downgrade(&inbox), &self.id)?;
        }
        log::info!("Horizon: plugin {} serves HTTP on port {port}", self.id);
        self.io.http = Some(inbox);
        Ok(())
    }
}

impl random::Host for State {
    fn bytes(&mut self, len: u32) -> Vec<u8> {
        let mut out = vec![0; len.min(MAX_RANDOM) as usize];
        if let Err(e) = getrandom::fill(&mut out) {
            // Never predictable bytes in place of random ones: none.
            log::warn!("plugin {}: no secure random source: {e}", self.id);
            return Vec::new();
        }
        out
    }
}

impl statistics::Host for State {
    fn of(&mut self, player: String) -> Option<String> {
        let files = lock(&self.io.files).clone();
        if let Some(live) = files.live.get(&player) {
            return Some(live.clone());
        }
        if !uuid_ok(&player) {
            return None;
        }
        let path = files
            .world
            .join("players/stats")
            .join(format!("{player}.json"));
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        Some(saved.get("stats")?.to_string())
    }
    fn players(&mut self) -> Vec<String> {
        let files = lock(&self.io.files);
        let mut out: Vec<String> = std::fs::read_dir(files.world.join("players/stats"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_lowercase();
                Some(name.strip_suffix(".json")?.to_owned())
            })
            .filter(|u| uuid_ok(u))
            .collect();
        out.extend(files.live.keys().cloned());
        out.sort();
        out.dedup();
        out
    }
    fn names(&mut self) -> Vec<(String, String)> {
        let path = lock(&self.io.files).usercache.clone();
        let Ok(serde_json::Value::Array(entries)) = std::fs::read(path)
            .map_err(drop)
            .and_then(|b| serde_json::from_slice(&b).map_err(drop))
        else {
            return Vec::new();
        };
        entries
            .iter()
            .filter_map(|e| {
                Some((
                    e["uuid"].as_str()?.to_lowercase(),
                    e["name"].as_str()?.to_owned(),
                ))
            })
            .collect()
    }
    fn translations(
        &mut self,
        language: String,
        prefixes: Vec<String>,
    ) -> Option<Vec<(String, String)>> {
        let code_ok = (1..=16).contains(&language.len())
            && language
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'_');
        if !code_ok {
            return None;
        }
        let path = lock(&self.io.files)
            .resources
            .join(format!("assets/minecraft/lang/{language}.json"));
        let all: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        let prefixes = &prefixes[..prefixes.len().min(PREFIXES)];
        Some(
            all.into_iter()
                .filter(|(k, _)| prefixes.iter().any(|p| k.starts_with(p.as_str())))
                .filter_map(|(k, v)| Some((k, v.as_str()?.to_owned())))
                .collect(),
        )
    }
    fn world_name(&mut self) -> String {
        let files = lock(&self.io.files);
        files
            .world
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// The socket's thread: what arrives waits in `inbox` for the plugin, until the plugin is gone.
fn serve_udp(
    socket: Arc<UdpSocket>,
    inbox: Weak<Mutex<VecDeque<Datagram>>>,
    id: &str,
) -> Result<(), String> {
    let id = id.to_owned();
    std::thread::Builder::new()
        .name(format!("plugin {id} udp"))
        .spawn(move || {
            let mut buf = vec![0u8; 65536];
            loop {
                let got = socket.recv_from(&mut buf);
                let Some(inbox) = inbox.upgrade() else {
                    return;
                };
                match got {
                    Ok((n, from)) => {
                        let mut inbox = lock(&inbox);
                        if inbox.len() < UDP_INBOX {
                            inbox.push_back(Datagram {
                                address: from.to_string(),
                                data: buf[..n].to_vec(),
                            });
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(e) => log::debug!("plugin {id}: UDP: {e}"),
                }
            }
        })
        .map(drop)
        .map_err(|e| e.to_string())
}

/// The listener's thread: each connection on a thread of its own (up to
/// [`HTTP_CONNECTIONS`]), until the plugin is gone.
fn serve_http(listener: TcpListener, inbox: Weak<HttpInbox>, id: &str) -> Result<(), String> {
    let id = id.to_owned();
    let open = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name(format!("plugin {id} http"))
        .spawn(move || {
            loop {
                if inbox.strong_count() == 0 {
                    return;
                }
                let stream = match listener.accept() {
                    Ok((s, _)) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    Err(e) => {
                        log::debug!("plugin {id}: HTTP accept: {e}");
                        continue;
                    }
                };
                if open.fetch_add(1, Ordering::SeqCst) >= HTTP_CONNECTIONS {
                    open.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
                let (inbox, count, id) = (inbox.clone(), Arc::clone(&open), id.clone());
                let spawned = std::thread::Builder::new()
                    .name(format!("plugin {id} request"))
                    .spawn(move || {
                        if let Err(e) = serve_request(stream, &inbox) {
                            log::debug!("plugin {id}: HTTP: {e}");
                        }
                        count.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    open.fetch_sub(1, Ordering::SeqCst);
                }
            }
        })
        .map(drop)
        .map_err(|e| e.to_string())
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn respond(s: &mut TcpStream, r: &Response) -> std::io::Result<()> {
    let status = if (100..=599).contains(&r.status) {
        r.status
    } else {
        500
    };
    let cache = match r.max_age {
        0 => "no-store".to_owned(),
        n => format!("public, max-age={n}"),
    };
    let kind: String = r
        .content_type
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect();
    let body = &r.body[..r.body.len().min(MAX_RESPONSE)];
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\
         Cache-Control: {cache}\r\nX-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(body)
}

fn plain(status: u16, text: &str) -> Response {
    Response {
        status,
        content_type: "text/plain; charset=utf-8".into(),
        max_age: 0,
        body: text.as_bytes().to_vec(),
    }
}

/// One connection: its request to the plugin, the plugin's response back.
fn serve_request(mut s: TcpStream, inbox: &Weak<HttpInbox>) -> std::io::Result<()> {
    s.set_nonblocking(false)?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = s.read(&mut chunk)?;
        if n == 0 || buf.len() + n > MAX_HEAD + MAX_BODY {
            return respond(&mut s, &plain(400, "bad request"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (method, path) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    if method.is_empty() || !path.starts_with('/') {
        return respond(&mut s, &plain(400, "bad request"));
    }
    let length = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if length > MAX_BODY {
        return respond(&mut s, &plain(413, "too large"));
    }
    let mut body = buf[head_end..].to_vec();
    while body.len() < length {
        let n = s.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    let request = Request {
        method: method.to_owned(),
        path: path.to_owned(),
        body,
    };
    let (tx, rx) = mpsc::channel();
    {
        let Some(inbox) = inbox.upgrade() else {
            return respond(&mut s, &plain(503, "the plugin stopped"));
        };
        let mut inbox = lock(&inbox);
        if inbox.len() >= HTTP_INBOX {
            return respond(&mut s, &plain(503, "busy"));
        }
        inbox.push_back((request, tx));
    }
    match rx.recv_timeout(HTTP_WAIT) {
        Ok(r) => respond(&mut s, &r),
        Err(_) => respond(&mut s, &plain(503, "the plugin didn't answer")),
    }
}

/// The imports of `plugin-host` (`players` is the server world's own, already linked).
pub(super) fn add_to_linker(
    linker: &mut wasmtime::component::Linker<State>,
) -> wasmtime::Result<()> {
    use wasmtime::component::HasSelf;
    movement::add_to_linker::<State, HasSelf<State>>(linker, |s| s)?;
    channels::add_to_linker::<State, HasSelf<State>>(linker, |s| s)?;
    network::add_to_linker::<State, HasSelf<State>>(linker, |s| s)?;
    random::add_to_linker::<State, HasSelf<State>>(linker, |s| s)?;
    statistics::add_to_linker::<State, HasSelf<State>>(linker, |s| s)
}

impl Mods {
    /// [`Mods::load`] with every mod on and sockets recorded, not opened
    /// (`cargo horizon-mod test`): a sent datagram is an [`Output::UdpSent`], and
    /// [`Mods::udp_arrived`] and [`Mods::http_request`] stand in for the network.
    pub fn load_simulated(paths: &[PathBuf]) -> Mods {
        let mods = Mods {
            simulated_network: true,
            ..Mods::default()
        };
        Mods::load_into(mods, super::Runs::Both, paths, |_| true, |_| false)
    }
    /// The statistics and files capability `statistics` reads.
    pub fn set_server_files(&mut self, files: ServerFiles) {
        *lock(&self.files) = files;
    }
    /// The statistics of the players online now, as JSON by UUID.
    pub fn set_live_statistics(&mut self, live: HashMap<String, String>) {
        lock(&self.files).live = live;
    }
    /// A running plugin reads statistics: the game keeps [`Mods::set_live_statistics`] fresh.
    pub fn wants_statistics(&self) -> bool {
        self.running().any(|r| r.statistics)
    }
    /// A running plugin has a socket: the game calls [`Mods::pump_network`] between ticks.
    pub fn wants_network(&self) -> bool {
        self.running().any(|r| {
            let io = &r.store.data().io;
            io.udp.is_some() || io.http.is_some()
        })
    }
    /// A running plugin listens to `channel`.
    pub fn listens(&self, channel: &str) -> bool {
        self.running().any(|r| r.store.data().io.hears(channel))
    }

    fn running(&self) -> impl Iterator<Item = &super::Running> {
        self.entries.iter().filter_map(|e| e.run.as_ref())
    }

    /// Calls `f` on every running plugin with `plugin-events` that `wants` it.
    fn each_plugin<R>(
        &mut self,
        wants: impl Fn(&super::Running) -> bool,
        mut f: impl FnMut(&PluginHost, &mut Store<State>) -> wasmtime::Result<R>,
        mut done: impl FnMut(R) -> bool,
    ) {
        for i in 0..self.entries.len() {
            let wanted = self.entries[i]
                .run
                .as_ref()
                .is_some_and(|r| r.plugin.is_some() && wants(r));
            if !wanted {
                continue;
            }
            let result = self.call(i, FUEL_EVENT, |run| match &run.plugin {
                Some(p) => f(p, &mut run.store).map(Some),
                None => Ok(None),
            });
            if let Some(Some(r)) = result
                && done(r)
            {
                return;
            }
        }
    }

    /// A payload from `player`: to every plugin listening to its channel. False when none does.
    pub fn payload(&mut self, player: &ServerPlayer, channel: &str, data: &[u8]) -> bool {
        if !self.listens(channel) {
            return false;
        }
        self.each_plugin(
            |r| r.store.data().io.hears(channel),
            |p, s| {
                p.horizon_mods_plugin_events()
                    .call_payload(s, player, channel, data)
            },
            |()| false,
        );
        true
    }
    /// `player`'s swap-hands key: true when a plugin with capability `movement` took it.
    pub fn swap_hands(&mut self, player: &ServerPlayer) -> bool {
        let mut taken = false;
        self.each_plugin(
            |r| r.moves,
            |p, s| p.horizon_mods_plugin_events().call_swap_hands(s, player),
            |t| {
                taken = t;
                t
            },
        );
        taken
    }
    /// A double jump of `player`, to the plugins with capability `movement`.
    pub fn double_jump(&mut self, player: &ServerPlayer) {
        self.each_plugin(
            |r| r.moves,
            |p, s| p.horizon_mods_plugin_events().call_double_jump(s, player),
            |()| false,
        );
    }

    /// What reached the plugins' sockets since the last pump: datagrams to their `udp` hook,
    /// requests to `http`, whose responses go back.
    pub fn pump_network(&mut self) {
        for i in 0..self.entries.len() {
            let Some(run) = &self.entries[i].run else {
                continue;
            };
            let io = &run.store.data().io;
            let datagrams: Vec<Datagram> = io
                .udp
                .as_ref()
                .map(|u| lock(&u.inbox).drain(..).collect())
                .unwrap_or_default();
            let requests: Vec<(Request, mpsc::Sender<Response>)> = io
                .http
                .as_ref()
                .map(|h| lock(h).drain(..).collect())
                .unwrap_or_default();
            if !datagrams.is_empty() {
                self.deliver(i, &datagrams);
            }
            for (request, back) in requests {
                let response = self
                    .answer(i, &request)
                    .unwrap_or_else(|| plain(500, "the plugin failed"));
                let _ = back.send(response);
            }
        }
    }
    fn deliver(&mut self, i: usize, datagrams: &[Datagram]) {
        self.call(i, FUEL_EVENT, |run| match &run.plugin {
            Some(p) => p
                .horizon_mods_plugin_events()
                .call_udp(&mut run.store, datagrams),
            None => Ok(()),
        });
    }
    fn answer(&mut self, i: usize, request: &Request) -> Option<Response> {
        self.call(i, FUEL_EVENT, |run| match &run.plugin {
            Some(p) => p
                .horizon_mods_plugin_events()
                .call_http(&mut run.store, request)
                .map(Some),
            None => Ok(None),
        })
        .flatten()
    }

    /// Datagrams to every plugin with a UDP socket, as if they arrived (`cargo horizon-mod test`).
    pub fn udp_arrived(&mut self, datagrams: &[Datagram]) {
        for i in 0..self.entries.len() {
            let bound = self.entries[i]
                .run
                .as_ref()
                .is_some_and(|r| r.store.data().io.udp.is_some());
            if bound {
                self.deliver(i, datagrams);
            }
        }
    }
    /// A request to the first plugin serving HTTP (`cargo horizon-mod test`); none when none does.
    pub fn http_request(&mut self, request: &Request) -> Option<Response> {
        let i = self.entries.iter().position(|e| {
            e.run
                .as_ref()
                .is_some_and(|r| r.store.data().io.http.is_some())
        })?;
        self.answer(i, request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_and_uuids_are_checked() {
        assert!(channel_ok("voicechat:request_secret"));
        assert!(channel_ok("my_mod:a/b"));
        assert!(!channel_ok("voicechat"));
        assert!(!channel_ok("Voice:x"));
        assert!(!channel_ok("a:"));
        assert!(uuid_ok("65bb83c4-f8ad-4be6-9953-12d6c353d9de"));
        assert!(!uuid_ok("../../../etc/passwd-aaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!uuid_ok("65BB83C4-F8AD-4BE6-9953-12D6C353D9DE"));
        assert!(port_ok(80).is_err() && port_ok(8100).is_ok());
    }
}
