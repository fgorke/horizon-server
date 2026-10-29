//! A server for the Simple Voice Chat mod (henkelmax/simple-voice-chat,
//! compatibility version [`COMPATIBILITY_VERSION`]). A client asks for a
//! secret over the `voicechat:request_secret` channel; with it, it talks to
//! the UDP port over AES-128-GCM. The server only relays the clients' Opus
//! frames: a microphone packet goes on to every other player in the same
//! level within the voice distance, as the mod's `Server.processProximityPacket`
//! does. Groups and recording are left out.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use horizon_protocol::crypto::Aes128;

/// The mod's `Voicechat.COMPATIBILITY_VERSION` this server speaks.
pub const COMPATIBILITY_VERSION: i32 = 20;
/// `ServerConfig`'s defaults: the port, the voice and whisper distances, the
/// MTU and the keep-alive interval in milliseconds.
pub const PORT: u16 = 24454;
pub const DISTANCE: f64 = 48.0;
pub const WHISPER_DISTANCE: f64 = 24.0;
pub const MTU: i32 = 1275;
pub const KEEP_ALIVE_MS: i32 = 1000;
/// The mod's channels the server listens on and sends on.
pub const CHANNELS: [&str; 6] = [
    "voicechat:request_secret",
    "voicechat:secret",
    "voicechat:update_state",
    "voicechat:state",
    "voicechat:states",
    "voicechat:remove_state",
];

const MAGIC: u8 = 0xFF;
const MAX_PACKET: usize = 2048;
const MAX_OPUS: usize = 1275;
/// A client that hasn't answered the keep-alives for this long is dropped
/// (`Server.sendKeepAlives`: ten intervals).
const TIMEOUT: Duration = Duration::from_millis(KEEP_ALIVE_MS as u64 * 10);

// ---- AES-128-GCM (`Secret`: a 12 byte IV in front, a 128 bit tag behind) ----

fn gf_mul(x: u128, y: u128) -> u128 {
    let (mut z, mut v) = (0u128, y);
    for i in 0..128 {
        if (x >> (127 - i)) & 1 == 1 {
            z ^= v;
        }
        let low = v & 1;
        v >>= 1;
        if low == 1 {
            v ^= 0xE1 << 120;
        }
    }
    z
}

struct Gcm {
    aes: Aes128,
    h: u128,
}

impl Gcm {
    fn new(key: &[u8; 16]) -> Self {
        let aes = Aes128::new(key);
        let mut h = [0u8; 16];
        aes.encrypt_block(&mut h);
        Gcm {
            aes,
            h: u128::from_be_bytes(h),
        }
    }

    fn block(&self, iv: &[u8; 12], counter: u32) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[..12].copy_from_slice(iv);
        b[12..].copy_from_slice(&counter.to_be_bytes());
        self.aes.encrypt_block(&mut b);
        b
    }

    fn ctr(&self, iv: &[u8; 12], data: &mut [u8]) {
        for (n, chunk) in data.chunks_mut(16).enumerate() {
            let pad = self.block(iv, n as u32 + 2);
            for (b, p) in chunk.iter_mut().zip(pad) {
                *b ^= p;
            }
        }
    }

    fn tag(&self, iv: &[u8; 12], cipher: &[u8]) -> [u8; 16] {
        let mut x = 0u128;
        for chunk in cipher.chunks(16) {
            let mut b = [0u8; 16];
            b[..chunk.len()].copy_from_slice(chunk);
            x = gf_mul(x ^ u128::from_be_bytes(b), self.h);
        }
        x = gf_mul(x ^ (cipher.len() as u128 * 8), self.h);
        (x ^ u128::from_be_bytes(self.block(iv, 1))).to_be_bytes()
    }

    fn seal(&self, iv: &[u8; 12], plain: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + plain.len() + 16);
        out.extend_from_slice(iv);
        let mut data = plain.to_vec();
        self.ctr(iv, &mut data);
        let tag = self.tag(iv, &data);
        out.extend_from_slice(&data);
        out.extend_from_slice(&tag);
        out
    }

    fn open(&self, payload: &[u8]) -> Option<Vec<u8>> {
        if payload.len() < 28 {
            return None;
        }
        let iv: [u8; 12] = payload[..12].try_into().ok()?;
        let (cipher, tag) = payload[12..].split_at(payload.len() - 28);
        // Not constant time: a forged tag only costs the forger the packet.
        if self.tag(&iv, cipher) != tag {
            return None;
        }
        let mut data = cipher.to_vec();
        self.ctr(&iv, &mut data);
        Some(data)
    }
}

