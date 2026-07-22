//! Durable session store: the versioned `session.json` envelope plus its
//! robustness rules (atomic write, corrupt-file quarantine, origin filtering,
//! carry-forward).
//!
//! This is the smithay-free half of session restore: pure serde types and file
//! IO over a path. The compositor-side glue that builds an envelope from live
//! state and materializes it back into suspended windows lives in the bin crate
//! (`state/session_store.rs`).
//!
//! Path: `$XDG_STATE_HOME/driftwm/session.json` (`~/.local/state/driftwm/`).
//! Distinct from the runtime tmpfs state file, which is wiped on logout.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The current on-disk schema version. A file with any other version is treated
/// as unreadable (quarantined), so a downgrade never misparses a newer schema.
pub const VERSION: u32 = 1;

/// Why a durable entry exists, which decides whether it materializes on restore.
/// `Explicit` (a live suspend) always comes back; `Quit` (serialized at
/// graceful shutdown) only when `restore_windows` is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Explicit,
    Quit,
}

/// One saved window. `position` is in Y-up rule coordinates (window center),
/// matching the window-rules / state-file convention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: u64,
    pub app_id: String,
    pub desktop_id: String,
    pub display_name: String,
    pub title: String,
    pub position: [i32; 2],
    pub size: [i32; 2],
    pub origin: Origin,
    /// Whether the suspended window's origin was client-decorated. Every
    /// stand-in is barred; a CSD origin shrank its body under the bar (this
    /// entry's rect is that shrunken body) so adopt can reassemble the full
    /// geometry. Additive: a file without this field defaults to `false`
    /// (SSD-origin).
    #[serde(default)]
    pub csd: bool,
}

/// A per-output camera/zoom, mirroring the runtime state file's shape.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SessionOutput {
    pub camera: [f64; 2],
    pub zoom: f64,
}

/// The whole durable session: entries bottom→top (z-order restores in file
/// order), plus per-output cameras keyed by output name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEnvelope {
    pub version: u32,
    /// Unix seconds at write time. Informational (for humans inspecting the
    /// file); nothing reads it back.
    pub saved_at: u64,
    pub entries: Vec<SessionEntry>,
    pub outputs: std::collections::BTreeMap<String, SessionOutput>,
}

impl SessionEnvelope {
    /// A fresh, empty envelope at the current version.
    pub fn empty() -> Self {
        Self {
            version: VERSION,
            saved_at: 0,
            entries: Vec::new(),
            outputs: std::collections::BTreeMap::new(),
        }
    }
}

/// `$XDG_STATE_HOME/driftwm/session.json`, falling back to
/// `$HOME/.local/state/driftwm/session.json`. `None` when neither is set.
pub fn default_session_path() -> Option<PathBuf> {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state_home.join("driftwm").join("session.json"))
}

/// Read and parse the session file. An unparseable file (or a version
/// mismatch) is quarantined to `session.json.corrupt.<unix-ts>`, an unreadable
/// one to `session.json.unreadable.<unix-ts>`; both return an empty envelope —
/// a bad file never crashes startup and never silently overwrites a file a
/// human might want to recover. A missing file is the normal fresh start.
pub fn read(path: &Path) -> SessionEnvelope {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return SessionEnvelope::empty();
        }
        // Permissions, is-a-directory, IO — anything but "missing" must not
        // read as an empty session, or the next durable write renames a fresh
        // envelope over data that was never read.
        Err(err) => {
            tracing::warn!("session store unreadable ({err}); quarantining");
            quarantine(path, "unreadable");
            return SessionEnvelope::empty();
        }
    };
    match serde_json::from_str::<SessionEnvelope>(&content) {
        Ok(envelope) if envelope.version == VERSION => envelope,
        _ => {
            quarantine(path, "corrupt");
            SessionEnvelope::empty()
        }
    }
}

