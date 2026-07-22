//! On-disk cache of downloaded thread mboxes, so a thread you have already
//! opened reopens instantly and stays readable offline.
//!
//! Entries are the gzipped `t.mbox.gz` payloads exactly as lore served them —
//! no recompression — keyed by list and Message-ID and laid out as
//! `<cache dir>/<list>/<escaped-message-id>.mbox.gz`. The folder defaults to
//! `$XDG_CACHE_HOME/koshi/mail` and can be redirected in Preferences
//! ([`crate::settings::cache_dir`]).
//!
//! A thread is a moving target — replies keep arriving — so a cached copy only
//! substitutes for the network while it is younger than [`FRESH_FOR_SECS`].
//! After that the network is consulted again, with any stale copy kept as the
//! offline fallback. The subscription watcher and the thread page's Refresh
//! button always fetch live (see [`crate::lore::fetch_thread_mbox_live`]);
//! both still rewrite the entry, so subscribed threads stay fresh for free.
//!
//! Every operation here is best-effort: a cache that cannot be read or written
//! must never break reading mail, so failures degrade to "no cache" silently
//! (writes log a warning, since a persistently failing disk is worth noticing).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gtk::glib;

use crate::settings;

/// How long a cached thread substitutes for the network, in seconds. Long
/// enough that browsing back and forth through a thread costs one download,
/// short enough that reopening a thread after a coffee shows new replies.
pub const FRESH_FOR_SECS: u64 = 15 * 60;

/// The effective cache folder: the override chosen in Preferences, or the
/// default under the XDG cache dir. The `mail` leaf keeps thread entries apart
/// from Koshi's other cache droppings (the extracted notification icon), so
/// clearing the mail cache never touches anything else.
pub fn dir() -> PathBuf {
    settings::cache_dir().unwrap_or_else(|| glib::user_cache_dir().join("koshi").join("mail"))
}

/// A cached thread fetched less than [`FRESH_FOR_SECS`] ago, as gzipped mbox
/// bytes, or `None` when the entry is absent or old enough that the network
/// should be asked for new replies.
pub fn load_fresh(list: &str, message_id: &str) -> Option<Vec<u8>> {
    let path = entry_path(list, message_id)?;
    let modified = fs::metadata(&path).ok()?.modified().ok()?;
    let age = SystemTime::now().duration_since(modified).ok()?;
    if age > Duration::from_secs(FRESH_FOR_SECS) {
        return None;
    }
    fs::read(&path).ok()
}

/// A cached thread of any age, as gzipped mbox bytes — the offline fallback
/// when the network fails: a stale thread beats an error page.
pub fn load_any(list: &str, message_id: &str) -> Option<Vec<u8>> {
    let path = entry_path(list, message_id)?;
    fs::read(&path).ok()
}

/// Record a freshly downloaded thread, replacing any older entry. `gz` is the
/// payload as served (still gzipped). Written via a temp file and rename so a
/// crash mid-write can't leave a truncated entry that would parse as an empty
/// thread.
pub fn store(list: &str, message_id: &str, gz: &[u8]) {
    let Some(path) = entry_path(list, message_id) else {
        return;
    };
    let write = || -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = tmp_path(&path);
        fs::write(&tmp, gz)?;
        fs::rename(&tmp, &path)
    };
    if let Err(err) = write() {
        log::warn!("could not cache thread at {}: {err}", path.display());
    }
}

/// Delete every cached thread (and any temp file a crash left behind),
/// leaving anything else in the folder alone — the user may have pointed the
/// cache at a folder that isn't exclusively ours, so only files matching our
/// own naming are touched. Emptied per-list subfolders are removed too.
pub fn clear() {
    let Ok(lists) = fs::read_dir(dir()) else {
        return;
    };
    for entry in lists.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Ok(files) = fs::read_dir(&path) {
                for file in files.flatten() {
                    remove_if_entry(&file.path());
                }
            }
            // Only succeeds once empty; a foreign file keeps the folder.
            let _ = fs::remove_dir(&path);
        } else {
            remove_if_entry(&path);
        }
    }
}

/// Total size of all cached threads in bytes, for the Preferences readout.
pub fn size_bytes() -> u64 {
    let Ok(lists) = fs::read_dir(dir()) else {
        return 0;
    };
    let mut total = 0;
    for entry in lists.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Ok(files) = fs::read_dir(&path) {
                total += files
                    .flatten()
                    .filter(|file| is_entry(&file.path()))
                    .filter_map(|file| file.metadata().ok())
                    .map(|meta| meta.len())
                    .sum::<u64>();
            }
        } else if is_entry(&path)
            && let Ok(meta) = entry.metadata()
        {
            total += meta.len();
        }
    }
    total
}