// ---- FriendlyByteBuf ----

/// The mod's `FriendlyByteBuf` writes: big-endian numbers, VarInt lengths.
#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    fn var_int(&mut self, mut v: u32) {
        loop {
            if v & !0x7F == 0 {
                self.0.push(v as u8);
                return;
            }
            self.0.push((v as u8 & 0x7F) | 0x80);
            v >>= 7;
        }
    }
    fn byte(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn bool(&mut self, v: bool) -> &mut Self {
        self.byte(u8::from(v))
    }
    fn int(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn long(&mut self, v: i64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn float(&mut self, v: f32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn double(&mut self, v: f64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn uuid(&mut self, v: u128) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.var_int(v.len() as u32);
        self.0.extend_from_slice(v);
        self
    }
    fn utf(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }
    fn raw(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
    fn byte(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn bool(&mut self) -> Option<bool> {
        self.byte().map(|b| b != 0)
    }
    fn int(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    fn long(&mut self) -> Option<i64> {
        Some(i64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }
    fn uuid(&mut self) -> Option<u128> {
        Some(u128::from_be_bytes(self.take(16)?.try_into().ok()?))
    }
    fn var_int(&mut self) -> Option<u32> {
        let mut v = 0u32;
        for shift in (0..35).step_by(7) {
            let b = self.byte()?;
            v |= u32::from(b & 0x7F) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    fn bytes(&mut self, max: usize) -> Option<&[u8]> {
        let n = self.var_int()? as usize;
        if n > max {
            return None;
        }
        self.take(n)
    }
}

// ---- The channel payloads ----

/// `RequestSecretPacket`: the client's compatibility version.
pub fn parse_request_secret(data: &[u8]) -> Option<i32> {
    Reader(data).int()
}

/// `UpdateStatePacket`: whether the client turned its voice chat off.
pub fn parse_update_state(data: &[u8]) -> Option<bool> {
    Reader(data).bool()
}

/// `SecretPacket`: the secret, the UDP port, the player, the codec (VOIP),
/// the MTU, the distance, the keep-alive, groups off, no voice host (the
/// client uses the server's address) and no recording.
pub fn secret_payload(secret: &[u8; 16], port: u16, player: u128) -> Vec<u8> {
    let mut w = Writer::default();
    w.raw(secret)
        .int(i32::from(port))
        .uuid(player)
        .byte(0)
        .int(MTU)
        .double(DISTANCE)
        .int(KEEP_ALIVE_MS)
        .bool(false)
        .utf("")
        .bool(false);
    w.0
}

/// A `PlayerState`: off, gone, the player, their name, no group.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerState {
    pub uuid: u128,
    pub name: String,
    pub disabled: bool,
    pub disconnected: bool,
}

impl PlayerState {
    fn write(&self, w: &mut Writer) {
        w.bool(self.disabled)
            .bool(self.disconnected)
            .uuid(self.uuid)
            .utf(&self.name)
            .bool(false);
    }
}

/// `PlayerStatePacket`.
pub fn state_payload(state: &PlayerState) -> Vec<u8> {
    let mut w = Writer::default();
    state.write(&mut w);
    w.0
}

/// `PlayerStatesPacket`.
pub fn states_payload(states: &[PlayerState]) -> Vec<u8> {
    let mut w = Writer::default();
    w.int(states.len() as i32);
    for s in states {
        s.write(&mut w);
    }
    w.0
}

/// `RemovePlayerStatePacket`.
pub fn remove_state_payload(uuid: u128) -> Vec<u8> {
    let mut w = Writer::default();
    w.uuid(uuid);
    w.0
}

/// `minecraft:register`: the channels, NUL separated.
pub fn register_payload() -> Vec<u8> {
    CHANNELS.join("\0").into_bytes()
}

// ---- The UDP server ----

/// What the server knows of a player: their secret, the address they
/// authenticated from, and where they are (from the game, every tick).
struct Client {
    gcm: Gcm,
    secret: [u8; 16],
    address: Option<SocketAddr>,
    checked: bool,
    last_answer: Instant,
    disabled: bool,
    dimension: String,
    position: [f64; 3],
    listening: bool,
}

#[derive(Default)]
struct Clients(HashMap<u128, Client>);

/// The voice chat server: a UDP socket and the thread that serves it.
pub struct VoiceServer {
    clients: Arc<Mutex<Clients>>,
    port: u16,
}

impl VoiceServer {
    /// Binds the port and starts the thread.
    pub fn start(port: u16) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", port))?;
        socket.set_read_timeout(Some(Duration::from_millis(10)))?;
        let port = socket.local_addr()?.port();
        let clients = Arc::new(Mutex::new(Clients::default()));
        let shared = Arc::clone(&clients);
        std::thread::Builder::new()
            .name("voice chat".into())
            .spawn(move || serve(&socket, &shared))?;
        log::info!("Voice chat server started at port {port}");
        Ok(VoiceServer { clients, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Clients> {
        self.clients.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A new secret for the player (`Server.generateNewSecret`); a reconnect
    /// replaces the old one.
    pub fn add(&self, player: u128) -> std::io::Result<[u8; 16]> {
        let mut secret = [0u8; 16];
        horizon_protocol::crypto::random_bytes(&mut secret)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        self.lock().0.insert(
            player,
            Client {
                gcm: Gcm::new(&secret),
                secret,
                address: None,
                checked: false,
                last_answer: Instant::now(),
                disabled: false,
                dimension: String::new(),
                position: [0.0; 3],
                listening: false,
            },
        );
        Ok(secret)
    }

    pub fn remove(&self, player: u128) {
        self.lock().0.remove(&player);
    }

    pub fn has(&self, player: u128) -> bool {
        self.lock().0.contains_key(&player)
    }

    pub fn set_disabled(&self, player: u128, disabled: bool) {
        if let Some(c) = self.lock().0.get_mut(&player) {
            c.disabled = disabled;
        }
    }

    /// The player's level and position, and whether they hear the others
    /// (alive and not a spectator).
    pub fn place(&self, player: u128, dimension: &str, position: [f64; 3], listening: bool) {
        if let Some(c) = self.lock().0.get_mut(&player) {
            if c.dimension != dimension {
                c.dimension = dimension.to_owned();
            }
            c.position = position;
            c.listening = listening;
        }
    }

    /// The players with a secret and whether they turned voice chat off.
    pub fn players(&self) -> Vec<(u128, bool)> {
        self.lock().0.iter().map(|(u, c)| (*u, c.disabled)).collect()
    }
}

fn serve(socket: &UdpSocket, clients: &Mutex<Clients>) {
    let mut buf = [0u8; MAX_PACKET];
    let mut last_keep_alive = Instant::now();
    loop {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                let mut clients = clients.lock().unwrap_or_else(PoisonError::into_inner);
                for (to, packet) in handle(&mut clients, &buf[..n], from) {
                    if let Err(e) = socket.send_to(&packet, to) {
                        log::debug!("voice chat: send to {to}: {e}");
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => log::debug!("voice chat: receive: {e}"),
        }
        if last_keep_alive.elapsed() >= Duration::from_millis(KEEP_ALIVE_MS as u64) {
            last_keep_alive = Instant::now();
            let mut clients = clients.lock().unwrap_or_else(PoisonError::into_inner);
            for (to, packet) in keep_alives(&mut clients) {
                if let Err(e) = socket.send_to(&packet, to) {
                    log::debug!("voice chat: keep-alive to {to}: {e}");
                }
            }
        }
    }
}

/// `NetworkMessage.writeServer`: the magic byte and the encrypted packet.
fn message(gcm: &Gcm, kind: u8, body: &[u8]) -> Option<Vec<u8>> {
    let mut iv = [0u8; 12];
    horizon_protocol::crypto::random_bytes(&mut iv).ok()?;
    let mut plain = Vec::with_capacity(1 + body.len());
    plain.push(kind);
    plain.extend_from_slice(body);
    let mut w = Writer::default();
    w.byte(MAGIC).bytes(&gcm.seal(&iv, &plain));
    Some(w.0)
}

/// `Server.sendKeepAlives`: the clients that stopped answering are dropped
/// back to unauthenticated, the others get a keep-alive.
fn keep_alives(clients: &mut Clients) -> Vec<(SocketAddr, Vec<u8>)> {
    let mut out = Vec::new();
    for c in clients.0.values_mut() {
        let Some(to) = c.address.filter(|_| c.checked) else {
            continue;
        };
        if c.last_answer.elapsed() >= TIMEOUT {
            c.address = None;
            c.checked = false;
            continue;
        }
        out.extend(message(&c.gcm, 0x8, &[]).map(|m| (to, m)));
    }
    out
}

/// One UDP packet from a client: the packets it calls for.
fn handle(clients: &mut Clients, data: &[u8], from: SocketAddr) -> Vec<(SocketAddr, Vec<u8>)> {
    let mut r = Reader(data);
    if r.byte() != Some(MAGIC) {
        return Vec::new();
    }
    let Some(player) = r.uuid() else {
        return Vec::new();
    };
    let Some(sender) = clients.0.get_mut(&player) else {
        return Vec::new();
    };
    let Some(plain) = r.bytes(MAX_PACKET).and_then(|p| sender.gcm.open(p)) else {
        return Vec::new();
    };
    let mut p = Reader(&plain);
    let reply = |c: &Client, kind: u8| message(&c.gcm, kind, &[]).map(|m| (from, m));
    match p.byte() {
        // AuthenticatePacket: the player and the secret again.
        Some(0x5) => {
            if p.uuid() == Some(player) && p.take(16) == Some(&sender.secret[..]) {
                sender.address = Some(from);
                sender.last_answer = Instant::now();
                return reply(sender, 0x6).into_iter().collect();
            }
            Vec::new()
        }
        // ConnectionCheckPacket: the client hears the server.
        Some(0x9) if sender.address == Some(from) => {
            sender.checked = true;
            sender.last_answer = Instant::now();
            reply(sender, 0xA).into_iter().collect()
        }
        Some(0x8) if sender.address == Some(from) => {
            sender.last_answer = Instant::now();
            Vec::new()
        }
        Some(0x1) if sender.address == Some(from) && sender.checked => {
            let (Some(opus), Some(sequence), Some(whispering)) =
                (p.bytes(MAX_OPUS).map(<[u8]>::to_vec), p.long(), p.bool())
            else {
                return Vec::new();
            };
            sender.last_answer = Instant::now();
            proximity(clients, player, &opus, sequence, whispering)
        }
        _ => Vec::new(),
    }
}

/// `Server.processProximityPacket`: a `PlayerSoundPacket` for every other
/// listening player in the sender's level within the distance and a block.
fn proximity(
    clients: &Clients,
    player: u128,
    opus: &[u8],
    sequence: i64,
    whispering: bool,
) -> Vec<(SocketAddr, Vec<u8>)> {
    let Some(sender) = clients.0.get(&player) else {
        return Vec::new();
    };
    let distance = if whispering { WHISPER_DISTANCE } else { DISTANCE };
    let range = distance + 1.0;
    let mut body = Writer::default();
    body.uuid(player)
        .uuid(player)
        .bytes(opus)
        .long(sequence)
        .float(distance as f32)
        .byte(u8::from(whispering));
    let mut out = Vec::new();
    for (uuid, c) in &clients.0 {
        if *uuid == player || c.disabled || !c.listening || !c.checked {
            continue;
        }
        let Some(to) = c.address else {
            continue;
        };
        let d2: f64 = (0..3).map(|k| (c.position[k] - sender.position[k]).powi(2)).sum();
        if c.dimension != sender.dimension || d2 > range * range {
            continue;
        }
        out.extend(message(&c.gcm, 0x2, &body.0).map(|m| (to, m)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn gcm_matches_the_published_test_cases() {
        // McGrew & Viega, "The Galois/Counter Mode of Operation", test cases 2 and 3.
        let gcm = Gcm::new(&[0; 16]);
        let sealed = gcm.seal(&[0; 12], &[0; 16]);
        assert_eq!(sealed[12..], hex("0388dace60b6a392f328c2b971b2fe78ab6e47d42cec13bdf53a67b21257bddf")[..]);
        let key: [u8; 16] = hex("feffe9928665731c6d6a8f9467308308").try_into().unwrap();
        let iv: [u8; 12] = hex("cafebabefacedbaddecaf888").try_into().unwrap();
        let plain = hex(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72\
             1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255",
        );
        let gcm = Gcm::new(&key);
        let sealed = gcm.seal(&iv, &plain);
        let want = hex(
            "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e\
             21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091473f5985\
             4d5c2af327cd64a62cf35abd2ba6fab4",
        );
        assert_eq!(sealed[12..], want[..]);
        assert_eq!(gcm.open(&sealed), Some(plain));
        let mut forged = sealed.clone();
        forged[20] ^= 1;
        assert_eq!(gcm.open(&forged), None);
    }

    /// A client's packet: magic, player, the encrypted type and body.
    fn client_packet(player: u128, secret: &[u8; 16], kind: u8, body: &[u8]) -> Vec<u8> {
        let gcm = Gcm::new(secret);
        let mut plain = vec![kind];
        plain.extend_from_slice(body);
        let mut w = Writer::default();
        w.byte(MAGIC).uuid(player).bytes(&gcm.seal(&[7; 12], &plain));
        w.0
    }

    fn server_packet(secret: &[u8; 16], data: &[u8]) -> Vec<u8> {
        let mut r = Reader(data);
        assert_eq!(r.byte(), Some(MAGIC));
        Gcm::new(secret).open(r.bytes(MAX_PACKET).unwrap()).unwrap()
    }

    fn join(clients: &mut Clients, player: u128, secret: [u8; 16], from: SocketAddr, at: [f64; 3]) {
        clients.0.insert(
            player,
            Client {
                gcm: Gcm::new(&secret),
                secret,
                address: None,
                checked: false,
                last_answer: Instant::now(),
                disabled: false,
                dimension: "minecraft:overworld".into(),
                position: at,
                listening: true,
            },
        );
        let mut auth = Writer::default();
        auth.uuid(player).raw(&secret);
        let out = handle(clients, &client_packet(player, &secret, 0x5, &auth.0), from);
        assert_eq!(server_packet(&secret, &out[0].1), vec![0x6]);
        let out = handle(clients, &client_packet(player, &secret, 0x9, &[]), from);
        assert_eq!(server_packet(&secret, &out[0].1), vec![0xA]);
    }

    #[test]
    fn a_voice_reaches_the_players_in_range_only() {
        let mut clients = Clients::default();
        let addr = |p: u16| SocketAddr::from(([127, 0, 0, 1], p));
        let (a, b, c) = (1u128, 2u128, 3u128);
        join(&mut clients, a, [1; 16], addr(1), [0.0, 64.0, 0.0]);
        join(&mut clients, b, [2; 16], addr(2), [30.0, 64.0, 0.0]);
        join(&mut clients, c, [3; 16], addr(3), [100.0, 64.0, 0.0]);
        let mut mic = Writer::default();
        mic.bytes(&[9, 9, 9]).long(42).bool(false);
        let out = handle(&mut clients, &client_packet(a, &[1; 16], 0x1, &mic.0), addr(1));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, addr(2));
        let got = server_packet(&[2; 16], &out[0].1);
        let mut r = Reader(&got);
        assert_eq!(r.byte(), Some(0x2));
        assert_eq!((r.uuid(), r.uuid()), (Some(a), Some(a)));
        assert_eq!(r.bytes(MAX_OPUS), Some(&[9, 9, 9][..]));
        assert_eq!(r.long(), Some(42));
        assert_eq!(r.take(4), Some(&48f32.to_be_bytes()[..]));
        assert_eq!(r.byte(), Some(0));
        // Whispering reaches 24 blocks: b at 30 is out.
        let mut mic = Writer::default();
        mic.bytes(&[1]).long(43).bool(true);
        let out = handle(&mut clients, &client_packet(a, &[1; 16], 0x1, &mic.0), addr(1));
        assert!(out.is_empty());
        // A packet from another address, or with the wrong secret, is dropped.
        let mut mic = Writer::default();
        mic.bytes(&[1]).long(44).bool(false);
        assert!(handle(&mut clients, &client_packet(a, &[1; 16], 0x1, &mic.0), addr(9)).is_empty());
        assert!(handle(&mut clients, &client_packet(a, &[5; 16], 0x1, &mic.0), addr(1)).is_empty());
    }

    #[test]
    fn the_secret_payload_is_laid_out_as_the_mod_reads_it() {
        let p = secret_payload(&[4; 16], 24454, 0xAB);
        let mut r = Reader(&p);
        assert_eq!(r.take(16), Some(&[4; 16][..]));
        assert_eq!(r.int(), Some(24454));
        assert_eq!(r.uuid(), Some(0xAB));
        assert_eq!(r.byte(), Some(0));
        assert_eq!(r.int(), Some(MTU));
        assert_eq!(r.take(8), Some(&DISTANCE.to_be_bytes()[..]));
        assert_eq!(r.int(), Some(KEEP_ALIVE_MS));
        assert_eq!(r.bool(), Some(false));
        assert_eq!(r.bytes(10), Some(&[][..]));
        assert_eq!(r.bool(), Some(false));
        assert!(r.0.is_empty());
        assert_eq!(parse_request_secret(&20i32.to_be_bytes()), Some(20));
    }
}
