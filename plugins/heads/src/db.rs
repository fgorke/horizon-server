//! The head catalog, `catalog/heads.bin` built into the plugin: the public head database HeadDB
//! reads (`data.headsdb.com`), packed by `tools/pack.py`. Little endian:
//!
//! ```text
//! "HDB1"
//! u16 categories, each: u8 len, id; u8 len, name
//! u16 tags, each: u8 len, name
//! u32 heads, each: u8 category; u8 hex digits, the texture hash (an odd count has a 0 before
//!   it), packed two to a byte; u8 len, name; u8 tags, each u16
//! ```

/// The catalog as built into the plugin.
pub const CATALOG: &[u8] = include_bytes!("../catalog/heads.bin");

/// One head of the catalog.
#[derive(Clone, Debug, PartialEq)]
pub struct Head<'a> {
    pub category: usize,
    /// The skin's hash at `textures.minecraft.net/texture/`.
    pub texture: String,
    pub name: &'a str,
    pub tags: Vec<usize>,
}

/// The catalog, read once: where each head starts, the heads of each category, and the names
/// in lowercase for searches.
pub struct Db {
    data: &'static [u8],
    /// Category ids (`animals`).
    pub categories: Vec<String>,
    pub tags: Vec<String>,
    tags_lower: Vec<String>,
    starts: Vec<u32>,
    /// The heads of each category, in catalog order.
    pub by_category: Vec<Vec<u32>>,
    names_lower: String,
    /// Where each head's lowercase name ends in `names_lower`.
    name_ends: Vec<u32>,
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Option<u8> {
        let b = *self.data.get(self.at)?;
        self.at += 1;
        Some(b)
    }
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let b = self.data.get(self.at..self.at + n)?;
        self.at += n;
        Some(b)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.bytes(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }
    fn text(&mut self) -> Option<&'a str> {
        let n = usize::from(self.u8()?);
        std::str::from_utf8(self.bytes(n)?).ok()
    }
    /// A head: its category, texture, name and tags.
    fn head(&mut self) -> Option<Head<'a>> {
        let category = usize::from(self.u8()?);
        let digits = usize::from(self.u8()?);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let packed = self.bytes(digits.div_ceil(2))?;
        let mut hex = String::with_capacity(packed.len() * 2);
        for b in packed {
            hex.push(HEX[usize::from(b >> 4)] as char);
            hex.push(HEX[usize::from(b & 15)] as char);
        }
        let texture = hex[hex.len() - digits..].to_owned();
        let name = self.text()?;
        let count = usize::from(self.u8()?);
        let tags = (0..count)
            .map(|_| self.u16().map(usize::from))
            .collect::<Option<_>>()?;
        Some(Head {
            category,
            texture,
            name,
            tags,
        })
    }
    /// A head's category and name, the rest skipped: what loading needs, cheaply.
    fn skim(&mut self) -> Option<(usize, &'a str)> {
        let category = usize::from(self.u8()?);
        let digits = usize::from(self.u8()?);
        self.bytes(digits.div_ceil(2))?;
        let name = self.text()?;
        let count = usize::from(self.u8()?);
        self.bytes(count * 2)?;
        Some((category, name))
    }
}

impl Db {
    /// Reads `data`; None when it isn't a catalog.
    pub fn read(data: &'static [u8]) -> Option<Db> {
        let mut r = Reader { data, at: 0 };
        if r.bytes(4)? != b"HDB1" {
            return None;
        }
        let categories = (0..r.u16()?)
            .map(|_| {
                let id = r.text()?.to_owned();
                r.text()?;
                Some(id)
            })
            .collect::<Option<Vec<_>>>()?;
        let tags: Vec<String> = (0..r.u16()?)
            .map(|_| r.text().map(str::to_owned))
            .collect::<Option<_>>()?;
        let count = r.u32()? as usize;
        let mut db = Db {
            data,
            tags_lower: tags.iter().map(|t| t.to_lowercase()).collect(),
            by_category: vec![Vec::new(); categories.len()],
            categories,
            tags,
            starts: Vec::with_capacity(count),
            names_lower: String::new(),
            name_ends: Vec::with_capacity(count),
        };
        for i in 0..count {
            db.starts.push(r.at as u32);
            let (category, name) = r.skim()?;
            db.by_category.get_mut(category)?.push(i as u32);
            if name.is_ascii() {
                let start = db.names_lower.len();
                db.names_lower.push_str(name);
                db.names_lower[start..].make_ascii_lowercase();
            } else {
                db.names_lower
                    .extend(name.chars().flat_map(char::to_lowercase));
            }
            // A separator no word holds: a match never spans two names.
            db.names_lower.push('\n');
            db.name_ends.push(db.names_lower.len() as u32);
        }
        Some(db)
    }

