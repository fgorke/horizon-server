//! The wire format of the Litematica Server Paster mod, on channel [`CHANNEL`]: a VarInt packet id,
//! then a compound in network NBT (a type byte, no root name). Strings are Java's modified UTF-8,
//! so they decode to UTF-16 code units: the mod cuts long commands by Java `char`, and a surrogate
//! pair may lie across two segments.

pub const CHANNEL: &str = "litematica-server-paster:network_v2";

const TAG_END: u8 = 0;
const TAG_STRING: u8 = 8;
const TAG_COMPOUND: u8 = 10;
const TAG_INT_ARRAY: u8 = 11;
/// Nesting a packet may hold. The mod's packets are flat; this bounds what skipping a value costs.
const MAX_DEPTH: u32 = 16;

/// The ids the mod sends, all of which the server takes.
const CLIENT_PACKETS: [i32; 5] = [0, 1, 2, 3, 4];

/// A packet from the mod.
#[derive(Debug, PartialEq)]
pub enum Packet {
    Hi,
    Chat(Vec<u16>),
    LongStart,
    LongContent(Vec<u16>),
    LongEnd,
    /// An id from a newer mod: the server's answer to `Hi` says it isn't taken.
    Unknown,
}

struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        if len > self.data.len() {
            return None;
        }
        let (head, rest) = self.data.split_at(len);
        self.data = rest;
        Some(head)
    }
    fn byte(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_be_bytes([b[0], b[1]]))
    }
    fn len_i32(&mut self) -> Option<usize> {
        let b = self.take(4)?;
        usize::try_from(i32::from_be_bytes([b[0], b[1], b[2], b[3]])).ok()
    }
    fn varint(&mut self) -> Option<i32> {
        let mut value = 0u32;
        for shift in 0..5 {
            let b = self.byte()?;
            value |= u32::from(b & 0x7f) << (7 * shift);
            if b & 0x80 == 0 {
                return Some(value as i32);
            }
        }
        None
    }
    /// A string tag's payload, its bytes still in modified UTF-8.
    fn string(&mut self) -> Option<&'a [u8]> {
        let len = self.u16()?;
        self.take(usize::from(len))
    }
    fn skip(&mut self, tag: u8, depth: u32) -> Option<()> {
        if depth > MAX_DEPTH {
            return None;
        }
        match tag {
            1 => self.take(1).map(drop),
            2 => self.take(2).map(drop),
            3 | 5 => self.take(4).map(drop),
            4 | 6 => self.take(8).map(drop),
            7 => {
                let n = self.len_i32()?;
                self.take(n).map(drop)
            }
            TAG_STRING => self.string().map(drop),
            9 => {
                let item = self.byte()?;
                let n = self.len_i32()?;
                for _ in 0..n {
                    self.skip(item, depth + 1)?;
                }
                Some(())
            }
            TAG_COMPOUND => loop {
                let inner = self.byte()?;
                if inner == TAG_END {
                    return Some(());
                }
                self.string()?;
                self.skip(inner, depth + 1)?;
            },
            TAG_INT_ARRAY => {
                let n = self.len_i32()?;
                self.take(n.checked_mul(4)?).map(drop)
            }
            12 => {
                let n = self.len_i32()?;
                self.take(n.checked_mul(8)?).map(drop)
            }
            _ => None,
        }
    }
}

/// Modified UTF-8 (`DataInput.readUTF`) to UTF-16 code units; `None` when it is malformed.
fn modified_utf8(bytes: &[u8]) -> Option<Vec<u16>> {
    let mut units = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let continuation = |at: usize| -> Option<u16> {
        let b = *bytes.get(at)?;
        (b & 0xc0 == 0x80).then_some(u16::from(b & 0x3f))
    };
    while i < bytes.len() {
        let b = u16::from(bytes[i]);
        match b {
            0x00..=0x7f => {
                units.push(b);
                i += 1;
            }
            0xc0..=0xdf => {
                units.push((b & 0x1f) << 6 | continuation(i + 1)?);
                i += 2;
            }
            0xe0..=0xef => {
                units.push((b & 0x0f) << 12 | continuation(i + 1)? << 6 | continuation(i + 2)?);
                i += 3;
            }
            _ => return None,
        }
    }
    Some(units)
}

/// The top-level string `key` of the compound the reader stands on; empty when it has none, as the
/// mod's `getStringOr(key, "")`.
fn top_string(r: &mut Reader, key: &str) -> Option<Vec<u16>> {
    if r.byte()? != TAG_COMPOUND {
        return None;
    }
    let mut found = Vec::new();
    loop {
        let tag = r.byte()?;
        if tag == TAG_END {
            return Some(found);
        }
        let name = r.string()?;
        if tag == TAG_STRING && name == key.as_bytes() {
            found = modified_utf8(r.string()?)?;
        } else {
            r.skip(tag, 1)?;
        }
    }
}

