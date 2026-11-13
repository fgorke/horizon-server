//! Message markup, for the lines a server owner writes in a plugin's `data/messages`:
//! the legacy codes after `&` or `§` (`&6`, `&l`, `&r`), hex colours (`&#40E0D0` or
//! `<#40E0D0>`), `<gradient:#40E0D0:#00008B>…</gradient>` (two stops or more, one colour per
//! character), `<font:minecraft:uniform>…</font>` and `<smallcaps>…</smallcaps>`. Anything
//! else is text as written.
use crate::text::{ClickEvent, Span, Style, Text, legacy_format};

/// Where a web link in a line starts.
const LINK_STARTS: [&str; 2] = ["https://", "http://"];

/// The web link at the start of `s`, without the punctuation a sentence puts after it: none
/// when `s` starts with none, or with only `https://`.
fn link_at(s: &str) -> Option<&str> {
    let start = LINK_STARTS.iter().find(|p| {
        s.get(..p.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(p))
    })?;
    let end = s
        .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '§'))
        .unwrap_or(s.len());
    let link = s[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'']);
    (link.len() > start.len()).then_some(link)
}

/// The style of a link's characters: `style`, underlined, opening `url` on a click.
fn link_style(style: &Style, url: &str) -> Style {
    Style {
        underlined: Some(true),
        click: Some(ClickEvent::OpenUrl(url.to_owned())),
        ..style.clone()
    }
}

/// `ᴀʙᴄ…`: the small capital of each lowercase letter (`x` has none).
const SMALL_CAPS: [char; 26] = [
    'ᴀ', 'ʙ', 'ᴄ', 'ᴅ', 'ᴇ', 'ꜰ', 'ɢ', 'ʜ', 'ɪ', 'ᴊ', 'ᴋ', 'ʟ', 'ᴍ', 'ɴ', 'ᴏ', 'ᴘ', 'ǫ', 'ʀ', 'ꜱ',
    'ᴛ', 'ᴜ', 'ᴠ', 'ᴡ', 'x', 'ʏ', 'ᴢ',
];

/// One character as drawn: its style, and the gradient it is inside.
struct Glyph {
    c: char,
    style: Style,
    gradient: Option<usize>,
}

/// An open tag, closed by its `</…>`.
enum Tag {
    Gradient,
    /// The font before it.
    Font(Option<String>),
    SmallCaps,
}

struct Parser<'a> {
    base: &'a Style,
    style: Style,
    glyphs: Vec<Glyph>,
    /// Each gradient's colour stops, by index.
    gradients: Vec<Vec<u32>>,
    /// The gradients open, the innermost last.
    open: Vec<usize>,
    tags: Vec<Tag>,
    small_caps: u32,
}

impl Text {
    /// A plugin message's markup ([`crate::markup`]) as text.
    pub fn markup(s: &str) -> Text {
        Self::markup_on(s, &Style::default())
    }

    /// [`Text::markup`] starting from `base`, which `&r` goes back to.
    pub fn markup_on(s: &str, base: &Style) -> Text {
        let mut p = Parser {
            base,
            style: base.clone(),
            glyphs: Vec::new(),
            gradients: Vec::new(),
            open: Vec::new(),
            tags: Vec::new(),
            small_caps: 0,
        };
        let mut rest = s;
        while let Some(c) = rest.chars().next() {
            if let Some(link) = link_at(rest) {
                let style = p.style.clone();
                p.style = link_style(&style, link);
                link.chars().for_each(|c| p.push(c));
                p.style = style;
                rest = &rest[link.len()..];
                continue;
            }
            let after = &rest[c.len_utf8()..];
            rest = p.code(c, after).unwrap_or_else(|| {
                p.push(c);
                after
            });
        }
        p.finish()
    }

    /// A player's chat line as written, its web links opening on a click (aqua, underlined);
    /// none when it has no link. No markup is read: a player's `&` and `<` stay as typed.
    pub fn linkified(s: &str) -> Option<Text> {
        let mut spans: Vec<Span> = Vec::new();
        let mut plain = String::new();
        let mut rest = s;
        let mut any = false;
        while let Some(c) = rest.chars().next() {
            if let Some(link) = link_at(rest) {
                if !plain.is_empty() {
                    spans.push(Span {
                        text: std::mem::take(&mut plain),
                        style: Style::default(),
                    });
                }
                spans.push(Span {
                    text: link.to_owned(),
                    style: link_style(&Style::color(LINK_COLOR), link),
                });
                rest = &rest[link.len()..];
                any = true;
                continue;
            }
            plain.push(c);
            rest = &rest[c.len_utf8()..];
        }
        if !plain.is_empty() {
            spans.push(Span {
                text: plain,
                style: Style::default(),
            });
        }
        any.then(|| {
            let mut text = Text::default();
            text.spans = spans;
            text
        })
    }
}

