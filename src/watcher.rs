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

use std::cell::RefCell;
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
async fn poll_one(
    app: &adw::Application,
    subscription: Subscription,
) -> Result<(), lore::Error> {
    let cancellable = gio::Cancellable::new();
    // Live, never cached: a poll exists to see messages the cache can't have
    // yet. As a side effect each poll refreshes the cache entry, so a
    // subscribed thread reopens with its newest replies already on disk.
    let mbox = lore::fetch_thread_mbox_live(
        &subscription.list,
        &subscription.message_id,
        &cancellable,
    )
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
            notify_new_message(app, &digest.author, &digest.subject);
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
/// Sent straight to `org.freedesktop.Notifications` rather than through
/// `GApplication::send_notification`. Under GNOME the latter routes via
/// `org.gtk.Notifications`, which only displays a notification for an app GNOME
/// Shell has indexed from an *installed* `.desktop` file — so an uninstalled
/// build (`cargo run`), or one whose desktop file was added to an already
/// running session, has every notification silently dropped with an `InvalidApp`
/// error. The freedesktop service imposes no such requirement, so the banner
/// shows however Koshi was launched.
fn notify_new_message(app: &adw::Application, author: &str, subject: &str) {
    let Some(connection) = app.dbus_connection() else {
        log::warn!("no session bus; cannot notify about \"{subject}\"");
        return;
    };
    // org.freedesktop.Notifications.Notify — signature `susssasa{sv}i`.
    let icon = notification_icon();
    let params = (
        "Koshi",                                 // app_name
        0u32,                                    // replaces_id: never coalesce
        icon.as_str(),                           // app_icon: Koshi's symbolic icon
        author,                                  // summary (notification title)
        subject,                                 // body
        &[] as &[&str],                          // actions: none
        HashMap::<String, glib::Variant>::new(), // hints: none
        -1i32,                                   // expire_timeout: server default
    )
        .to_variant();
    connection.call(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "Notify",
        Some(&params),
        Some(glib::VariantTy::new("(u)").expect("valid reply signature")),
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

/// The bundled symbolic app icon, as a gresource path.
const ICON_RESOURCE: &str =
    "/moe/nikableh/Koshi/icons/symbolic/apps/moe.nikableh.Koshi-symbolic.svg";

thread_local! {
    /// Filesystem path to Koshi's notification icon, or `None` before the first
    /// notification materialises it. See [`notification_icon`].
    static NOTIFICATION_ICON: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The `app_icon` to hand `org.freedesktop.Notifications`: an absolute path to
/// Koshi's symbolic icon, written out from the bundled gresource on first use.
///
/// The notification daemon is a separate process and can't read Koshi's
/// in-process resources, so the icon has to exist as a real file; extracting it
/// works whether or not Koshi is installed with its icon in a system theme. The
/// `-symbolic.svg` filename is preserved so the shell recolours it for the
/// current theme (light icon on a dark banner). Extraction is done once per run
/// and cached; if it fails, the themed `mail-unread` stands in.
fn notification_icon() -> String {
    NOTIFICATION_ICON.with_borrow_mut(|cached| {
        if let Some(path) = cached {
            return path.clone();
        }
        let path = extract_notification_icon().unwrap_or_else(|| "mail-unread".to_string());
        *cached = Some(path.clone());
        path
    })
}

/// Write the bundled icon to `$XDG_CACHE_HOME/koshi/` and return its path, or
/// `None` if the resource is missing or the file can't be written.
fn extract_notification_icon() -> Option<String> {
    let bytes = gio::resources_lookup_data(ICON_RESOURCE, gio::ResourceLookupFlags::NONE).ok()?;
    let dir = glib::user_cache_dir().join("koshi");
    std::fs::create_dir_all(&dir).ok()?;
    // Keep the -symbolic.svg name so the shell recolours it.
    let path = dir.join("moe.nikableh.Koshi-symbolic.svg");
    std::fs::write(&path, bytes.as_ref()).ok()?;
    path.into_os_string().into_string().ok()
}
