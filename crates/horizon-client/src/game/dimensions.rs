//! The server's switch for travel to the End (`/dimension end on|off`), kept in
//! `horizon-dimensions.json` in the world folder. The Nether's is the vanilla
//! game rule `allow_entering_nether_using_portals`.

use std::path::Path;

const FILE: &str = "horizon-dimensions.json";

/// Whether players may go through an End portal into the End.
pub(super) fn end_open(world_root: &Path) -> bool {
    std::fs::read(world_root.join(FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["end"].as_bool())
        .unwrap_or(true)
}

/// Opens or closes the End; false when the file could not be written.
pub(super) fn set_end_open(world_root: &Path, open: bool) -> bool {
    let body = serde_json::json!({ "end": open }).to_string();
    match std::fs::write(world_root.join(FILE), body) {
        Ok(()) => true,
        Err(e) => {
            log::warn!("{FILE}: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_end_is_open_until_closed() {
        let dir = std::env::temp_dir().join(format!("hz-dims-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(end_open(&dir));
        assert!(set_end_open(&dir, false));
        assert!(!end_open(&dir));
        assert!(set_end_open(&dir, true));
        assert!(end_open(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }
}
