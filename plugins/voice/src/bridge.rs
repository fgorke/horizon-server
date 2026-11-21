//! The bot's link to its program on the same machine, over the plugin's UDP port.
//!
//! Program to plugin, from 127.0.0.1: `FE`, the 16 byte secret (`storage` key `bridge_token`,
//! as 32 hex digits), then a kind: `01` hello (once a second; the plugin answers with its
//! status), `02` + a player's UUID (join that player's group), `03` (leave), `04` + sequence
//! (i64) + an Opus frame (the bot's voice, 48 kHz mono, 20 ms), `05` + a player's name, a
//! NUL and a line for their action bar (what the bot does; no console command, so no
//! operator sees a command's feedback), `06` + a player's name (what they see: the plugin
//! answers `83` w h palette-size, `86` index + block state per palette entry, `84` offset +
//! 6 bytes per pixel (block u16, face, u, v, distance) and `85` + 1 or 0 at the end), `07` +
//! an order for the bot's body (`body::Order`: 01 come + name, 02 remove, 03 follow + name,
//! 04 stay, 05 walk + east i16 + south i16, 06 hit player + name, 07 hit mob + kind, 08 build +
//! per block east/up/south as i8 and a length-prefixed (u8) block state). The plugin tells
//! where the body is once a second: `87` + dimension, NUL, x y z (f64), what it does; `87`
//! alone when there is none.
//!
//! Plugin to program: `FE 81` + the speaker's UUID + an Opus frame (what the group says), and
//! `FE 82` + 1 or 0 (in a group or not).
use crate::protocol::{PlayerState, Writer};
use horizon_mod_sdk::server::network;

pub const BOT_UUID: u128 = 0xc1a0_de00_0000_4000_8000_0000_c1a0_de01;
pub const BOT_NAME: &str = "Schorsch";
const MAGIC: u8 = 0xFE;
/// Ticks without a hello after which the program counts as gone (10 s).
const SILENT_TICKS: u64 = 200;

pub enum Message {
    Hello,
    Join(u128),
    Leave,
    Audio(i64, Vec<u8>),
    ActionBar(String, String),
    View(String),
    Body(crate::body::Order),
}

/// An order for the body: its kind byte, then what it needs; names, mob kinds and block
/// states are checked here (a refused block is left out of a build).
fn body_order(data: &[u8]) -> Option<crate::body::Order> {
    use crate::body::{Order, allowed_state, mob_kind, player_name};
    let text = || String::from_utf8(data.get(1..)?.to_vec()).ok();
    let name = || text().filter(|n| player_name(n));
    Some(match *data.first()? {
        1 => Order::Come(name()?),
        2 => Order::Remove,
        3 => Order::Follow(name()?),
        4 => Order::Stay,
        5 => Order::Walk(
            i16::from_be_bytes(data.get(1..3)?.try_into().ok()?),
            i16::from_be_bytes(data.get(3..5)?.try_into().ok()?),
        ),
        6 => Order::AttackPlayer(name()?),
        7 => Order::AttackMob(mob_kind(&text()?)?),
        8 => {
            let mut blocks = Vec::new();
            let mut rest = data.get(1..)?;
            while rest.len() >= 4 && blocks.len() < crate::body::MAX_BUILD {
                let d = [rest[0] as i8, rest[1] as i8, rest[2] as i8];
                let len = usize::from(rest[3]);
                let state = std::str::from_utf8(rest.get(4..4 + len)?).ok()?;
                if let Some(state) = allowed_state(state) {
                    blocks.push((d, state));
                }
                rest = &rest[4 + len..];
            }
            Order::Build(blocks)
        }
        _ => return None,
    })
}

/// Whether a datagram is meant for the bridge: from this machine, with the bridge's magic byte.
pub fn is_bridge(from: &str, data: &[u8]) -> bool {
    from.starts_with("127.0.0.1:") && data.first() == Some(&MAGIC)
}

