use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

/// A subscribed thread, keyed by the Message-ID of the message it was
/// subscribed from. A subscription marks a thread to be watched: the
/// background watcher ([`crate::watcher`]) refetches it on an interval and
/// raises a notification for every message it has not seen before.
#[derive(Clone)]
pub struct Subscription {
    pub message_id: String,
    pub subject: String,
    pub date: String,
    /// The lore list the mail was opened from, used to fetch it again.
    pub list: String,
    /// Message-IDs the watcher has already accounted for on this thread. A
    /// fresh subscription starts empty and is seeded — with whatever the thread
    /// holds *now* — the moment it is added (or, failing that, on the next
    /// poll), silently, so subscribing never dumps the existing backlog as
    /// notifications. Only genuinely new arrivals do.
    pub seen: Vec<String>,
}

thread_local! {
    static SUBSCRIPTIONS: RefCell<Vec<Subscription>> = const { RefCell::new(Vec::new()) };
    static STORE_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Load subscriptions from `path` and persist every later change back to it.
/// Without this call the store is in-memory only.
pub fn init(path: PathBuf) {
    if let Some(mails) = load(&path) {
        SUBSCRIPTIONS.set(mails);
    }
    STORE_PATH.set(Some(path));
}

fn load(path: &Path) -> Option<Vec<Subscription>> {
    let json = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&json)
        .inspect_err(|err| {
            eprintln!("koshi: ignoring malformed {}: {err}", path.display());
        })
        .ok()?;
    let text = |item: &serde_json::Value, key: &str| {
        item.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let strings = |item: &serde_json::Value, key: &str| {
        item.get(key)
            .and_then(serde_json::Value::as_array)
            .map(|array| {
                array
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mails = value
        .get("mails")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|item| {
            Some(Subscription {
                message_id: text(item, "message_id")?,
                subject: text(item, "subject")?,
                date: text(item, "date")?,
                list: text(item, "list")?,
                // Absent on subscriptions written by an older Koshi; an empty
                // seen list just means the next poll reseeds the baseline.
                seen: strings(item, "seen"),
            })
        })
        .collect();
    Some(mails)
}

fn save() {
    let Some(path) = STORE_PATH.with_borrow(|path| path.clone()) else {
        return;
    };
    let mails: Vec<_> = SUBSCRIPTIONS.with_borrow(|subs| {
        subs.iter()
            .map(|sub| {
                serde_json::json!({
                    "message_id": sub.message_id,
                    "subject": sub.subject,
                    "date": sub.date,
                    "list": sub.list,
                    "seen": sub.seen,
                })
            })
            .collect()
    });
    let value = serde_json::json!({ "mails": mails });
    if let Err(err) = write_atomically(&path, &value.to_string()) {
        eprintln!(
            "koshi: failed to save subscriptions to {}: {err}",
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

pub fn is_subscribed(message_id: &str) -> bool {
    SUBSCRIPTIONS.with_borrow(|subs| subs.iter().any(|sub| sub.message_id == message_id))
}

/// Flip the subscription state of `sub` and return the new state
/// (true = now subscribed).
pub fn toggle(sub: Subscription) -> bool {
    let subscribed = SUBSCRIPTIONS.with_borrow_mut(|subs| {
        match subs.iter().position(|s| s.message_id == sub.message_id) {
            Some(index) => {
                subs.remove(index);
                false
            }
            None => {
                subs.push(sub);
                true
            }
        }
    });
    save();
    subscribed
}

/// A snapshot of every current subscription, for the watcher to poll.
pub fn all() -> Vec<Subscription> {
    SUBSCRIPTIONS.with_borrow(|subs| subs.clone())
}

/// Replace the seen-Message-ID set of the subscription keyed by `message_id`
/// and persist it, so the watcher only ever notifies once per arrival. A no-op
/// if the subscription was removed while a poll was in flight.
pub fn set_seen(message_id: &str, seen: Vec<String>) {
    let changed = SUBSCRIPTIONS.with_borrow_mut(|subs| {
        match subs.iter_mut().find(|sub| sub.message_id == message_id) {
            Some(sub) => {
                sub.seen = seen;
                true
            }
            None => false,
        }
    });
    if changed {
        save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(id: &str) -> Subscription {
        Subscription {
            message_id: id.to_string(),
            subject: format!("subject for {id}"),
            date: "Thu, 3 Jul 2026 12:00:00 +0000".to_string(),
            list: "lkml".to_string(),
            seen: Vec::new(),
        }
    }

    // The store is thread-local and each #[test] runs on its own thread, so
    // these tests each see a fresh, isolated store.
    #[test]
    fn toggle_adds_when_absent_and_removes_when_present() {
        assert!(!is_subscribed("<a@example>"));
        assert!(toggle(sub("<a@example>")), "first toggle must add");
        assert!(is_subscribed("<a@example>"));
        assert!(!toggle(sub("<a@example>")), "second toggle must remove");
        assert!(!is_subscribed("<a@example>"));
        assert!(all().is_empty());
    }

    #[test]
    fn repeated_toggles_never_duplicate() {
        for _ in 0..3 {
            toggle(sub("<b@example>"));
            toggle(sub("<b@example>"));
        }
        assert!(toggle(sub("<b@example>")));
        let matches = all()
            .iter()
            .filter(|s| s.message_id == "<b@example>")
            .count();
        assert_eq!(matches, 1);
    }

    #[test]
    fn subscriptions_are_keyed_by_message_id_only() {
        assert!(toggle(sub("<c@example>")));
        let same_id_different_subject = Subscription {
            subject: "another subject".to_string(),
            ..sub("<c@example>")
        };
        assert!(!toggle(same_id_different_subject), "must remove, not add");
        assert!(all().is_empty());
    }

    /// A scratch store file in a per-test temp dir; the dir is removed when
    /// the guard drops.
    struct ScratchStore {
        dir: std::path::PathBuf,
    }

    impl ScratchStore {
        fn new(test_name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("koshi-{test_name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn path(&self) -> std::path::PathBuf {
            self.dir.join("subscriptions.json")
        }
    }

    impl Drop for ScratchStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn subscriptions_survive_a_reload() {
        let store = ScratchStore::new("subscriptions-survive-a-reload");
        init(store.path());
        toggle(sub("<p@example>"));

        // Wipe the in-memory store to prove the reload comes from disk.
        SUBSCRIPTIONS.set(Vec::new());
        init(store.path());

        assert!(is_subscribed("<p@example>"));
        let reloaded = all();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].subject, "subject for <p@example>");
        assert_eq!(reloaded[0].list, "lkml");
    }

    #[test]
    fn seen_ids_are_recorded_and_survive_a_reload() {
        let store = ScratchStore::new("subscriptions-seen-survive");
        init(store.path());
        toggle(sub("<s@example>"));
        set_seen(
            "<s@example>",
            vec!["<a@x>".to_string(), "<b@x>".to_string()],
        );

        SUBSCRIPTIONS.set(Vec::new());
        init(store.path());

        let reloaded = all();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].seen, vec!["<a@x>", "<b@x>"]);
    }

    #[test]
    fn set_seen_on_a_missing_subscription_is_a_noop() {
        let store = ScratchStore::new("subscriptions-seen-missing");
        init(store.path());
        // Nothing subscribed; must not panic or create a phantom entry.
        set_seen("<gone@example>", vec!["<a@x>".to_string()]);
        assert!(all().is_empty());
    }

    #[test]
    fn unsubscribing_persists_too() {
        let store = ScratchStore::new("unsubscribing-persists-too");
        init(store.path());
        toggle(sub("<q@example>"));
        toggle(sub("<q@example>"));

        SUBSCRIPTIONS.set(vec![sub("<q@example>")]);
        init(store.path());
        assert!(!is_subscribed("<q@example>"));
    }

    #[test]
    fn malformed_store_file_starts_empty() {
        let store = ScratchStore::new("malformed-subscriptions-store");
        std::fs::write(store.path(), "not json").unwrap();
        init(store.path());
        assert!(all().is_empty());
    }
}
