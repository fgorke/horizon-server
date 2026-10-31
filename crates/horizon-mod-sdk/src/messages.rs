//! A plugin's messages, which the server owner words in its `data/messages` (storage key
//! `messages`): one `key = value` per line, `"quoted"` to keep spaces at its ends, `#` for a
//! comment. The plugin gives each key's default, and the file is written with them the first
//! time; `{name}` in a value takes [`Messages::get`]'s argument of that name. Values are the
//! server's markup (`docs/plugins.md`): `&6` colours, `&#40E0D0`,
//! `<gradient:#40E0D0:#00008B>…</gradient>`, `<font:…>`, `<smallcaps>`. The key `prefix` is
//! read by the server itself: what the plugin's chat lines start with.
use std::collections::BTreeMap;
use std::fmt::Display;

const KEY: &str = "messages";

/// The plugin's messages, by key.
pub struct Messages {
    lines: BTreeMap<String, String>,
}

impl Messages {
    /// The messages in `data/messages`, the defaults for keys it leaves out. With no file yet,
    /// the defaults are written to it.
    pub fn load(defaults: &[(&str, &str)]) -> Messages {
        let mut lines: BTreeMap<String, String> = defaults
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        match crate::storage::get(KEY) {
            Some(bytes) => lines.extend(String::from_utf8_lossy(&bytes).lines().filter_map(line)),
            None => {
                if let Err(e) = crate::storage::set(KEY, file(defaults).as_bytes()) {
                    crate::warn(&format!("could not write the default messages: {e}"));
                }
            }
        }
        Messages { lines }
    }

    /// The message `key`, each `{name}` in it replaced by its argument; the key itself when
    /// there is no such message.
    pub fn get(&self, key: &str, args: &[(&str, &dyn Display)]) -> String {
        let mut text = self
            .lines
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_owned());
        for (name, value) in args {
            text = text.replace(&format!("{{{name}}}"), &value.to_string());
        }
        text
    }
}

/// One `key = value` line of `data/messages`; none for a blank line or a comment.
pub fn line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, value) = line.split_once('=')?;
    let value = value.trim();
    let value = match value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        Some(quoted) => quoted.replace("\\\"", "\"").replace("\\\\", "\\"),
        None => value.to_owned(),
    };
    Some((key.trim().to_owned(), value))
}

/// The file written the first time: the defaults, quoted, after a note on the markup.
fn file(defaults: &[(&str, &str)]) -> String {
    let mut out = String::from(
        "# This plugin's messages. Colours: &0-&f, &l bold, &o italic, &r reset, &#RRGGBB;\n\
         # <gradient:#40E0D0:#00008B>text</gradient>, <font:minecraft:uniform>text</font>,\n\
         # <smallcaps>text</smallcaps>. {name} is filled in. /plugins reload after a change.\n\
         # What the plugin's chat lines start with, instead of its name:\n\
         # prefix = \"<gradient:#40E0D0:#00008B>[Server]</gradient> &7\"\n",
    );
    for (key, value) in defaults {
        let value = value.replace('\\', "\\\\").replace('"', "\\\"");
        out += &format!("{key} = \"{value}\"\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_keep_quoted_spaces_and_skip_comments() {
        assert_eq!(
            line("prefix = \"&b[Winter] \""),
            Some(("prefix".into(), "&b[Winter] ".into()))
        );
        assert_eq!(
            line("  done=  &aDone. "),
            Some(("done".into(), "&aDone.".into()))
        );
        assert_eq!(line("# prefix = x"), None);
        assert_eq!(
            line("say = \"a \\\"b\\\"\""),
            Some(("say".into(), "a \"b\"".into()))
        );
    }

    #[test]
    fn the_written_file_reads_back_as_the_defaults() {
        let defaults = [("set", "Set: \"{x}\""), ("back\\", "a\\b")];
        let read: Vec<(String, String)> = file(&defaults).lines().filter_map(line).collect();
        let want: Vec<(String, String)> = defaults
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert_eq!(read, want);
    }
}
