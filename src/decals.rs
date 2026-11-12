//! Decals: images pinned to canvas coordinates, drawn below the windows.
//!
//! The smithay-free half: serde types, the on-disk file and its bounds. The
//! compositor-side state, texture cache and drawing live in the bin crate
//! (`state/decals.rs`, `render/decals.rs`).
//!
//! Path: `$XDG_STATE_HOME/driftwm/decals.json`, next to `session.json`. A
//! separate file because a decal is not a window: it needs no relaunch, so none
//! of the session store's restore flags apply, and it is always saved.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;

/// The registry takes no new decal past this many.
pub const MAX_DECALS: usize = 256;

/// The scale range a decal may be given: its pixel size times this is its size
/// in canvas units.
pub const SCALE_RANGE: std::ops::RangeInclusive<f64> = 0.01..=64.0;

/// The canvas range a decal's center may sit in.
pub const POSITION_LIMIT: f64 = 1e7;

/// One image pinned to the canvas. `position` is the image's center in Y-up rule
/// coordinates, the convention of bookmarks and `msg move`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decal {
    pub id: u64,
    pub path: PathBuf,
    pub position: [f64; 2],
    pub scale: f64,
}

#[derive(Debug, Serialize, Deserialize)]
struct DecalFile {
    version: u32,
    decals: Vec<Decal>,
}

/// `$XDG_STATE_HOME/driftwm/decals.json`, falling back to
/// `$HOME/.local/state/driftwm/decals.json`. `None` when neither is set.
pub fn default_path() -> Option<PathBuf> {
    crate::session::default_session_path().map(|p| p.with_file_name("decals.json"))
}

/// Why a decal at `position` with `scale` can't be held, if it can't.
pub fn problem(position: [f64; 2], scale: f64) -> Option<String> {
    if !position.iter().all(|c| c.is_finite()) {
        return Some("decal coordinates must be finite".to_string());
    }
    if position.iter().any(|c| c.abs() > POSITION_LIMIT) {
        return Some(format!(
            "decal coordinates must be within ±{POSITION_LIMIT:e}"
        ));
    }
    if !SCALE_RANGE.contains(&scale) {
        return Some(format!(
            "decal scale must be within {}..={}",
            SCALE_RANGE.start(),
            SCALE_RANGE.end()
        ));
    }
    None
}

/// Read the saved decals. A missing file is a fresh start; an unreadable or
/// unparseable one is set aside (`decals.json.<label>.<unix-ts>`) and reads as
/// empty, so a bad file never crashes startup and the next write can't destroy
/// what a human might want to recover. Entries a hand edit made out of bounds
/// are dropped.
pub fn read(path: &Path) -> Vec<Decal> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            tracing::warn!("decals file unreadable ({err}); quarantining");
            crate::session::quarantine(path, "unreadable");
            return Vec::new();
        }
    };
    match serde_json::from_str::<DecalFile>(&content) {
        Ok(file) if file.version == VERSION => file
            .decals
            .into_iter()
            .filter(|d| problem(d.position, d.scale).is_none())
            .take(MAX_DECALS)
            .collect(),
        _ => {
            crate::session::quarantine(path, "corrupt");
            Vec::new()
        }
    }
}

/// Atomically write the decals, bottom→top.
pub fn write(path: &Path, decals: &[Decal]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(&DecalFile {
        version: VERSION,
        decals: decals.to_vec(),
    })?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    crate::session::write_private(Path::new(&tmp), path, json.as_bytes(), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decal(id: u64) -> Decal {
        Decal {
            id,
            path: PathBuf::from("/img/a.png"),
            position: [100.0, -50.0],
            scale: 1.0,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("driftwm-decals-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("decals.json")
    }

    #[test]
    fn a_written_file_reads_back() {
        let path = scratch("roundtrip");
        let decals = vec![decal(1), decal(2)];
        write(&path, &decals).unwrap();
        assert_eq!(read(&path), decals);
    }

    #[test]
    fn a_missing_file_is_a_fresh_start() {
        assert!(read(&scratch("missing")).is_empty());
    }

    #[test]
    fn a_garbled_file_is_set_aside_and_reads_empty() {
        let path = scratch("garbled");
        std::fs::write(&path, "{ nope").unwrap();
        assert!(read(&path).is_empty());
        assert!(!path.exists(), "the bad file was moved aside");
    }

    #[test]
    fn out_of_bounds_entries_are_dropped_on_read() {
        let path = scratch("bounds");
        let mut far = decal(2);
        far.position = [1e12, 0.0];
        let mut huge = decal(3);
        huge.scale = 1e6;
        write(&path, &[decal(1), far, huge]).unwrap();
        assert_eq!(read(&path), vec![decal(1)]);
    }

    #[test]
    fn problem_rejects_non_finite_and_out_of_range() {
        assert!(problem([0.0, 0.0], 1.0).is_none());
        assert!(problem([f64::NAN, 0.0], 1.0).is_some());
        assert!(problem([0.0, f64::INFINITY], 1.0).is_some());
        assert!(problem([0.0, 0.0], 0.0).is_some());
        assert!(problem([0.0, 0.0], 1e3).is_some());
    }
}