    pub fn len(&self) -> usize {
        self.starts.len()
    }

    /// Head `i`.
    pub fn head(&self, i: u32) -> Option<Head<'static>> {
        let at = *self.starts.get(i as usize)? as usize;
        Reader {
            data: self.data,
            at,
        }
        .head()
    }

    /// Whether a tag of head `i` passes `hit`, read in place.
    fn any_tag(&self, i: usize, hit: &[bool]) -> bool {
        let Some(&at) = self.starts.get(i) else {
            return false;
        };
        let mut r = Reader {
            data: self.data,
            at: at as usize + 1,
        };
        let mut read = || -> Option<bool> {
            let digits = usize::from(r.u8()?);
            r.bytes(digits.div_ceil(2))?;
            r.text()?;
            let count = r.u8()?;
            for _ in 0..count {
                if hit.get(usize::from(r.u16()?)).copied().unwrap_or(false) {
                    return Some(true);
                }
            }
            Some(false)
        };
        read().unwrap_or(false)
    }

    /// The heads whose name holds `word` or one of whose tags `tag_hit` marks.
    fn matching(&self, word: &str, tag_hit: &[bool]) -> Vec<bool> {
        let mut hit = vec![false; self.len()];
        // One search through every name: `name_ends` finds whose each match is.
        for (at, _) in self.names_lower.match_indices(word) {
            let i = self.name_ends.partition_point(|&end| end as usize <= at);
            if let Some(h) = hit.get_mut(i) {
                *h = true;
            }
        }
        if tag_hit.contains(&true) {
            for (i, h) in hit.iter_mut().enumerate() {
                if !*h {
                    *h = self.any_tag(i, tag_hit);
                }
            }
        }
        hit
    }

    /// The heads whose name or a tag holds every word of `query` (any case), in catalog order.
    pub fn search(&self, query: &str) -> Vec<u32> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            return Vec::new();
        }
        let mut all = vec![true; self.len()];
        for w in &words {
            let tag_hit: Vec<bool> = self
                .tags_lower
                .iter()
                .map(|t| t.contains(w.as_str()))
                .collect();
            for (a, m) in all.iter_mut().zip(self.matching(w, &tag_hit)) {
                *a &= m;
            }
        }
        (0..self.len() as u32)
            .filter(|&i| all[i as usize])
            .collect()
    }
}
/// Base64 (standard, padded).
pub fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The `textures` property a head with skin `texture` carries.
pub fn texture_value(texture: &str) -> String {
    base64(
        format!(
            "{{\"textures\":{{\"SKIN\":{{\"url\":\"http://textures.minecraft.net/texture/{texture}\"}}}}}}"
        )
        .as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_catalog_reads() {
        let db = Db::read(CATALOG).expect("catalog");
        assert!(db.len() > 100_000);
        assert_eq!(db.categories.len(), 15);
        let first = db.head(0).expect("head 0");
        assert_eq!(first.name, "Melon");
        assert_eq!(
            first.texture,
            "9636dee806ba47a2c40e95b57a12f37de6c2e677f2160132a07e24eeffa6"
        );
        let total: usize = db.by_category.iter().map(Vec::len).sum();
        assert_eq!(total, db.len());
    }

    #[test]
    fn searches_names_and_tags() {
        let db = Db::read(CATALOG).expect("catalog");
        let melons = db.search("MELON");
        assert!(melons.contains(&0));
        assert!(db.search("berry").len() > 1, "a tag finds heads");
        assert!(db.search("zzzqqqxxx").is_empty());
        assert!(db.search("  ").is_empty());
    }
}
