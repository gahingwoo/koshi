//! The background watcher behind thread subscriptions: on a timer it refetches
//! every subscribed thread from lore and raises a desktop notification for each
//! message it has not seen before — titled with the sender's name and bodied
//! with the subject, the way a mail client announces new mail.
//!
//! It lives entirely in-process: notifications arrive only while Koshi is open,
//! which is the whole promise of "poll every few minutes." The poll interval is
//! a user preference ([`crate::settings::poll_interval_minutes`]); each cycle
//! re-reads it, so changing it in Preferences takes effect on the next tick.
//!
//! Polling is deliberately gentle: one fetch at a time with a pause between
//! them, and the first cycle waits out the launch page load. lore.kernel.org
//! rate-limits bursts of requests with 503 Service Unavailable — and once the
//! limiter trips, it answers *everything* with 503, including whatever page
//! the user is trying to read.

use std::collections::{HashMap, HashSet};

use adw::prelude::*;
use gtk::{gio, glib};

use crate::lore;
use crate::settings;
use crate::subscriptions::{self, Subscription};
use crate::thread_page::thread_message_digests;

/// How long the first poll waits after startup. Long enough for the launch
/// page's own fetch to finish first — polling the moment the app opens is what
/// used to trip lore's rate limiter and turn the landing page into a 503 —
/// while still surfacing replies that landed while Koshi was closed "on
/// launch" rather than a full interval later.
const STARTUP_POLL_DELAY_SECONDS: u32 = 10;

/// Pause between consecutive subscription fetches within one cycle, so a long
/// subscription list reads as a trickle rather than a burst to lore's rate
/// limiter.
const POLL_SPACING_SECONDS: u32 = 2;

/// Begin watching subscribed threads. Call once at startup: it polls shortly
/// after launch — so replies that landed while Koshi was closed surface right
/// away instead of only after the first interval elapses — then keeps polling
/// for the life of the application.
///
/// Each cycle runs to completion before the next interval starts counting, so
/// cycles never overlap: a slow cycle (many subscriptions, spaced fetches)
/// under a short interval just stretches the cadence instead of piling
/// concurrent cycles onto lore. The interval is re-read every cycle, so a
/// change in Preferences is honoured from the following one.
pub fn start(app: &adw::Application) {
    let weak = app.downgrade();
    glib::spawn_future_local(async move {
        glib::timeout_future_seconds(STARTUP_POLL_DELAY_SECONDS).await;
        loop {
            {
                // Upgrade per cycle, not for the loop's lifetime: a strong
                // reference held across the interval sleep would keep the
                // application alive after its last window closes.
                let Some(app) = weak.upgrade() else { return };
                poll_all(&app).await;
            }
            let seconds = settings::poll_interval_minutes().saturating_mul(60);
            glib::timeout_future_seconds(seconds).await;
        }
    });
}

/// Seed a just-created subscription's baseline right away, rather than leaving
/// it for the next scheduled poll up to a whole interval later. It fetches the
/// thread once and records everything in it as already seen — silently, since
/// this is the pre-existing backlog, not new mail — so a reply that lands
/// moments after you subscribe is still caught by the following poll instead of
/// being folded into a late baseline and missed.
///
/// Call it from every place a subscription is added; it reuses the watcher's
/// own digest logic, so the seeded IDs can never drift from the format later
/// polls diff against. A failed fetch leaves the baseline empty, which just
/// hands the seeding back to the next scheduled poll — the same behaviour as
/// before, so a subscribe is never worse off for this shortcut.
pub fn seed_new_subscription(subscription: Subscription) {
    glib::spawn_future_local(async move {
        let cancellable = gio::Cancellable::new();
        // Live, never cached: the baseline must reflect the thread as it is
        // right now, and each fetch also refreshes the cache entry.
        let Ok(mbox) = lore::fetch_thread_mbox_live(
            &subscription.list,
            &subscription.message_id,
            &cancellable,
        )
        .await
        else {
            return;
        };
        let seen: Vec<String> = thread_message_digests(&mbox)
            .into_iter()
            .map(|digest| digest.message_id)
            .collect();
        // An empty result (unfetchable or unparsable) is left for the next poll
        // to seed.
        if seen.is_empty() {
            return;
        }
        // Defer to a baseline that already exists: a scheduled poll may have
        // fired and seeded (via its own union-extending set_seen) while this
        // fetch was in flight, and its view could be newer than ours — a blind
        // overwrite here could drop a message it saw and we didn't, resurfacing
        // it as a spurious notification. A non-empty seen also means the store
        // still holds this subscription, so this doubles as the unsubscribe
        // check. Only seed when the baseline is genuinely still empty.
        let unseeded = subscriptions::all()
            .iter()
            .any(|sub| sub.message_id == subscription.message_id && sub.seen.is_empty());
        if unseeded {
            subscriptions::set_seen(&subscription.message_id, seen);
        }
    });
}