/// A link's colour in a player's chat line: vanilla's aqua.
const LINK_COLOR: u32 = 0x55FFFF;

impl Parser<'_> {
    /// The markup starting at `c` (`after` follows it): what is left after it, or none when
    /// `c` is plain text.
    fn code<'s>(&mut self, c: char, after: &'s str) -> Option<&'s str> {
        match c {
            '&' | '§' => {
                if let Some(color) = after.strip_prefix('#').and_then(|h| hex_color(h.get(..6)?)) {
                    self.style.color = Some(color);
                    return Some(&after[7..]);
                }
                let code = after.chars().next().filter(|k| is_legacy_code(*k))?;
                let reset = Style {
                    font: self.style.font.clone(),
                    ..self.base.clone()
                };
                self.style = legacy_format(&self.style, code, &reset);
                Some(&after[code.len_utf8()..])
            }
            '<' => {
                let end = after.find('>')?;
                self.tag(&after[..end]).then(|| &after[end + 1..])
            }
            _ => None,
        }
    }

    /// One `<…>` tag, without its brackets: false when it is no tag the markup knows.
    fn tag(&mut self, tag: &str) -> bool {
        if let Some(color) = tag.strip_prefix('#').and_then(hex_color) {
            self.style.color = Some(color);
            return true;
        }
        if let Some(stops) = tag.strip_prefix("gradient:") {
            let stops: Option<Vec<u32>> = stops
                .split(':')
                .map(|s| hex_color(s.strip_prefix('#').unwrap_or(s)))
                .collect();
            let Some(stops) = stops.filter(|s| s.len() >= 2) else {
                return false;
            };
            self.open.push(self.gradients.len());
            self.gradients.push(stops);
            self.tags.push(Tag::Gradient);
            return true;
        }
        if let Some(font) = tag.strip_prefix("font:") {
            let Some(id) = crate::identifier::parse_identifier(font) else {
                return false;
            };
            let before = self.style.font.replace(id);
            self.tags.push(Tag::Font(before));
            return true;
        }
        match tag {
            "smallcaps" => {
                self.small_caps += 1;
                self.tags.push(Tag::SmallCaps);
                true
            }
            "/gradient" | "/font" | "/smallcaps" => {
                self.close(tag);
                true
            }
            _ => false,
        }
    }

    /// `</gradient>`, `</font>` or `</smallcaps>`: the innermost tag of that kind ends.
    fn close(&mut self, tag: &str) {
        let Some(i) = self.tags.iter().rposition(|t| {
            matches!(
                (t, tag),
                (Tag::Gradient, "/gradient")
                    | (Tag::Font(_), "/font")
                    | (Tag::SmallCaps, "/smallcaps")
            )
        }) else {
            return;
        };
        match self.tags.remove(i) {
            Tag::Gradient => {
                self.open.pop();
            }
            Tag::Font(before) => self.style.font = before,
            Tag::SmallCaps => self.small_caps -= 1,
        }
    }

    fn push(&mut self, c: char) {
        let c = match c {
            'a'..='z' if self.small_caps > 0 => SMALL_CAPS[c as usize - 'a' as usize],
            _ => c,
        };
        self.glyphs.push(Glyph {
            c,
            style: self.style.clone(),
            gradient: self.open.last().copied(),
        });
    }

    /// Each gradient's colours over its characters, then runs of one style as spans.
    fn finish(mut self) -> Text {
        for (g, stops) in self.gradients.iter().enumerate() {
            let inside: Vec<usize> = (0..self.glyphs.len())
                .filter(|&i| self.glyphs[i].gradient == Some(g))
                .collect();
            let last = inside.len().saturating_sub(1).max(1) as f32;
            for (k, &i) in inside.iter().enumerate() {
                self.glyphs[i].style.color = Some(blend(stops, k as f32 / last));
            }
        }
        let mut out = Text::default();
        for g in self.glyphs {
            match out.spans.last_mut() {
                Some(span) if span.style == g.style => span.text.push(g.c),
                _ => out.spans.push(Span {
                    text: g.c.to_string(),
                    style: g.style,
                }),
            }
        }
        out
    }
}

