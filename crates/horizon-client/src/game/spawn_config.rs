//! The server's spawn settings, kept in `horizon-spawn.json` in the world
//! folder: the spot /spawn leads to (/setspawn) and the spawn elytra region
//! (/spawnelytra pos1|pos2). Read once and cached; every change is written.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const FILE: &str = "horizon-spawn.json";

/// A spot in a level, with the look to arrive with.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct SpawnPoint {
    pub(super) dimension: String,
    pub(super) at: [f64; 3],
    pub(super) yaw: f32,
    pub(super) pitch: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct SpawnConfig {
    pub(super) spawn: Option<SpawnPoint>,
    /// The spawn elytra region's level and corners, as they were set.
    pub(super) region_dimension: Option<String>,
    pub(super) pos1: Option<[i32; 3]>,
    pub(super) pos2: Option<[i32; 3]>,
}

impl SpawnConfig {
    /// Whether `at` in `dimension` is in the spawn elytra region (both
    /// corners' blocks included).
    pub(super) fn in_region(&self, dimension: &str, at: [f64; 3]) -> bool {
        let (Some(a), Some(b), Some(dim)) =
            (self.pos1, self.pos2, self.region_dimension.as_deref())
        else {
            return false;
        };
        dim == dimension
            && (0..3).all(|k| {
                let (lo, hi) = (a[k].min(b[k]), a[k].max(b[k]));
                at[k] >= f64::from(lo) && at[k] < f64::from(hi) + 1.0
            })
    }

    fn from_json(v: &Value) -> Self {
        let ints = |v: &Value| -> Option<[i32; 3]> {
            let a = v.as_array()?;
            Some([0, 1, 2].map(|k| a.get(k).and_then(Value::as_i64).unwrap_or(0) as i32))
        };
        let spawn = v.get("spawn").and_then(|s| {
            let a = s["at"].as_array()?;
            Some(SpawnPoint {
                dimension: s["dimension"].as_str()?.to_owned(),
                at: [0, 1, 2].map(|k| a.get(k).and_then(Value::as_f64).unwrap_or(0.0)),
                yaw: s["yaw"].as_f64().unwrap_or(0.0) as f32,
                pitch: s["pitch"].as_f64().unwrap_or(0.0) as f32,
            })
        });
        let region = &v["elytra"];
        SpawnConfig {
            spawn,
            region_dimension: region["dimension"].as_str().map(str::to_owned),
            pos1: ints(&region["pos1"]),
            pos2: ints(&region["pos2"]),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "spawn": self.spawn.as_ref().map(|s| json!({
                "dimension": s.dimension,
                "at": s.at,
                "yaw": s.yaw,
                "pitch": s.pitch,
            })),
            "elytra": {
                "dimension": self.region_dimension,
                "pos1": self.pos1,
                "pos2": self.pos2,
            },
        })
    }
}

static CACHE: Mutex<Option<(PathBuf, SpawnConfig)>> = Mutex::new(None);

fn load(root: &Path) -> SpawnConfig {
    std::fs::read(root.join(FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .map(|v| SpawnConfig::from_json(&v))
        .unwrap_or_default()
}

/// The settings of the world at `root`.
pub(super) fn read<R>(root: &Path, f: impl FnOnce(&SpawnConfig) -> R) -> R {
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cache.as_ref().is_none_or(|(r, _)| r != root) {
        *cache = Some((root.to_path_buf(), load(root)));
    }
    f(&cache.as_ref().expect("just loaded").1)
}

/// Changes the settings and writes them; false when the file could not be
/// written (the change is kept in memory all the same).
pub(super) fn change(root: &Path, f: impl FnOnce(&mut SpawnConfig)) -> bool {
    let mut next = read(root, Clone::clone);
    f(&mut next);
    let body = serde_json::to_string_pretty(&next.to_json()).unwrap_or_default();
    *CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((root.to_path_buf(), next));
    match std::fs::write(root.join(FILE), body) {
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
    fn the_region_takes_both_corner_blocks_in_its_level() {
        let c = SpawnConfig {
            region_dimension: Some("minecraft:overworld".into()),
            pos1: Some([10, 200, -5]),
            pos2: Some([-10, 150, 5]),
            ..Default::default()
        };
        assert!(c.in_region("minecraft:overworld", [10.9, 200.9, 5.9]));
        assert!(c.in_region("minecraft:overworld", [-10.0, 150.0, -5.0]));
        assert!(!c.in_region("minecraft:overworld", [11.0, 180.0, 0.0]));
        assert!(!c.in_region("minecraft:overworld", [0.0, 149.9, 0.0]));
        assert!(!c.in_region("minecraft:the_nether", [0.0, 180.0, 0.0]));
        assert!(!SpawnConfig::default().in_region("minecraft:overworld", [0.0; 3]));
    }

    #[test]
    fn the_settings_survive_the_file() {
        let c = SpawnConfig {
            spawn: Some(SpawnPoint {
                dimension: "minecraft:overworld".into(),
                at: [0.5, 201.0, -3.25],
                yaw: 90.0,
                pitch: -10.0,
            }),
            region_dimension: Some("minecraft:overworld".into()),
            pos1: Some([1, 2, 3]),
            pos2: None,
        };
        assert_eq!(SpawnConfig::from_json(&c.to_json()), c);
        assert_eq!(SpawnConfig::from_json(&json!({})), SpawnConfig::default());
    }
}