/// Poll every current subscription, one at a time with a courtesy pause
/// between fetches. Sequential on purpose: the original shape — one concurrent
/// future per subscription — hit lore with a burst of simultaneous `t.mbox.gz`
/// requests, and its rate limiter answered every one of them (plus the page
/// the user was opening) with 503 Service Unavailable.
///
/// If lore reports rate limiting anyway, the rest of the cycle is abandoned:
/// pressing on would only prolong the penalty, and the untouched subscriptions
/// simply retry next cycle.
async fn poll_all(app: &adw::Application) {
    for (index, subscription) in subscriptions::all().into_iter().enumerate() {
        if index > 0 {
            glib::timeout_future_seconds(POLL_SPACING_SECONDS).await;
        }
        let message_id = subscription.message_id.clone();
        if let Err(err) = poll_one(app, subscription).await {
            log::warn!("could not poll subscription {message_id}: {err}");
            if rate_limited(&err) {
                log::warn!(
                    "lore.kernel.org is rate limiting; \
                     leaving the remaining subscriptions for the next cycle"
                );
                return;
            }
        }
    }
}

/// Whether an error is lore's rate limiter talking — the signal to back off
/// for the rest of the cycle rather than keep asking. lore answers overload
/// with 503 Service Unavailable.
fn rate_limited(err: &lore::Error) -> bool {
    matches!(err, lore::Error::Status(soup::Status::ServiceUnavailable))
}

/// Refetch one thread and reconcile it with the subscription's seen set:
///
/// - A fresh subscription (empty seen) is *seeded* — every current message is
///   recorded as seen and nothing is notified, so subscribing never replays the
///   existing backlog.
/// - Otherwise every message whose Message-ID is not yet in seen is a new
///   arrival: it raises a notification and joins the seen set.
///
/// The seen set only grows, so a transiently short fetch can't resurrect old
/// messages as "new". A failed fetch leaves the subscription untouched to retry
/// next cycle; the error is surfaced so [`poll_all`] can spot rate limiting.
async fn poll_one(app: &adw::Application, subscription: Subscription) -> Result<(), lore::Error> {
    let cancellable = gio::Cancellable::new();
    // Live, never cached: a poll exists to see messages the cache can't have
    // yet. As a side effect each poll refreshes the cache entry, so a
    // subscribed thread reopens with its newest replies already on disk.
    let mbox =
        lore::fetch_thread_mbox_live(&subscription.list, &subscription.message_id, &cancellable)
            .await?;

    let digests = thread_message_digests(&mbox);
    if digests.is_empty() {
        return Ok(());
    }

    // The subscription may have been toggled off while the fetch was in flight;
    // if so, drop this result rather than notify or persist against it.
    if !subscriptions::is_subscribed(&subscription.message_id) {
        return Ok(());
    }

    let seen: HashSet<&str> = subscription.seen.iter().map(String::as_str).collect();
    let seeding = seen.is_empty();
    if !seeding {
        for digest in digests
            .iter()
            .filter(|d| !seen.contains(d.message_id.as_str()))
        {
            notify_new_message(app, &digest.message_id, &digest.author, &digest.subject);
        }
    }

    // Record the full current set (existing seen ∪ everything just fetched), so
    // the next poll's diff starts from here.
    let mut updated: Vec<String> = subscription.seen;
    let known: HashSet<&str> = updated.iter().map(String::as_str).collect::<HashSet<_>>();
    let fresh: Vec<String> = digests
        .iter()
        .filter(|d| !known.contains(d.message_id.as_str()))
        .map(|d| d.message_id.clone())
        .collect();
    updated.extend(fresh);
    subscriptions::set_seen(&subscription.message_id, updated);
    Ok(())
}

