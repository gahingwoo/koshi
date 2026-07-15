//! Koshi's own user preferences, kept in a small JSON file under
//! `$XDG_CONFIG_HOME/koshi/`. Unlike [`crate::profile`] — which reads the git
//! configuration that *is* the account (name, email, send-email) — these are
//! Koshi's presentation choices, so they live in the config dir rather than in
//! git config or the data dir used for window state and favorites.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

use crate::profile;

thread_local! {
    static STORE_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Point the settings store at `path`; loads and saves are no-ops until this
/// is called.
pub fn init(path: PathBuf) {
    STORE_PATH.set(Some(path));
}

/// The reply signature the user set in Preferences, or `None` if they never
/// touched it (so callers fall back to the default). An empty string is a
/// deliberate "no signature" and is returned as `Some("")`, overriding the
/// default.
pub fn signature() -> Option<String> {
    let path = STORE_PATH.with_borrow(|path| path.clone())?;
    let json = fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&json)
        .inspect_err(|err| {
            eprintln!("koshi: ignoring malformed {}: {err}", path.display());
        })
        .ok()?;
    value
        .get("signature")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// The signature to actually insert into a reply: the user's configured one,
/// or — when they have never set one — [`profile::Profile::default_signature`],
/// so a fresh install signs replies with a default the user can then edit or
/// clear in Preferences.
pub fn reply_signature() -> String {
    signature().unwrap_or_else(|| profile::load().default_signature())
}

/// Persist `signature` as the reply signature, preserving any other settings
/// already in the file. An empty string records a deliberate "no signature".
pub fn set_signature(signature: &str) {
    let Some(path) = STORE_PATH.with_borrow(|path| path.clone()) else {
        return;
    };
    let mut value = fs::read_to_string(&path)
        .ok()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    value["signature"] = serde_json::Value::String(signature.to_owned());
    if let Err(err) = write_atomically(&path, &value.to_string()) {
        eprintln!(
            "koshi: failed to save settings to {}: {err}",
            path.display()
        );
    }
}

/// Whether outgoing replies carry a `User-Agent` header identifying Koshi as
/// the mail client. Defaults to `true`: Koshi announces itself unless the user
/// opts out in Preferences.
pub fn send_user_agent() -> bool {
    let Some(path) = STORE_PATH.with_borrow(|path| path.clone()) else {
        return true;
    };
    fs::read_to_string(&path)
        .ok()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .as_ref()
        .and_then(|value| value.get("sendUserAgent"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

/// Persist whether replies carry a `User-Agent` header, preserving any other
/// settings already in the file.
pub fn set_send_user_agent(enabled: bool) {
    let Some(path) = STORE_PATH.with_borrow(|path| path.clone()) else {
        return;
    };
    let mut value = fs::read_to_string(&path)
        .ok()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    value["sendUserAgent"] = serde_json::Value::Bool(enabled);
    if let Err(err) = write_atomically(&path, &value.to_string()) {
        eprintln!(
            "koshi: failed to save settings to {}: {err}",
            path.display()
        );
    }
}

/// Write via a temp file and rename so a crash mid-write can't truncate the
/// store.
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
            self.dir.join("settings.json")
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
    fn no_store_has_no_signature() {
        assert!(signature().is_none());
    }

    #[test]
    fn signature_survives_a_reload() {
        let store = ScratchStore::new("settings-survives");
        init(store.path());
        set_signature("-- \nNika Krasnova");
        assert_eq!(signature().as_deref(), Some("-- \nNika Krasnova"));
    }

    #[test]
    fn empty_signature_is_recorded_as_deliberate() {
        // Clearing the box means "no signature", distinct from never having
        // set one, so it is stored rather than dropped.
        let store = ScratchStore::new("settings-empty");
        init(store.path());
        set_signature("");
        assert_eq!(signature().as_deref(), Some(""));
    }

    #[test]
    fn user_agent_defaults_on_without_a_store() {
        // A fresh install (or a store that never recorded the choice) announces
        // Koshi.
        assert!(send_user_agent());
    }

    #[test]
    fn user_agent_choice_survives_a_reload() {
        let store = ScratchStore::new("settings-ua");
        init(store.path());
        set_send_user_agent(false);
        assert!(!send_user_agent());
        set_send_user_agent(true);
        assert!(send_user_agent());
    }

    #[test]
    fn set_user_agent_preserves_the_signature() {
        let store = ScratchStore::new("settings-ua-preserve");
        init(store.path());
        set_signature("sig");
        set_send_user_agent(false);
        assert_eq!(signature().as_deref(), Some("sig"));
        assert!(!send_user_agent());
    }

    #[test]
    fn set_signature_preserves_other_settings() {
        let store = ScratchStore::new("settings-preserve");
        fs::write(store.path(), r#"{"other": 42}"#).unwrap();
        init(store.path());
        set_signature("sig");
        let json = fs::read_to_string(store.path()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value.get("other").and_then(serde_json::Value::as_i64),
            Some(42)
        );
        assert_eq!(
            value.get("signature").and_then(serde_json::Value::as_str),
            Some("sig")
        );
    }
}
