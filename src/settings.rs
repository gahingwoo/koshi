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
    read_key("signature")?.as_str().map(str::to_owned)
}

/// The signature to actually insert into a reply: the user's configured one,
/// or — when they have never set one — [`profile::Profile::default_signature`],
/// so a fresh install signs replies with a default the user can then edit or
/// clear in Preferences.
pub fn reply_signature() -> String {
    signature().unwrap_or_else(|| profile::cached().default_signature())
}

/// Persist `signature` as the reply signature, preserving any other settings
/// already in the file. An empty string records a deliberate "no signature".
pub fn set_signature(signature: &str) {
    write_key("signature", serde_json::Value::String(signature.to_owned()));
}

/// Whether to Cc the sender on their own replies, so a copy lands in their own
/// mailbox. On by default.
pub fn cc_self() -> bool {
    read_key("cc_self")
        .and_then(|value| value.as_bool())
        .unwrap_or(true)
}

/// Persist the "Cc myself on replies" preference.
pub fn set_cc_self(enabled: bool) {
    write_key("cc_self", serde_json::Value::Bool(enabled));
}

/// Whether outgoing replies carry a `User-Agent` header identifying Koshi as
/// the mail client. On by default: Koshi announces itself unless the user opts
/// out in Preferences (and can still delete the prefilled header per message).
pub fn send_user_agent() -> bool {
    read_key("send_user_agent")
        .and_then(|value| value.as_bool())
        .unwrap_or(true)
}

/// Persist the "identify Koshi in sent mail" preference.
pub fn set_send_user_agent(enabled: bool) {
    write_key("send_user_agent", serde_json::Value::Bool(enabled));
}

/// Read a single settings key, or `None` when the store is unset, absent, or
/// malformed.
fn read_key(key: &str) -> Option<serde_json::Value> {
    let path = STORE_PATH.with_borrow(|path| path.clone())?;
    let json = fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&json)
        .inspect_err(|err| {
            eprintln!("koshi: ignoring malformed {}: {err}", path.display());
        })
        .ok()?;
    value.get(key).cloned()
}

/// Merge `key = value` into the store, preserving every other setting.
fn write_key(key: &str, new: serde_json::Value) {
    let Some(path) = STORE_PATH.with_borrow(|path| path.clone()) else {
        return;
    };
    let mut value = fs::read_to_string(&path)
        .ok()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    value[key] = new;
    if let Err(err) = write_atomically(&path, &value.to_string()) {
        eprintln!(
            "koshi: failed to save settings to {}: {err}",
            path.display()
        );
    }
}

/// Koshi's default gap between subscription polls, in minutes.
pub const DEFAULT_POLL_INTERVAL_MINUTES: u32 = 5;

/// The narrowest and widest poll interval the Preferences spinner offers:
/// every minute at the eager end, once a day at the lazy end.
pub const MIN_POLL_INTERVAL_MINUTES: u32 = 1;
pub const MAX_POLL_INTERVAL_MINUTES: u32 = 1440;

/// How often the watcher refetches each subscribed thread, in minutes. Defaults
/// to [`DEFAULT_POLL_INTERVAL_MINUTES`] and is clamped to the spinner's range so
/// a hand-edited store can't schedule a zero-second (busy-loop) poll.
pub fn poll_interval_minutes() -> u32 {
    let stored = STORE_PATH
        .with_borrow(|path| path.clone())
        .and_then(|path| fs::read_to_string(&path).ok())
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .as_ref()
        .and_then(|value| value.get("pollIntervalMinutes"))
        .and_then(serde_json::Value::as_u64);
    match stored {
        Some(minutes) => {
            (minutes as u32).clamp(MIN_POLL_INTERVAL_MINUTES, MAX_POLL_INTERVAL_MINUTES)
        }
        None => DEFAULT_POLL_INTERVAL_MINUTES,
    }
}

/// Persist the subscription poll interval, preserving any other settings
/// already in the file.
pub fn set_poll_interval_minutes(minutes: u32) {
    let Some(path) = STORE_PATH.with_borrow(|path| path.clone()) else {
        return;
    };
    let mut value = fs::read_to_string(&path)
        .ok()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    value["pollIntervalMinutes"] = serde_json::Value::from(minutes);
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
    fn cc_self_defaults_on_and_survives_a_reload() {
        // Never set: on by default.
        assert!(cc_self());

        let store = ScratchStore::new("settings-cc-self");
        init(store.path());
        set_cc_self(false);
        assert!(!cc_self());
        set_cc_self(true);
        assert!(cc_self());
    }

    #[test]
    fn cc_self_and_signature_share_the_store() {
        // The two preferences must not clobber each other.
        let store = ScratchStore::new("settings-two-keys");
        init(store.path());
        set_signature("-- \nNika");
        set_cc_self(false);
        assert_eq!(signature().as_deref(), Some("-- \nNika"));
        assert!(!cc_self());
    }

    #[test]
    fn user_agent_defaults_on_and_survives_a_reload() {
        // Never set: Koshi announces itself by default.
        assert!(send_user_agent());

        let store = ScratchStore::new("settings-user-agent");
        init(store.path());
        set_send_user_agent(false);
        assert!(!send_user_agent());
        set_send_user_agent(true);
        assert!(send_user_agent());
    }

    #[test]
    fn poll_interval_defaults_without_a_store() {
        assert_eq!(poll_interval_minutes(), DEFAULT_POLL_INTERVAL_MINUTES);
    }

    #[test]
    fn poll_interval_survives_a_reload() {
        let store = ScratchStore::new("settings-poll");
        init(store.path());
        set_poll_interval_minutes(15);
        assert_eq!(poll_interval_minutes(), 15);
    }

    #[test]
    fn poll_interval_is_clamped_to_the_spinner_range() {
        let store = ScratchStore::new("settings-poll-clamp");
        init(store.path());
        // A hand-edited store must never schedule a zero-minute busy loop.
        fs::write(store.path(), r#"{"pollIntervalMinutes": 0}"#).unwrap();
        assert_eq!(poll_interval_minutes(), MIN_POLL_INTERVAL_MINUTES);
        fs::write(store.path(), r#"{"pollIntervalMinutes": 99999}"#).unwrap();
        assert_eq!(poll_interval_minutes(), MAX_POLL_INTERVAL_MINUTES);
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