/// Atomically write the envelope: serialize to a sibling `.tmp`, then rename
/// over the target. `fsync` flushes the file before the rename (the shutdown
/// write); steady-state writes skip it to stay off the blocking path.
pub fn write(path: &Path, envelope: &SessionEnvelope, fsync: bool) -> std::io::Result<()> {
    use std::io::Write as _;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(envelope)?;

    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);

    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(json.as_bytes())?;
    if fsync {
        file.sync_all()?;
    }
    drop(file);
    std::fs::rename(&tmp, path)
}

/// Split entries into those to materialize now and those to carry forward
/// unchanged (re-emitted on the next write), so a flag-off session never
/// destroys the saved session.
pub fn partition_for_restore(
    entries: Vec<SessionEntry>,
    restore_windows: bool,
) -> (Vec<SessionEntry>, Vec<SessionEntry>) {
    entries
        .into_iter()
        .partition(|e| restore_windows || e.origin == Origin::Explicit)
}

/// Rename a bad file aside (`.<label>.<unix-ts>`) so startup can continue from
/// empty. Best-effort: a failed rename just means the directory isn't writable,
/// so the next write's tmp+rename can't clobber anything either.
fn quarantine(path: &Path, label: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut aside = path.as_os_str().to_owned();
    aside.push(format!(".{label}.{ts}"));
    let _ = std::fs::rename(path, PathBuf::from(aside));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// Self-cleaning unique temp directory (the bin-crate fixture's is
    /// unreachable from a lib-crate test — mirror `desktop_entry`'s pattern).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("driftwm-session-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn entry(id: u64, origin: Origin) -> SessionEntry {
        SessionEntry {
            id,
            app_id: format!("app{id}"),
            desktop_id: format!("app{id}.desktop"),
            display_name: format!("App {id}"),
            title: format!("Title {id}"),
            position: [id as i32 * 10, -(id as i32) * 5],
            size: [400, 300],
            origin,
            csd: false,
        }
    }

    #[test]
    fn round_trip_preserves_envelope_and_version() {
        let tmp = TempDir::new();
        let path = tmp.path.join("session.json");
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "eDP-1".to_string(),
            SessionOutput {
                camera: [-960.0, -540.0],
                zoom: 1.25,
            },
        );
        let envelope = SessionEnvelope {
            version: VERSION,
            saved_at: 123,
            entries: vec![entry(1, Origin::Explicit), entry(2, Origin::Quit)],
            outputs,
        };

        write(&path, &envelope, false).unwrap();
        // The serialized file carries the version field.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"version\""));

        let read_back = read(&path);
        assert_eq!(read_back, envelope);
        assert_eq!(read_back.version, VERSION);
    }

    #[test]
    fn csd_round_trips_and_defaults_false_for_old_files() {
        let tmp = TempDir::new();
        let path = tmp.path.join("session.json");
        let mut csd = entry(1, Origin::Explicit);
        csd.csd = true;
        let envelope = SessionEnvelope {
            version: VERSION,
            saved_at: 0,
            entries: vec![csd, entry(2, Origin::Explicit)],
            outputs: BTreeMap::new(),
        };
        write(&path, &envelope, false).unwrap();
        let read_back = read(&path);
        assert!(read_back.entries[0].csd, "csd flag survives the write");
        assert!(!read_back.entries[1].csd);

        // A file predating the field omits it entirely and defaults to SSD.
        std::fs::write(
            &path,
            r#"{"version":1,"saved_at":0,"outputs":{},"entries":[
                {"id":1,"app_id":"a","desktop_id":"a","display_name":"A","title":"t",
                 "position":[0,0],"size":[400,300],"origin":"explicit"}]}"#,
        )
        .unwrap();
        let legacy = read(&path);
        assert_eq!(legacy.entries.len(), 1);
        assert!(
            !legacy.entries[0].csd,
            "a pre-field file defaults to an SSD-origin stand-in"
        );
    }

    #[test]
    fn corrupt_file_is_quarantined_and_reads_empty() {
        let tmp = TempDir::new();
        let path = tmp.path.join("session.json");
        std::fs::write(&path, "{ not valid json ][").unwrap();

        let envelope = read(&path);
        assert!(envelope.entries.is_empty());
        assert_eq!(envelope.version, VERSION);

        // The bad file was renamed aside, not left in place or deleted.
        assert!(!path.exists());
        let quarantined: Vec<_> = std::fs::read_dir(&tmp.path)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("session.json.corrupt.")
            })
            .collect();
        assert_eq!(quarantined.len(), 1, "exactly one quarantined copy");
    }

    #[test]
    fn version_mismatch_is_quarantined() {
        let tmp = TempDir::new();
        let path = tmp.path.join("session.json");
        // Well-formed JSON, but a schema version this build can't trust.
        std::fs::write(
            &path,
            r#"{"version":999,"saved_at":0,"entries":[],"outputs":{}}"#,
        )
        .unwrap();

        let envelope = read(&path);
        assert!(envelope.entries.is_empty());
        assert!(!path.exists(), "a future-version file is quarantined");
    }

    #[test]
    fn missing_file_reads_empty_without_quarantine() {
        let tmp = TempDir::new();
        let path = tmp.path.join("does-not-exist.json");
        let envelope = read(&path);
        assert!(envelope.entries.is_empty());
    }

    #[test]
    fn unreadable_file_is_quarantined_and_reads_empty() {
        let tmp = TempDir::new();
        let path = tmp.path.join("session.json");
        // A directory where the file should be makes `read_to_string` fail
        // with a non-`NotFound` error on every platform.
        std::fs::create_dir(&path).unwrap();

        let envelope = read(&path);
        assert!(envelope.entries.is_empty());

        // The unreadable path was renamed aside, not left in place — so the
        // next durable write can't clobber it.
        assert!(!path.exists());
        let quarantined: Vec<_> = std::fs::read_dir(&tmp.path)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("session.json.unreadable.")
            })
            .collect();
        assert_eq!(quarantined.len(), 1, "exactly one quarantined copy");
    }

    #[test]
    fn origin_filtering_with_restore_off_keeps_explicit_carries_quit() {
        let entries = vec![
            entry(1, Origin::Explicit),
            entry(2, Origin::Quit),
            entry(3, Origin::Explicit),
        ];
        let (materialize, carried) = partition_for_restore(entries, false);
        assert_eq!(
            materialize.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![1, 3],
            "explicit entries always materialize"
        );
        assert_eq!(
            carried.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![2],
            "quit entries are carried forward when restore is off"
        );
    }

    #[test]
    fn origin_filtering_with_restore_on_materializes_everything() {
        let entries = vec![entry(1, Origin::Explicit), entry(2, Origin::Quit)];
        let (materialize, carried) = partition_for_restore(entries, true);
        assert_eq!(materialize.len(), 2);
        assert!(carried.is_empty());
    }

    #[test]
    fn carry_forward_survives_a_rewrite() {
        let tmp = TempDir::new();
        let path = tmp.path.join("session.json");

        // A prior session saved one explicit + one quit entry.
        let original = SessionEnvelope {
            version: VERSION,
            saved_at: 1,
            entries: vec![entry(1, Origin::Explicit), entry(2, Origin::Quit)],
            outputs: BTreeMap::new(),
        };
        write(&path, &original, false).unwrap();

        // Restore is off: the quit entry is carried, not materialized.
        let loaded = read(&path);
        let (_materialize, carried) = partition_for_restore(loaded.entries, false);

        // The next rewrite re-emits the carried entry, so it isn't destroyed.
        let rewritten = SessionEnvelope {
            version: VERSION,
            saved_at: 2,
            entries: carried,
            outputs: BTreeMap::new(),
        };
        write(&path, &rewritten, false).unwrap();

        let after = read(&path);
        assert_eq!(after.entries, vec![entry(2, Origin::Quit)]);
    }
}