fn parse_token(raw: Option<Vec<u8>>) -> Option<[u8; 16]> {
    let text = String::from_utf8(raw?).ok()?;
    let hex = text.trim();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

pub struct Bot {
    token: Option<[u8; 16]>,
    program: Option<String>,
    last_hello: u64,
    tick: u64,
    pub group: Option<u128>,
}

impl Bot {
    /// No secret, no bridge: every datagram for it is dropped.
    pub fn new(token: Option<Vec<u8>>) -> Self {
        Bot {
            token: parse_token(token),
            program: None,
            last_hello: 0,
            tick: 0,
            group: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.token.is_some()
    }

    pub fn set_tick(&mut self, tick: u64) {
        self.tick = tick;
    }

    pub fn silent(&self) -> bool {
        self.tick.saturating_sub(self.last_hello) > SILENT_TICKS
    }

    /// The bot as the voice chat shows it, while it is in a group.
    pub fn state(&self) -> Option<PlayerState> {
        self.group.map(|g| PlayerState {
            uuid: BOT_UUID,
            name: BOT_NAME.to_owned(),
            disabled: false,
            disconnected: false,
            group: Some(g),
        })
    }

    /// A datagram from the program, if it carries the secret.
    pub fn read(&mut self, from: &str, data: &[u8]) -> Option<Message> {
        let token = self.token?;
        let rest = data.get(1..)?;
        if rest.get(..16)? != token {
            return None;
        }
        let body = &rest[16..];
        let message = match *body.first()? {
            1 => Message::Hello,
            2 => Message::Join(u128::from_be_bytes(body.get(1..17)?.try_into().ok()?)),
            3 => Message::Leave,
            4 => Message::Audio(
                i64::from_be_bytes(body.get(1..9)?.try_into().ok()?),
                body.get(9..)?.to_vec(),
            ),
            5 => {
                let rest = body.get(1..)?;
                let nul = rest.iter().position(|b| *b == 0)?;
                let name = String::from_utf8(rest[..nul].to_vec()).ok()?;
                let text = String::from_utf8(rest[nul + 1..].to_vec()).ok()?;
                if name.is_empty() || name.len() > 16 || text.chars().count() > 256 {
                    return None;
                }
                Message::ActionBar(name, text)
            }
            6 => {
                let name = String::from_utf8(body.get(1..)?.to_vec()).ok()?;
                if name.is_empty() || name.len() > 16 {
                    return None;
                }
                Message::View(name)
            }
            7 => Message::Body(body_order(body.get(1..)?)?),
            _ => return None,
        };
        self.program = Some(from.to_owned());
        self.last_hello = self.tick;
        Some(message)
    }

    fn send(&self, kind: u8, body: &[u8]) {
        if let Some(to) = &self.program {
            let mut w = Writer::default();
            w.0.push(MAGIC);
            w.0.push(kind);
            w.0.extend_from_slice(body);
            network::udp_send(to, &w.0);
        }
    }

    pub fn tell_status(&self) {
        self.send(0x82, &[u8::from(self.group.is_some())]);
    }

    /// A finished view: its size, palette and pixels, then the end.
    pub fn tell_view(&self, view: &crate::view::View) {
        let mut head = Vec::new();
        head.extend_from_slice(&crate::view::WIDTH.to_be_bytes());
        head.extend_from_slice(&crate::view::HEIGHT.to_be_bytes());
        head.extend_from_slice(&(view.palette.len() as u16).to_be_bytes());
        self.send(0x83, &head);
        for (i, state) in view.palette.iter().enumerate() {
            let mut body = (i as u16).to_be_bytes().to_vec();
            body.extend_from_slice(state.as_bytes());
            self.send(0x86, &body);
        }
        for (n, chunk) in view.pixels.chunks(600).enumerate() {
            let mut body = ((n * 600) as u32).to_be_bytes().to_vec();
            for p in chunk {
                body.extend_from_slice(&p.block.to_be_bytes());
                body.extend_from_slice(&[p.face, p.u, p.v, p.distance]);
            }
            self.send(0x84, &body);
        }
        self.send(0x85, &[1]);
    }

    /// Where the body is and what it does, or that there is none.
    pub fn tell_body(&self, body: Option<&crate::body::Body>) {
        let mut out = Vec::new();
        if let Some(b) = body {
            out.extend_from_slice(b.dimension.as_bytes());
            out.push(0);
            for v in b.at {
                out.extend_from_slice(&v.to_be_bytes());
            }
            out.extend_from_slice(b.doing().as_bytes());
        }
        self.send(0x87, &out);
    }

    /// No view: the player is not online.
    pub fn tell_no_view(&self) {
        self.send(0x85, &[0]);
    }

    pub fn tell_heard(&self, speaker: u128, opus: &[u8]) {
        let mut body = speaker.to_be_bytes().to_vec();
        body.extend_from_slice(opus);
        self.send(0x81, &body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot() -> Bot {
        Bot::new(Some(
            b"000102030405060708090a0b0c0d0e0f
"
            .to_vec(),
        ))
    }

    fn packet(token: [u8; 16], body: &[u8]) -> Vec<u8> {
        let mut p = vec![MAGIC];
        p.extend_from_slice(&token);
        p.extend_from_slice(body);
        p
    }

    const TOKEN: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

    #[test]
    fn only_this_machine_with_the_secret_reaches_the_bot() {
        assert!(is_bridge("127.0.0.1:5000", &[MAGIC]));
        assert!(!is_bridge("10.0.0.1:5000", &[MAGIC]));
        assert!(!is_bridge("127.0.0.1:5000", &[0xFF]));
        let mut b = bot();
        assert!(b.read("127.0.0.1:5000", &packet([9; 16], &[1])).is_none());
        assert!(matches!(
            b.read("127.0.0.1:5000", &packet(TOKEN, &[1])),
            Some(Message::Hello)
        ));
        let mut join = vec![2];
        join.extend_from_slice(&42u128.to_be_bytes());
        assert!(matches!(
            b.read("127.0.0.1:5000", &packet(TOKEN, &join)),
            Some(Message::Join(42))
        ));
        let mut audio = vec![4];
        audio.extend_from_slice(&7i64.to_be_bytes());
        audio.extend_from_slice(&[1, 2, 3]);
        match b.read("127.0.0.1:5000", &packet(TOKEN, &audio)) {
            Some(Message::Audio(7, opus)) => assert_eq!(opus, vec![1, 2, 3]),
            _ => panic!("no audio"),
        }
        assert!(b.read("127.0.0.1:5000", &packet(TOKEN, &[2, 1])).is_none());
        match b.read("127.0.0.1:5000", &packet(TOKEN, b"\x05Alex\x00\xc2\xa76hi")) {
            Some(Message::ActionBar(name, text)) => {
                assert_eq!((name.as_str(), text.as_str()), ("Alex", "\u{a7}6hi"))
            }
            _ => panic!("no action bar"),
        }
        assert!(
            b.read("127.0.0.1:5000", &packet(TOKEN, b"\x05Alex"))
                .is_none()
        );
        // Without a secret there is no bot.
        let mut none = Bot::new(None);
        assert!(none.read("127.0.0.1:5000", &packet(TOKEN, &[1])).is_none());
        // Quiet for more than 10 s: gone.
        b.set_tick(300);
        assert!(b.silent());
    }
}
