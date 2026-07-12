use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

/// A starred mail, keyed by its Message-ID.
#[derive(Clone)]
pub struct Favorite {
    pub message_id: String,
    pub subject: String,
    pub date: String,
    /// The lore list the mail was opened from, used to fetch it again.
    pub list: String,
}

/// A starred inbox, keyed by its lore slug.
#[derive(Clone)]
pub struct FavoriteInbox {
    pub slug: String,
    pub description: String,
}

thread_local! {
    static FAVORITES: RefCell<Vec<Favorite>> = const { RefCell::new(Vec::new()) };
    static FAVORITE_INBOXES: RefCell<Vec<FavoriteInbox>> = const { RefCell::new(Vec::new()) };
    static STORE_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Load favorites from `path` and persist every later change back to it.
/// Without this call the store is in-memory only.
pub fn init(path: PathBuf) {
    if let Some((mails, inboxes)) = load(&path) {
        FAVORITES.set(mails);
        FAVORITE_INBOXES.set(inboxes);
    }
    STORE_PATH.set(Some(path));
}

fn load(path: &Path) -> Option<(Vec<Favorite>, Vec<FavoriteInbox>)> {
    let json = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&json)
        .inspect_err(|err| {
            eprintln!("koshi: ignoring malformed {}: {err}", path.display());
        })
        .ok()?;
    let items = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let text = |item: &serde_json::Value, key: &str| {
        item.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let mails = items("mails")
        .iter()
        .filter_map(|item| {
            Some(Favorite {
                message_id: text(item, "message_id")?,
                subject: text(item, "subject")?,
                date: text(item, "date")?,
                list: text(item, "list")?,
            })
        })
        .collect();
    let inboxes = items("inboxes")
        .iter()
        .filter_map(|item| {
            Some(FavoriteInbox {
                slug: text(item, "slug")?,
                description: text(item, "description")?,
            })
        })
        .collect();
    Some((mails, inboxes))
}

fn save() {
    let Some(path) = STORE_PATH.with_borrow(|path| path.clone()) else {
        return;
    };
    let mails: Vec<_> = FAVORITES.with_borrow(|favs| {
        favs.iter()
            .map(|fav| {
                serde_json::json!({
                    "message_id": fav.message_id,
                    "subject": fav.subject,
                    "date": fav.date,
                    "list": fav.list,
                })
            })
            .collect()
    });
    let inboxes: Vec<_> = FAVORITE_INBOXES.with_borrow(|favs| {
        favs.iter()
            .map(|fav| {
                serde_json::json!({
                    "slug": fav.slug,
                    "description": fav.description,
                })
            })
            .collect()
    });
    let value = serde_json::json!({ "mails": mails, "inboxes": inboxes });
    if let Err(err) = write_atomically(&path, &value.to_string()) {
        eprintln!(
            "koshi: failed to save favorites to {}: {err}",
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

pub fn is_favorite(message_id: &str) -> bool {
    FAVORITES.with_borrow(|favs| favs.iter().any(|fav| fav.message_id == message_id))
}

/// Flip the favorite state of `fav` and return the new state
/// (true = now favorited).
pub fn toggle(fav: Favorite) -> bool {
    let starred = FAVORITES.with_borrow_mut(|favs| {
        match favs.iter().position(|f| f.message_id == fav.message_id) {
            Some(index) => {
                favs.remove(index);
                false
            }
            None => {
                favs.push(fav);
                true
            }
        }
    });
    save();
    starred
}

pub fn all() -> Vec<Favorite> {
    FAVORITES.with_borrow(|favs| favs.clone())
}

pub fn is_favorite_inbox(slug: &str) -> bool {
    FAVORITE_INBOXES.with_borrow(|favs| favs.iter().any(|fav| fav.slug == slug))
}

/// Flip the favorite state of `fav` and return the new state
/// (true = now favorited).
pub fn toggle_inbox(fav: FavoriteInbox) -> bool {
    let starred = FAVORITE_INBOXES.with_borrow_mut(|favs| {
        match favs.iter().position(|f| f.slug == fav.slug) {
            Some(index) => {
                favs.remove(index);
                false
            }
            None => {
                favs.push(fav);
                true
            }
        }
    });
    save();
    starred
}

pub fn all_inboxes() -> Vec<FavoriteInbox> {
    FAVORITE_INBOXES.with_borrow(|favs| favs.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fav(id: &str) -> Favorite {
        Favorite {
            message_id: id.to_string(),
            subject: format!("subject for {id}"),
            date: "Thu, 3 Jul 2026 12:00:00 +0000".to_string(),
            list: "lkml".to_string(),
        }
    }

    // The store is thread-local and each #[test] runs on its own thread, so
    // these tests each see a fresh, isolated store. Persistence only kicks
    // in after init(), so tests that never call it touch no files.
    #[test]
    fn toggle_adds_when_absent_and_removes_when_present() {
        assert!(!is_favorite("<a@example>"));
        assert!(toggle(fav("<a@example>")), "first toggle must add");
        assert!(is_favorite("<a@example>"));
        assert!(!toggle(fav("<a@example>")), "second toggle must remove");
        assert!(!is_favorite("<a@example>"));
        assert!(all().is_empty());
    }

    #[test]
    fn repeated_toggles_never_duplicate() {
        for _ in 0..3 {
            toggle(fav("<b@example>"));
            toggle(fav("<b@example>"));
        }
        assert!(toggle(fav("<b@example>")));
        let matches = all()
            .iter()
            .filter(|f| f.message_id == "<b@example>")
            .count();
        assert_eq!(matches, 1);
    }

    fn inbox(slug: &str) -> FavoriteInbox {
        FavoriteInbox {
            slug: slug.to_string(),
            description: format!("description for {slug}"),
        }
    }

    #[test]
    fn inbox_toggle_adds_when_absent_and_removes_when_present() {
        assert!(!is_favorite_inbox("lkml"));
        assert!(toggle_inbox(inbox("lkml")), "first toggle must add");
        assert!(is_favorite_inbox("lkml"));
        assert!(!toggle_inbox(inbox("lkml")), "second toggle must remove");
        assert!(!is_favorite_inbox("lkml"));
        assert!(all_inboxes().is_empty());
    }

    #[test]
    fn favorite_inboxes_are_keyed_by_slug_only() {
        assert!(toggle_inbox(inbox("bpf")));
        // Same slug with a different description still counts as present.
        let same_slug_different_description = FavoriteInbox {
            description: "another description".to_string(),
            ..inbox("bpf")
        };
        assert!(
            !toggle_inbox(same_slug_different_description),
            "must remove, not add"
        );
        assert!(all_inboxes().is_empty());
    }

    #[test]
    fn mail_and_inbox_stores_are_independent() {
        toggle(fav("<d@example>"));
        toggle_inbox(inbox("netdev"));
        assert!(toggle_inbox(inbox("<d@example>")), "no cross-store key hit");
        assert_eq!(all().len(), 1);
        assert_eq!(all_inboxes().len(), 2);
    }

    /// A scratch store file in a per-test temp dir; the dir is removed
    /// when the guard drops.
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
            self.dir.join("favorites.json")
        }
    }

    impl Drop for ScratchStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn favorites_survive_a_reload() {
        let store = ScratchStore::new("favorites-survive-a-reload");
        init(store.path());
        toggle(fav("<p@example>"));
        toggle_inbox(inbox("rust-for-linux"));

        // Wipe the in-memory store to prove the reload comes from disk.
        FAVORITES.set(Vec::new());
        FAVORITE_INBOXES.set(Vec::new());
        init(store.path());

        assert!(is_favorite("<p@example>"));
        assert!(is_favorite_inbox("rust-for-linux"));
        let reloaded = all();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].subject, "subject for <p@example>");
        assert_eq!(reloaded[0].date, "Thu, 3 Jul 2026 12:00:00 +0000");
        assert_eq!(reloaded[0].list, "lkml");
        assert_eq!(
            all_inboxes()[0].description,
            "description for rust-for-linux"
        );
    }

    #[test]
    fn unstarring_persists_too() {
        let store = ScratchStore::new("unstarring-persists-too");
        init(store.path());
        toggle(fav("<q@example>"));
        toggle(fav("<q@example>"));

        FAVORITES.set(vec![fav("<q@example>")]);
        init(store.path());
        assert!(!is_favorite("<q@example>"));
    }

    #[test]
    fn malformed_store_file_starts_empty_and_is_replaced_on_save() {
        let store = ScratchStore::new("malformed-store-file");
        std::fs::write(store.path(), "not json").unwrap();
        init(store.path());
        assert!(all().is_empty());
        assert!(all_inboxes().is_empty());

        toggle(fav("<r@example>"));
        FAVORITES.set(Vec::new());
        init(store.path());
        assert!(is_favorite("<r@example>"));
    }

    #[test]
    fn missing_store_file_starts_empty() {
        let store = ScratchStore::new("missing-store-file");
        init(store.path());
        assert!(all().is_empty());
        assert!(all_inboxes().is_empty());
    }

    #[test]
    fn entries_missing_fields_are_skipped_not_fatal() {
        let store = ScratchStore::new("entries-missing-fields");
        std::fs::write(
            store.path(),
            r#"{"mails": [{"message_id": "<s@example>"}], "inboxes": [{"slug": "bpf", "description": "BPF"}]}"#,
        )
        .unwrap();
        init(store.path());
        assert!(all().is_empty(), "partial mail entry must be dropped");
        assert!(is_favorite_inbox("bpf"));
    }

    #[test]
    fn favorites_are_keyed_by_message_id_only() {
        assert!(toggle(fav("<c@example>")));
        // Same Message-ID with different metadata still counts as present.
        let same_id_different_subject = Favorite {
            subject: "another subject".to_string(),
            ..fav("<c@example>")
        };
        assert!(!toggle(same_id_different_subject), "must remove, not add");
        assert!(all().is_empty());
    }
}