fn is_legacy_code(c: char) -> bool {
    matches!(c.to_ascii_lowercase(), '0'..='9' | 'a'..='f' | 'k'..='o' | 'r')
}

/// `RRGGBB`, six hex digits.
fn hex_color(s: &str) -> Option<u32> {
    if s.len() == 6 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        u32::from_str_radix(s, 16).ok()
    } else {
        None
    }
}

/// The colour at `t` (0 to 1) along evenly spaced stops.
fn blend(stops: &[u32], t: f32) -> u32 {
    let at = t.clamp(0.0, 1.0) * (stops.len() - 1) as f32;
    let i = (at.floor() as usize).min(stops.len() - 2);
    let f = at - i as f32;
    let channel = |shift: u32| {
        let (a, b) = ((stops[i] >> shift) & 0xFF, (stops[i + 1] >> shift) & 0xFF);
        ((a as f32 + (b as f32 - a as f32) * f).round() as u32) << shift
    };
    channel(16) | channel(8) | channel(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(t: &Text) -> Vec<(&str, Option<u32>)> {
        t.spans
            .iter()
            .map(|s| (s.text.as_str(), s.style.color))
            .collect()
    }

    #[test]
    fn legacy_and_hex_codes_colour_what_follows() {
        let t = Text::markup("&6Gold §ltoo &#40E0D0teal&r plain");
        assert_eq!(t.plain(), "Gold too teal plain");
        assert_eq!(t.spans[0].style.color, Some(0xFFAA00));
        assert_eq!(t.spans[1].style.bold, Some(true));
        assert_eq!(t.spans[2].style.color, Some(0x40E0D0));
        assert_eq!(t.spans[3].style, Style::default());
    }

    #[test]
    fn a_gradient_runs_from_its_first_stop_to_its_last() {
        let t = Text::markup("<gradient:#40E0D0:#00008B>abc</gradient>!");
        assert_eq!(
            runs(&t),
            [
                ("a", Some(0x40E0D0)),
                ("b", Some(0x2070AE)),
                ("c", Some(0x00008B)),
                ("!", None)
            ]
        );
    }

    #[test]
    fn fonts_and_small_caps_nest_and_close() {
        let t = Text::markup("<font:uniform><smallcaps>Winter</smallcaps></font> x");
        assert_eq!(t.plain(), "Wɪɴᴛᴇʀ x");
        assert_eq!(t.spans[0].style.font.as_deref(), Some("minecraft:uniform"));
        assert_eq!(t.spans[1].style.font, None);
    }

    #[test]
    fn web_links_open_on_a_click() {
        let t = Text::markup("&7See https://example.com/a?b=1&c=2. Thanks");
        let link = t
            .spans
            .iter()
            .find(|s| s.text.starts_with("https"))
            .unwrap();
        assert_eq!(link.text, "https://example.com/a?b=1&c=2");
        assert_eq!(
            link.style.click,
            Some(ClickEvent::OpenUrl("https://example.com/a?b=1&c=2".into()))
        );
        assert_eq!(link.style.underlined, Some(true));
        assert_eq!(t.plain(), "See https://example.com/a?b=1&c=2. Thanks");
        assert!(
            Text::markup("https:// alone").spans[0]
                .style
                .click
                .is_none()
        );

        let chat = Text::linkified("look &a http://x.de/y) <b>").unwrap();
        assert_eq!(chat.plain(), "look &a http://x.de/y) <b>");
        assert_eq!(chat.spans[1].text, "http://x.de/y");
        assert_eq!(chat.spans[1].style.color, Some(LINK_COLOR));
        assert!(chat.spans[0].style.click.is_none());
        assert!(Text::linkified("no link here").is_none());
    }

    #[test]
    fn unknown_tags_and_lone_ampersands_stay_text() {
        let t = Text::markup("Tom & Jerry <b> &z <gradient:#fff>");
        assert_eq!(t.plain(), "Tom & Jerry <b> &z <gradient:#fff>");
    }
}