/// A packet of the mod; `None` when the data doesn't read as one.
pub fn decode(data: &[u8]) -> Option<Packet> {
    let mut r = Reader { data };
    Some(match r.varint()? {
        0 => Packet::Hi,
        1 => Packet::Chat(top_string(&mut r, "chat")?),
        2 => Packet::LongStart,
        3 => Packet::LongContent(top_string(&mut r, "segment")?),
        4 => Packet::LongEnd,
        _ => Packet::Unknown,
    })
}

fn named(out: &mut Vec<u8>, tag: u8, name: &str) {
    out.push(tag);
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
}

/// S2C `HI`: the server's version, which the mod logs. `version` is plain ASCII.
pub fn hi(version: &str) -> Vec<u8> {
    let mut out = vec![0, TAG_COMPOUND];
    named(&mut out, TAG_STRING, "mod_version");
    out.extend_from_slice(&(version.len() as u16).to_be_bytes());
    out.extend_from_slice(version.as_bytes());
    out.push(TAG_END);
    out
}

/// S2C `ACCEPT_PACKETS`: what the server takes, long commands included.
pub fn accept_packets() -> Vec<u8> {
    let mut out = vec![1, TAG_COMPOUND];
    named(&mut out, TAG_INT_ARRAY, "ids");
    out.extend_from_slice(&(CLIENT_PACKETS.len() as i32).to_be_bytes());
    for id in CLIENT_PACKETS {
        out.extend_from_slice(&id.to_be_bytes());
    }
    out.push(TAG_END);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(id: u8, key: &str, value: &[u8]) -> Vec<u8> {
        let mut out = vec![id, TAG_COMPOUND];
        named(&mut out, TAG_STRING, key);
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value);
        out.push(TAG_END);
        out
    }

    #[test]
    fn reads_the_mods_packets() {
        assert_eq!(decode(&[0, 10, 0]), Some(Packet::Hi));
        assert_eq!(decode(&[2, 10, 0]), Some(Packet::LongStart));
        assert_eq!(decode(&[4, 10, 0]), Some(Packet::LongEnd));
        assert_eq!(decode(&[9, 10, 0]), Some(Packet::Unknown));
        assert_eq!(
            decode(&packet(1, "chat", b"/setblock")),
            Some(Packet::Chat("/setblock".encode_utf16().collect()))
        );
        assert_eq!(
            decode(&packet(3, "segment", b"x")),
            Some(Packet::LongContent(vec![u16::from(b'x')]))
        );
    }

    #[test]
    fn a_missing_key_reads_empty_and_other_tags_are_skipped() {
        assert_eq!(decode(&[1, 10, 0]), Some(Packet::Chat(Vec::new())));
        // {n:[I;7], l:[{}], chat:"a"}
        let mut data = vec![1, TAG_COMPOUND];
        named(&mut data, TAG_INT_ARRAY, "n");
        data.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 7]);
        named(&mut data, 9, "l");
        data.extend_from_slice(&[TAG_COMPOUND, 0, 0, 0, 1, TAG_END]);
        named(&mut data, TAG_STRING, "chat");
        data.extend_from_slice(&[0, 1, b'a', TAG_END]);
        assert_eq!(decode(&data), Some(Packet::Chat(vec![u16::from(b'a')])));
    }

    #[test]
    fn modified_utf8_is_javas() {
        // A NUL is two bytes, an emoji two three-byte surrogates.
        let bytes = [0xc0, 0x80, 0xc3, 0xa4, 0xed, 0xa0, 0xbd, 0xed, 0xb8, 0x80];
        assert_eq!(modified_utf8(&bytes), Some(vec![0, 0xe4, 0xd83d, 0xde00]));
        assert_eq!(modified_utf8(&[0xc3]), None);
        assert_eq!(modified_utf8(&[0xf0, 0x9f, 0x98, 0x80]), None);
    }

    #[test]
    fn damaged_packets_are_refused() {
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[1]), None);
        assert_eq!(decode(&[1, 9, 0]), None);
        assert_eq!(decode(&[1, TAG_COMPOUND, TAG_STRING, 0, 4, b'c']), None);
        // A negative array length, and a list nested past the limit.
        let mut data = vec![1, TAG_COMPOUND];
        named(&mut data, 7, "a");
        data.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
        assert_eq!(decode(&data), None);
        let mut deep = vec![1, TAG_COMPOUND];
        named(&mut deep, 9, "l");
        for _ in 0..64 {
            deep.extend_from_slice(&[9, 0, 0, 0, 1]);
        }
        assert_eq!(decode(&deep), None);
    }

    #[test]
    fn the_answers_read_as_the_mod_reads_them() {
        assert_eq!(
            hi("1"),
            [
                0, 10, 8, 0, 11, b'm', b'o', b'd', b'_', b'v', b'e', b'r', b's', b'i', b'o', b'n',
                0, 1, b'1', 0
            ]
        );
        let accept = accept_packets();
        assert_eq!(&accept[..7], &[1, 10, 11, 0, 3, b'i', b'd']);
        assert_eq!(accept.len(), 2 + 3 + 3 + 4 + 20 + 1);
        assert_eq!(accept.last(), Some(&0));
    }
}
