use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

/// Window geometry persisted between runs so a window reopens the way it was
/// last left. See [`crate::apply_window_state`] for how it is restored.
#[derive(Clone, Copy)]
pub struct WindowState {
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
    pub fullscreen: bool,
}

thread_local! {
    static STORE_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Point the store at `path`; loads and saves are no-ops until this is called.
pub fn init(path: PathBuf) {
    STORE_PATH.set(Some(path));
}

/// The last saved state, or `None` on the very first launch (no store yet) or
/// if the store is missing or corrupt — the caller treats that as first-run.
pub fn load() -> Option<WindowState> {
    let path = STORE_PATH.with_borrow(|path| path.clone())?;
    let json = fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&json)
        .inspect_err(|err| {
            eprintln!("koshi: ignoring malformed {}: {err}", path.display());
        })
        .ok()?;
    let int = |key: &str| value.get(key).and_then(serde_json::Value::as_i64);
    let flag = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    Some(WindowState {
        width: int("width")? as i32,
        height: int("height")? as i32,
        maximized: flag("maximized"),
        fullscreen: flag("fullscreen"),
    })
}

/// Persist `state`, to be restored on the next launch.
pub fn save(state: WindowState) {
    let Some(path) = STORE_PATH.with_borrow(|path| path.clone()) else {
        return;
    };
    let value = serde_json::json!({
        "width": state.width,
        "height": state.height,
        "maximized": state.maximized,
        "fullscreen": state.fullscreen,
    });
    if let Err(err) = write_atomically(&path, &value.to_string()) {
        eprintln!(
            "koshi: failed to save window state to {}: {err}",
            path.display()
        );
    }
}

/// Write via a temp file and rename so a crash mid-write can't truncate
/// the store.
fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch store file in a per-test temp dir; the dir is removed when
    /// the guard drops.
    struct ScratchStore {
        dir: PathBuf,
    }

    impl ScratchStore {
        fn new(test_name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("koshi-{test_name}-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn path(&self) -> PathBuf {
            self.dir.join("window-state.json")
        }
    }

    impl Drop for ScratchStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    // The store path is thread-local and each #[test] runs on its own thread,
    // so these tests each see a fresh, isolated store.
    #[test]
    fn no_store_reads_as_first_launch() {
        assert!(load().is_none());
    }

    #[test]
    fn state_survives_a_reload() {
        let store = ScratchStore::new("window-state-survives");
        init(store.path());
        save(WindowState {
            width: 1280,
            height: 800,
            maximized: true,
            fullscreen: false,
        });

        let reloaded = load().expect("state must round-trip");
        assert_eq!(reloaded.width, 1280);
        assert_eq!(reloaded.height, 800);
        assert!(reloaded.maximized);
        assert!(!reloaded.fullscreen);
    }

    #[test]
    fn missing_store_file_reads_as_first_launch() {
        let store = ScratchStore::new("window-state-missing");
        init(store.path());
        assert!(load().is_none());
    }

    #[test]
    fn malformed_store_reads_as_first_launch() {
        let store = ScratchStore::new("window-state-malformed");
        fs::write(store.path(), "not json").unwrap();
        init(store.path());
        assert!(load().is_none());
    }

    #[test]
    fn entries_missing_size_read_as_first_launch() {
        let store = ScratchStore::new("window-state-partial");
        fs::write(store.path(), r#"{"maximized": true}"#).unwrap();
        init(store.path());
        assert!(load().is_none(), "without a size the state is unusable");
    }
}