/// Raise a desktop notification for one new message: the sender's name as the
/// title, the subject as the body — mirroring how a desktop mail client
/// announces an arrival.
///
/// Sent through the XDG desktop portal (`org.freedesktop.portal.Notification`)
/// rather than the bare `org.freedesktop.Notifications` service or GTK's own
/// `GApplication::send_notification`. The portal is the sandbox-friendly route:
/// it needs no D-Bus hole poked in the Flatpak manifest (the desktop portal is
/// always reachable), attributes the banner to Koshi's app id on its own, and
/// works identically installed or run from `cargo`. It replaces the earlier
/// direct call, which required a `--talk-name` grant and a hand-extracted icon
/// file for the notification daemon to read.
///
/// `id` is the message's own Message-Id: the portal replaces a notification
/// whose id it has already seen, so reusing it dedupes a message that somehow
/// surfaces twice without coalescing distinct arrivals.
fn notify_new_message(app: &adw::Application, id: &str, author: &str, subject: &str) {
    let Some(connection) = app.dbus_connection() else {
        log::warn!("no session bus; cannot notify about \"{subject}\"");
        return;
    };
    let params = add_notification_params(id, author, subject);
    connection.call(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Notification",
        "AddNotification",
        Some(&params),
        None,
        gio::DBusCallFlags::NONE,
        -1,
        gio::Cancellable::NONE,
        |result| {
            if let Err(err) = result {
                log::warn!("notification failed: {err}");
            }
        },
    );
}

/// The `(sa{sv})` argument tuple for
/// `org.freedesktop.portal.Notification.AddNotification`: the notification id
/// and its property dictionary (title, body, icon).
fn add_notification_params(id: &str, author: &str, subject: &str) -> glib::Variant {
    let mut notification: HashMap<&str, glib::Variant> = HashMap::new();
    notification.insert("title", author.to_variant());
    notification.insert("body", subject.to_variant());
    // Serialized GIcon (sv): a themed icon, tried in order. The app id resolves
    // to Koshi's installed icon; `mail-unread` is the themed fallback for an
    // uninstalled build whose icon isn't in the theme yet.
    notification.insert(
        "icon",
        ("themed", ["moe.nikableh.Koshi", "mail-unread"].to_variant()).to_variant(),
    );
    (id, notification).to_variant()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The params must serialise to exactly what `AddNotification` expects —
    /// `(sa{sv})` — including the icon nested as a boxed `(sv)`. A mismatch here
    /// is only caught at the D-Bus call otherwise, never at compile time.
    #[test]
    fn add_notification_params_has_portal_signature() {
        let params = add_notification_params("<id@lore>", "Linus", "Re: patch");
        assert_eq!(params.type_().as_str(), "(sa{sv})");

        // Each a{sv} entry is (key: s, value: v); unbox the value and confirm
        // the icon is a serialised GIcon tuple `(sv)`.
        let dict = params.child_value(1);
        let icon = dict
            .iter()
            .find(|entry| entry.child_value(0).str() == Some("icon"))
            .and_then(|entry| entry.child_value(1).as_variant())
            .expect("icon key present");
        assert_eq!(icon.type_().as_str(), "(sv)");
    }
}
