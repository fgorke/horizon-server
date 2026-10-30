//! The Simple Voice Chat protocol (henkelmax/simple-voice-chat, compatibility version
//! [`COMPATIBILITY_VERSION`]): the channel payloads, AES-128-GCM, and the relay of the
//! clients' UDP packets. A microphone packet goes on to every other player in the same
//! level within the voice distance, as the mod's `Server.processProximityPacket` does, and
//! to the other members of the sender's group wherever they are (`processGroupPacket`).
//! Recording is left out.

use std::collections::HashMap;

use crate::aes::Aes128;

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
pub const CHANNELS: [&str; 12] = [
    "voicechat:request_secret",
    "voicechat:secret",
    "voicechat:update_state",
    "voicechat:state",
    "voicechat:states",
    "voicechat:remove_state",
    "voicechat:create_group",
    "voicechat:set_group",
    "voicechat:leave_group",
    "voicechat:add_group",
    "voicechat:remove_group",
    "voicechat:joined_group",
];
/// `Voicechat.MAX_GROUP_NAME_LENGTH`, in characters.
const MAX_GROUP_NAME: usize = 24;
/// `JoinGroupPacket`'s password length limit, in bytes.
const MAX_PASSWORD: usize = 512;

const MAGIC: u8 = 0xFF;
const MAX_PACKET: usize = 2048;
const MAX_OPUS: usize = 1275;
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
    fn short(&mut self, v: u16) -> &mut Self {
        self.raw(&v.to_be_bytes())
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
    fn utf(&mut self, max: usize) -> Option<String> {
        String::from_utf8(self.bytes(max * 3)?.to_vec()).ok()
    }
    fn short(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }
}

/// `Voicechat.GROUP_REGEX`: 1 to 24 characters, none a control character,
/// the first no whitespace either.
fn valid_group_text(s: &str) -> bool {
    let n = s.chars().count();
    (1..=MAX_GROUP_NAME).contains(&n)
        && !s.chars().any(char::is_control)
        && !s.starts_with(char::is_whitespace)
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
/// the MTU, the distance, the keep-alive, groups on, no voice host (the
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
        .bool(true)
        .utf("")
        .bool(false);
    w.0
}

/// A `PlayerState`: off, gone, the player, their name, their group.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerState {
    pub uuid: u128,
    pub name: String,
    pub disabled: bool,
    pub disconnected: bool,
    pub group: Option<u128>,
}

/// `Group.Type`: NORMAL (members hear only each other and the players
/// near them), OPEN (the players near them hear them too) and ISOLATED
/// (members hear no one outside the group).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupKind {
    Normal,
    Open,
    Isolated,
}

impl GroupKind {
    /// `GroupImpl.TypeImpl.fromInt`.
    fn from_short(v: u16) -> Self {
        match v {
            1 => GroupKind::Open,
            2 => GroupKind::Isolated,
            _ => GroupKind::Normal,
        }
    }
    fn to_short(self) -> u16 {
        match self {
            GroupKind::Normal => 0,
            GroupKind::Open => 1,
            GroupKind::Isolated => 2,
        }
    }
}

/// A voice chat group (`Group`): its name, password and type.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub name: String,
    pub password: Option<String>,
    pub kind: GroupKind,
}

/// `CreateGroupPacket`: the name, the password if any, the type; `None`
/// unless both name and password pass `GROUP_REGEX`.
pub fn parse_create_group(data: &[u8]) -> Option<Group> {
    let mut r = Reader(data);
    let name = r.utf(MAX_GROUP_NAME)?;
    let password = if r.bool()? {
        Some(r.utf(MAX_GROUP_NAME)?)
    } else {
        None
    };
    let kind = GroupKind::from_short(r.short()?);
    if !valid_group_text(&name) || password.as_deref().is_some_and(|p| !valid_group_text(p)) {
        return None;
    }
    Some(Group {
        name,
        password,
        kind,
    })
}

/// `JoinGroupPacket` (`voicechat:set_group`): the group and the password.
pub fn parse_join_group(data: &[u8]) -> Option<(u128, Option<String>)> {
    let mut r = Reader(data);
    let group = r.uuid()?;
    let password = if r.bool()? {
        Some(r.utf(MAX_PASSWORD)?)
    } else {
        None
    };
    Some((group, password))
}

