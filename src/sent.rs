//! A local log of replies Koshi has sent, so "did I already reply to this"
//! and "what did I say" don't depend on lore.kernel.org having indexed the
//! message yet (it can take minutes). Purely a history: Koshi does not use
//! this to decide anything about sending itself, and nothing here is ever
//! sent anywhere — it is written only after `git send-email` has already
//! confirmed delivery.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

/// One reply Koshi has sent. `list`/`thread_message_id` are the thread it was
/// sent into, when it was a reply to one Koshi had open (a from-scratch
/// compose has neither) — enough to reopen that thread the same way a
/// favorite or subscription does.
#[derive(Clone)]
pub struct SentMessage {
    pub subject: String,
    pub to: String,
    pub cc: String,
    /// RFC 2822 send time, as `humantime`-free local formatting already used
    /// elsewhere in Koshi (see `Mail::date`) — a plain string, not parsed
    /// back into anything.
    pub sent_at: String,
    pub list: Option<String>,
    pub thread_message_id: Option<String>,
}

thread_local! {
    static SENT: RefCell<Vec<SentMessage>> = const { RefCell::new(Vec::new()) };
    static STORE_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Load sent history from `path` and persist every later record back to it.
/// Without this call the store is in-memory only.
pub fn init(path: PathBuf) {
    if let Some(sent) = load(&path) {
        SENT.set(sent);
    }
    STORE_PATH.set(Some(path));
}

fn load(path: &Path) -> Option<Vec<SentMessage>> {
    let json = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&json)
        .inspect_err(|err| {
            log::warn!("ignoring malformed {}: {err}", path.display());
        })
        .ok()?;
    let items = value.get("sent").and_then(serde_json::Value::as_array)?;
    let text = |item: &serde_json::Value, key: &str| {
        item.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    Some(
        items
            .iter()
            .filter_map(|item| {
                Some(SentMessage {
                    subject: text(item, "subject")?,
                    to: text(item, "to")?,
                    cc: text(item, "cc")?,
                    sent_at: text(item, "sent_at")?,
                    list: text(item, "list"),
                    thread_message_id: text(item, "thread_message_id"),
                })
            })
            .collect(),
    )
}

fn save() {
    let Some(path) = STORE_PATH.with_borrow(|path| path.clone()) else {
        return;
    };
    let sent: Vec<_> = SENT.with_borrow(|sent| {
        sent.iter()
            .map(|msg| {
                serde_json::json!({
                    "subject": msg.subject,
                    "to": msg.to,
                    "cc": msg.cc,
                    "sent_at": msg.sent_at,
                    "list": msg.list,
                    "thread_message_id": msg.thread_message_id,
                })
            })
            .collect()
    });
    let value = serde_json::json!({ "sent": sent });
    if let Err(err) = write_atomically(&path, &value.to_string()) {
        log::error!("failed to save sent history to {}: {err}", path.display());
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

/// Record a successfully sent message. Call only after `git send-email` has
/// confirmed delivery (see `send::Outcome::Sent`) — this is a history, not a
/// send queue, so there is nothing to retry or undo here.
pub fn record(msg: SentMessage) {
    SENT.with_borrow_mut(|sent| sent.push(msg));
    save();
}

/// Every sent message, most recent first.
pub fn all() -> Vec<SentMessage> {
    SENT.with_borrow(|sent| sent.iter().rev().cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(subject: &str) -> SentMessage {
        SentMessage {
            subject: subject.to_string(),
            to: "maintainer@example.org".to_string(),
            cc: "list@vger.kernel.org".to_string(),
            sent_at: "Thu, 3 Jul 2026 12:00:00 +0000".to_string(),
            list: Some("rockchip".to_string()),
            thread_message_id: Some("<root@example>".to_string()),
        }
    }

    #[test]
    fn all_is_empty_without_a_store() {
        assert!(all().is_empty());
    }

    #[test]
    fn record_appends_and_all_returns_most_recent_first() {
        record(msg("[PATCH v1] first"));
        record(msg("[PATCH v2] second"));
        let sent = all();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].subject, "[PATCH v2] second");
        assert_eq!(sent[1].subject, "[PATCH v1] first");
    }

    #[test]
    fn record_keeps_every_send_even_with_the_same_subject() {
        // Unlike favorites/subscriptions, history is not keyed or deduped -
        // resending (e.g. a v2) is a distinct, equally real event.
        record(msg("[PATCH] retry"));
        record(msg("[PATCH] retry"));
        assert_eq!(all().len(), 2);
    }

    /// A scratch store file in a per-test temp dir; the dir is removed
    /// when the guard drops.
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
            self.dir.join("sent.json")
        }
    }

    impl Drop for ScratchStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn sent_history_survives_a_reload() {
        let store = ScratchStore::new("sent-survives-a-reload");
        init(store.path());
        record(msg("[PATCH] persisted"));

        SENT.set(Vec::new());
        init(store.path());

        let sent = all();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].subject, "[PATCH] persisted");
        assert_eq!(sent[0].to, "maintainer@example.org");
        assert_eq!(sent[0].list.as_deref(), Some("rockchip"));
        assert_eq!(sent[0].thread_message_id.as_deref(), Some("<root@example>"));
    }

    #[test]
    fn a_from_scratch_compose_has_no_thread_to_link_back_to() {
        let store = ScratchStore::new("sent-no-thread");
        init(store.path());
        record(SentMessage {
            list: None,
            thread_message_id: None,
            ..msg("[ANNOUNCE] something")
        });
        let sent = all();
        assert!(sent[0].list.is_none());
        assert!(sent[0].thread_message_id.is_none());
    }

    #[test]
    fn malformed_store_file_starts_empty_and_is_replaced_on_save() {
        let store = ScratchStore::new("sent-malformed-store");
        fs::write(store.path(), "not json").unwrap();
        init(store.path());
        assert!(all().is_empty());

        record(msg("[PATCH] after malformed"));
        SENT.set(Vec::new());
        init(store.path());
        assert_eq!(all().len(), 1);
    }

    #[test]
    fn missing_store_file_starts_empty() {
        let store = ScratchStore::new("sent-missing-store");
        init(store.path());
        assert!(all().is_empty());
    }

    #[test]
    fn entries_missing_required_fields_are_skipped_not_fatal() {
        let store = ScratchStore::new("sent-entries-missing-fields");
        fs::write(
            store.path(),
            r#"{"sent": [{"subject": "no to/cc/sent_at"}, {"subject": "ok", "to": "a@b", "cc": "", "sent_at": "now"}]}"#,
        )
        .unwrap();
        init(store.path());
        assert_eq!(all().len(), 1, "the incomplete entry must be dropped");
        assert_eq!(all()[0].subject, "ok");
    }
}
