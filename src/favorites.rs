use std::cell::RefCell;

/// A starred mail, keyed by its Message-ID. In-memory only for now.
#[derive(Clone)]
pub struct Favorite {
    pub message_id: String,
    pub subject: String,
    pub date: String,
}

thread_local! {
    static FAVORITES: RefCell<Vec<Favorite>> = const { RefCell::new(Vec::new()) };
}

pub fn is_favorite(message_id: &str) -> bool {
    FAVORITES.with_borrow(|favs| favs.iter().any(|fav| fav.message_id == message_id))
}

/// Flip the favorite state of `fav` and return the new state
/// (true = now favorited).
pub fn toggle(fav: Favorite) -> bool {
    FAVORITES.with_borrow_mut(|favs| {
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
    })
}

pub fn all() -> Vec<Favorite> {
    FAVORITES.with_borrow(|favs| favs.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fav(id: &str) -> Favorite {
        Favorite {
            message_id: id.to_string(),
            subject: format!("subject for {id}"),
            date: "Thu, 3 Jul 2026 12:00:00 +0000".to_string(),
        }
    }

    // The store is thread-local and each #[test] runs on its own thread, so
    // these tests each see a fresh, isolated store.
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