/// `AddGroupPacket`: a `ClientGroup` (id, name, has a password, not
/// persistent, not hidden, type).
pub fn add_group_payload(id: u128, group: &Group) -> Vec<u8> {
    let mut w = Writer::default();
    w.uuid(id)
        .utf(&group.name)
        .bool(group.password.is_some())
        .bool(false)
        .bool(false)
        .short(group.kind.to_short());
    w.0
}

/// `RemoveGroupPacket`.
pub fn remove_group_payload(id: u128) -> Vec<u8> {
    let mut w = Writer::default();
    w.uuid(id);
    w.0
}

/// `JoinedGroupPacket`: the group joined (none: left, or refused) and
/// whether the password was wrong.
pub fn joined_group_payload(group: Option<u128>, wrong_password: bool) -> Vec<u8> {
    let mut w = Writer::default();
    w.bool(group.is_some());
    if let Some(g) = group {
        w.uuid(g);
    }
    w.bool(wrong_password);
    w.0
}

impl PlayerState {
    fn write(&self, w: &mut Writer) {
        w.bool(self.disabled)
            .bool(self.disconnected)
            .uuid(self.uuid)
            .utf(&self.name)
            .bool(self.group.is_some());
        if let Some(g) = self.group {
            w.uuid(g);
        }
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

/// A version 4 UUID from 16 random bytes, as `UUID.randomUUID`.
pub fn random_uuid(bytes: &[u8]) -> Option<u128> {
    let mut id: [u8; 16] = bytes.try_into().ok()?;
    id[6] = (id[6] & 0x0F) | 0x40;
    id[8] = (id[8] & 0x3F) | 0x80;
    Some(u128::from_be_bytes(id))
}

/// `minecraft:register`: the channels, NUL separated.
pub fn register_payload() -> Vec<u8> {
    CHANNELS.join("\0").into_bytes()
}

// ---- The relay ----

/// Ticks between keep-alives, and without an answer after which a client is dropped back to
/// unauthenticated (`Server.sendKeepAlives`: ten intervals).
pub const KEEP_ALIVE_TICKS: u64 = KEEP_ALIVE_MS as u64 / 50;
const TIMEOUT: u64 = KEEP_ALIVE_TICKS * 10;

/// Fresh 12 byte IVs: random, since GCM must never use one twice under a key.
pub type Ivs<'a> = &'a mut dyn FnMut() -> Option<[u8; 12]>;

/// What the relay knows of a player: their secret, the address they authenticated from, and
/// where they are (from the game, every tick).
struct Client {
    gcm: Gcm,
    secret: [u8; 16],
    address: Option<String>,
    checked: bool,
    /// The tick of their last answer.
    last_answer: u64,
    disabled: bool,
    dimension: String,
    position: [f64; 3],
    listening: bool,
    group: Option<u128>,
}

/// The players by UUID, the groups by id, and the tick now.
#[derive(Default)]
pub struct Clients(HashMap<u128, Client>, HashMap<u128, Group>, u64);

impl Clients {
    pub fn set_tick(&mut self, tick: u64) {
        self.2 = tick;
    }

    /// The player's new secret (`Server.generateNewSecret`); a reconnect replaces the old one.
    pub fn add(&mut self, player: u128, secret: [u8; 16]) {
        self.0.insert(
            player,
            Client {
                gcm: Gcm::new(&secret),
                secret,
                address: None,
                checked: false,
                last_answer: self.2,
                disabled: false,
                dimension: String::new(),
                position: [0.0; 3],
                listening: false,
                group: None,
            },
        );
    }

    /// `ServerGroupManager.addGroup`: a new group with id `id`, the player in it. False when
    /// the player has no secret.
    pub fn create_group(&mut self, player: u128, id: u128, group: Group) -> bool {
        let Some(c) = self.0.get_mut(&player) else {
            return false;
        };
        c.group = Some(id);
        self.1.insert(id, group);
        true
    }

    /// `ServerGroupManager.joinGroup`: `Err(true)` for a wrong password, `Err(false)` for no
    /// such group.
    pub fn join_group(
        &mut self,
        player: u128,
        id: u128,
        password: Option<&str>,
    ) -> Result<(), bool> {
        let group = self.1.get(&id).ok_or(false)?;
        if group.password.is_some() && group.password.as_deref() != password {
            return Err(true);
        }
        self.0.get_mut(&player).ok_or(false)?.group = Some(id);
        Ok(())
    }

    /// `ServerGroupManager.leaveGroup`.
    pub fn leave_group(&mut self, player: u128) {
        if let Some(c) = self.0.get_mut(&player) {
            c.group = None;
        }
    }

    /// `ServerGroupManager.cleanupGroups`: the groups nobody is in are removed; their ids.
    pub fn cleanup_groups(&mut self) -> Vec<u128> {
        let used: Vec<u128> = self.0.values().filter_map(|c| c.group).collect();
        let empty: Vec<u128> = self
            .1
            .keys()
            .copied()
            .filter(|g| !used.contains(g))
            .collect();
        for id in &empty {
            self.1.remove(id);
        }
        empty
    }

    /// The groups there are.
    pub fn groups(&self) -> Vec<(u128, Group)> {
        self.1.iter().map(|(id, g)| (*id, g.clone())).collect()
    }

    pub fn remove(&mut self, player: u128) {
        self.0.remove(&player);
    }

    pub fn has(&self, player: u128) -> bool {
        self.0.contains_key(&player)
    }

    pub fn set_disabled(&mut self, player: u128, disabled: bool) {
        if let Some(c) = self.0.get_mut(&player) {
            c.disabled = disabled;
        }
    }

    /// The player's level and position, and whether they hear the others (alive and not a
    /// spectator).
    pub fn place(&mut self, player: u128, dimension: &str, position: [f64; 3], listening: bool) {
        if let Some(c) = self.0.get_mut(&player) {
            if c.dimension != dimension {
                c.dimension = dimension.to_owned();
            }
            c.position = position;
            c.listening = listening;
        }
    }

    /// Whether the player turned voice chat off, and their group; none without a secret.
    pub fn state(&self, player: u128) -> Option<(bool, Option<u128>)> {
        self.0.get(&player).map(|c| (c.disabled, c.group))
    }
}

/// `NetworkMessage.writeServer`: the magic byte and the encrypted packet.
fn message(gcm: &Gcm, kind: u8, body: &[u8], ivs: Ivs) -> Option<Vec<u8>> {
    let iv = ivs()?;
    let mut plain = Vec::with_capacity(1 + body.len());
    plain.push(kind);
    plain.extend_from_slice(body);
    let mut w = Writer::default();
    w.byte(MAGIC).bytes(&gcm.seal(&iv, &plain));
    Some(w.0)
}

/// `Server.sendKeepAlives`: the clients that stopped answering are dropped back to
/// unauthenticated, the others get a keep-alive.
pub fn keep_alives(clients: &mut Clients, ivs: Ivs) -> Vec<(String, Vec<u8>)> {
    let now = clients.2;
    let mut out = Vec::new();
    for c in clients.0.values_mut() {
        let Some(to) = c.address.clone().filter(|_| c.checked) else {
            continue;
        };
        if now.saturating_sub(c.last_answer) >= TIMEOUT {
            c.address = None;
            c.checked = false;
            continue;
        }
        out.extend(message(&c.gcm, 0x8, &[], ivs).map(|m| (to, m)));
    }
    out
}

/// One UDP packet from a client: the packets it calls for.
pub fn handle(clients: &mut Clients, data: &[u8], from: &str, ivs: Ivs) -> Vec<(String, Vec<u8>)> {
    let now = clients.2;
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
    let from_sender = sender.address.as_deref() == Some(from);
    match p.byte() {
        // AuthenticatePacket: the player and the secret again.
        Some(0x5) => {
            if p.uuid() == Some(player) && p.take(16) == Some(&sender.secret[..]) {
                sender.address = Some(from.to_owned());
                sender.last_answer = now;
                return message(&sender.gcm, 0x6, &[], ivs)
                    .map(|m| (from.to_owned(), m))
                    .into_iter()
                    .collect();
            }
            Vec::new()
        }
        // ConnectionCheckPacket: the client hears the server.
        Some(0x9) if from_sender => {
            sender.checked = true;
            sender.last_answer = now;
            message(&sender.gcm, 0xA, &[], ivs)
                .map(|m| (from.to_owned(), m))
                .into_iter()
                .collect()
        }
        Some(0x8) if from_sender => {
            sender.last_answer = now;
            Vec::new()
        }
        Some(0x1) if from_sender && sender.checked => {
            let (Some(opus), Some(sequence), Some(whispering)) =
                (p.bytes(MAX_OPUS).map(<[u8]>::to_vec), p.long(), p.bool())
            else {
                return Vec::new();
            };
            sender.last_answer = now;
            let group = sender.group;
            // Server.processMicPacket: the group, then the players around unless the group
            // keeps to itself.
            let Some(id) = group else {
                return proximity(clients, player, &opus, sequence, whispering, ivs);
            };
            let mut out = group_voice(clients, player, id, &opus, sequence, ivs);
            if clients.1.get(&id).is_none_or(|g| g.kind == GroupKind::Open) {
                out.extend(proximity(clients, player, &opus, sequence, whispering, ivs));
            }
            out
        }
        _ => Vec::new(),
    }
}

/// `Server.processGroupPacket`: a `GroupSoundPacket` for every other member of the group,
/// wherever they are.
fn group_voice(
    clients: &Clients,
    player: u128,
    group: u128,
    opus: &[u8],
    sequence: i64,
    ivs: Ivs,
) -> Vec<(String, Vec<u8>)> {
    let mut body = Writer::default();
    body.uuid(player)
        .uuid(player)
        .bytes(opus)
        .long(sequence)
        .byte(0);
    let mut out = Vec::new();
    for (uuid, c) in &clients.0 {
        if *uuid == player || c.group != Some(group) || c.disabled || !c.checked {
            continue;
        }
        if let (Some(to), Some(m)) = (&c.address, message(&c.gcm, 0x3, &body.0, ivs)) {
            out.push((to.clone(), m));
        }
    }
    out
}

/// `Server.processProximityPacket`: a `PlayerSoundPacket` for every other listening player in
/// the sender's level within the distance and a block.
fn proximity(
    clients: &Clients,
    player: u128,
    opus: &[u8],
    sequence: i64,
    whispering: bool,
    ivs: Ivs,
) -> Vec<(String, Vec<u8>)> {
    let Some(sender) = clients.0.get(&player) else {
        return Vec::new();
    };
    let distance = if whispering {
        WHISPER_DISTANCE
    } else {
        DISTANCE
    };
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
        // Server.broadcast: the sender's group heard it already, and an isolated group's
        // members hear no one else.
        if c.group.is_some()
            && (c.group == sender.group
                || c.group
                    .and_then(|g| clients.1.get(&g))
                    .is_some_and(|g| g.kind == GroupKind::Isolated))
        {
            continue;
        }
        let Some(to) = &c.address else {
            continue;
        };
        let d2: f64 = (0..3)
            .map(|k| (c.position[k] - sender.position[k]).powi(2))
            .sum();
        if c.dimension != sender.dimension || d2 > range * range {
            continue;
        }
        out.extend(message(&c.gcm, 0x2, &body.0, ivs).map(|m| (to.clone(), m)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(clients: &mut Clients, data: &[u8], from: &str) -> Vec<(String, Vec<u8>)> {
        super::handle(clients, data, from, &mut || Some([0; 12]))
    }

    fn addr(p: u16) -> &'static str {
        Box::leak(format!("127.0.0.1:{p}").into_boxed_str())
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn gcm_matches_the_published_test_cases() {
        // McGrew & Viega, "The Galois/Counter Mode of Operation", test cases 2 and 3.
        let gcm = Gcm::new(&[0; 16]);
        let sealed = gcm.seal(&[0; 12], &[0; 16]);
        assert_eq!(
            sealed[12..],
            hex("0388dace60b6a392f328c2b971b2fe78ab6e47d42cec13bdf53a67b21257bddf")[..]
        );
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
        w.byte(MAGIC)
            .uuid(player)
            .bytes(&gcm.seal(&[7; 12], &plain));
        w.0
    }

    fn server_packet(secret: &[u8; 16], data: &[u8]) -> Vec<u8> {
        let mut r = Reader(data);
        assert_eq!(r.byte(), Some(MAGIC));
        Gcm::new(secret).open(r.bytes(MAX_PACKET).unwrap()).unwrap()
    }

    fn join(clients: &mut Clients, player: u128, secret: [u8; 16], from: &str, at: [f64; 3]) {
        clients.0.insert(
            player,
            Client {
                gcm: Gcm::new(&secret),
                secret,
                address: None,
                checked: false,
                last_answer: 0,
                disabled: false,
                dimension: "minecraft:overworld".into(),
                position: at,
                listening: true,
                group: None,
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
        let (a, b, c) = (1u128, 2u128, 3u128);
        join(&mut clients, a, [1; 16], addr(1), [0.0, 64.0, 0.0]);
        join(&mut clients, b, [2; 16], addr(2), [30.0, 64.0, 0.0]);
        join(&mut clients, c, [3; 16], addr(3), [100.0, 64.0, 0.0]);
        let mut mic = Writer::default();
        mic.bytes(&[9, 9, 9]).long(42).bool(false);
        let out = handle(
            &mut clients,
            &client_packet(a, &[1; 16], 0x1, &mic.0),
            addr(1),
        );
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
        let out = handle(
            &mut clients,
            &client_packet(a, &[1; 16], 0x1, &mic.0),
            addr(1),
        );
        assert!(out.is_empty());
        // A packet from another address, or with the wrong secret, is dropped.
        let mut mic = Writer::default();
        mic.bytes(&[1]).long(44).bool(false);
        assert!(
            handle(
                &mut clients,
                &client_packet(a, &[1; 16], 0x1, &mic.0),
                addr(9)
            )
            .is_empty()
        );
        assert!(
            handle(
                &mut clients,
                &client_packet(a, &[5; 16], 0x1, &mic.0),
                addr(1)
            )
            .is_empty()
        );
    }

    #[test]
    fn a_group_hears_its_members_anywhere_and_isolated_ones_nothing_else() {
        let mut clients = Clients::default();
        let (a, b, c) = (1u128, 2u128, 3u128);
        join(&mut clients, a, [1; 16], addr(1), [0.0, 64.0, 0.0]);
        join(&mut clients, b, [2; 16], addr(2), [5000.0, 64.0, 0.0]);
        join(&mut clients, c, [3; 16], addr(3), [10.0, 64.0, 0.0]);
        let mut group = |kind| {
            clients.1.insert(
                7,
                Group {
                    name: "g".into(),
                    password: None,
                    kind,
                },
            );
            clients.0.get_mut(&a).unwrap().group = Some(7);
            clients.0.get_mut(&b).unwrap().group = Some(7);
            let mut mic = Writer::default();
            mic.bytes(&[1]).long(1).bool(false);
            let mut to: Vec<String> = handle(
                &mut clients,
                &client_packet(a, &[1; 16], 0x1, &mic.0),
                addr(1),
            )
            .into_iter()
            .map(|o| o.0)
            .collect();
            to.sort();
            to
        };
        // Normal: b far away hears a, c next to a doesn't.
        assert_eq!(group(GroupKind::Normal), vec![addr(2)]);
        // Open: c nearby too.
        assert_eq!(group(GroupKind::Open), vec![addr(2), addr(3)]);
        // c talking: an isolated group's a doesn't hear it.
        clients.1.insert(
            7,
            Group {
                name: "g".into(),
                password: None,
                kind: GroupKind::Isolated,
            },
        );
        let mut mic = Writer::default();
        mic.bytes(&[1]).long(2).bool(false);
        assert!(
            handle(
                &mut clients,
                &client_packet(c, &[3; 16], 0x1, &mic.0),
                addr(3)
            )
            .is_empty()
        );
    }

    #[test]
    fn group_packets_are_read_and_laid_out_as_the_mod_does() {
        let mut w = Writer::default();
        w.utf("Bau").bool(true).utf("pw").short(1);
        assert_eq!(
            parse_create_group(&w.0),
            Some(Group {
                name: "Bau".into(),
                password: Some("pw".into()),
                kind: GroupKind::Open
            })
        );
        let mut w = Writer::default();
        w.utf(" leer").bool(false).short(0);
        assert_eq!(parse_create_group(&w.0), None);
        let mut w = Writer::default();
        w.uuid(9).bool(false);
        assert_eq!(parse_join_group(&w.0), Some((9, None)));
        let g = Group {
            name: "Bau".into(),
            password: None,
            kind: GroupKind::Isolated,
        };
        let p = add_group_payload(9, &g);
        let mut r = Reader(&p);
        assert_eq!(r.uuid(), Some(9));
        assert_eq!(r.utf(24).as_deref(), Some("Bau"));
        assert_eq!(
            (r.bool(), r.bool(), r.bool(), r.short()),
            (Some(false), Some(false), Some(false), Some(2))
        );
        assert_eq!(joined_group_payload(None, true), vec![0, 1]);
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
        assert_eq!(r.bool(), Some(true));
        assert_eq!(r.bytes(10), Some(&[][..]));
        assert_eq!(r.bool(), Some(false));
        assert!(r.0.is_empty());
        assert_eq!(parse_request_secret(&20i32.to_be_bytes()), Some(20));
    }
}