const ENTRY_SUFFIX: &str = ".mbox.gz";

/// Where the entry for `(list, message_id)` lives, or `None` for a
/// pathological Message-ID whose escaped form would overflow the common
/// 255-byte filename limit — such a thread simply goes uncached.
fn entry_path(list: &str, message_id: &str) -> Option<PathBuf> {
    let trimmed = message_id.trim().trim_matches(['<', '>']);
    let file = format!("{}{ENTRY_SUFFIX}", escape(trimmed));
    if file.len() > 255 {
        return None;
    }
    Some(dir().join(escape(list).as_str()).join(file))
}

/// Percent-encode a cache key component down to the URI-unreserved set
/// (alphanumerics and `-._~`), which is filename-safe on every filesystem —
/// Message-IDs can legally contain `/` and other separators.
fn escape(component: &str) -> glib::GString {
    glib::Uri::escape_string(component, None, false)
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

fn is_entry(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.ends_with(ENTRY_SUFFIX) || name.ends_with(concat!(".mbox.gz", ".tmp"))
        })
}

fn remove_if_entry(path: &Path) {
    if is_entry(path)
        && let Err(err) = fs::remove_file(path)
    {
        log::warn!("could not remove {}: {err}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch settings store pointing the cache at a per-test temp folder;
    /// both are removed when the guard drops. The settings path is
    /// thread-local and each #[test] runs on its own thread, so tests are
    /// isolated from each other and from the real store.
    struct ScratchCache {
        dir: PathBuf,
    }

    impl ScratchCache {
        fn new(test_name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("koshi-cache-{test_name}-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            settings::init(dir.join("settings.json"));
            settings::set_cache_dir(Some(&dir.join("mail")));
            Self { dir }
        }
    }

    impl Drop for ScratchCache {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn backdate(path: &Path, secs: u64) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn store_then_load_roundtrips() {
        let _scratch = ScratchCache::new("roundtrip");
        store("lkml", "<some-id@example.org>", b"gzipped bytes");
        // Angle brackets and whitespace never make it into the key.
        assert_eq!(
            load_fresh("lkml", " some-id@example.org ").as_deref(),
            Some(b"gzipped bytes".as_slice())
        );
    }

    #[test]
    fn missing_entry_loads_nothing() {
        let _scratch = ScratchCache::new("missing");
        assert!(load_fresh("lkml", "absent@example.org").is_none());
        assert!(load_any("lkml", "absent@example.org").is_none());
    }

    #[test]
    fn stale_entry_is_not_fresh_but_still_loads() {
        let _scratch = ScratchCache::new("stale");
        store("lkml", "old@example.org", b"stale payload");
        let path = entry_path("lkml", "old@example.org").unwrap();
        backdate(&path, FRESH_FOR_SECS + 60);
        assert!(load_fresh("lkml", "old@example.org").is_none());
        // ...but the offline fallback still serves it.
        assert_eq!(
            load_any("lkml", "old@example.org").as_deref(),
            Some(b"stale payload".as_slice())
        );
    }

    #[test]
    fn message_ids_with_separators_stay_within_one_entry() {
        let _scratch = ScratchCache::new("escape");
        // A Message-ID containing '/' must not create nested folders or
        // escape the cache dir.
        store("lkml", "a/b/../../c@example.org", b"payload");
        let path = entry_path("lkml", "a/b/../../c@example.org").unwrap();
        assert!(path.parent().unwrap().ends_with("mail/lkml"));
        assert_eq!(
            load_any("lkml", "a/b/../../c@example.org").as_deref(),
            Some(b"payload".as_slice())
        );
    }

    #[test]
    fn overlong_message_id_goes_uncached() {
        let _scratch = ScratchCache::new("overlong");
        let id = format!("{}@example.org", "x".repeat(300));
        store("lkml", &id, b"payload");
        assert!(load_any("lkml", &id).is_none());
    }

    #[test]
    fn clear_removes_entries_and_reports_empty() {
        let scratch = ScratchCache::new("clear");
        store("lkml", "one@example.org", b"first");
        store("bpf", "two@example.org", b"second");
        assert!(size_bytes() > 0);

        // A foreign file in the cache folder must survive a clear.
        let foreign = scratch.dir.join("mail").join("keep.txt");
        fs::write(&foreign, b"not ours").unwrap();

        clear();
        assert_eq!(size_bytes(), 0);
        assert!(load_any("lkml", "one@example.org").is_none());
        assert!(load_any("bpf", "two@example.org").is_none());
        assert!(foreign.exists());
        // Emptied per-list folders are gone; the cache dir itself remains.
        assert!(!scratch.dir.join("mail").join("lkml").exists());
    }
}
