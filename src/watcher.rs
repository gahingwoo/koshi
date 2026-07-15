//! The background watcher behind thread subscriptions: on a timer it refetches
//! every subscribed thread from lore and raises a desktop notification for each
//! message it has not seen before — titled with the sender's name and bodied
//! with the subject, the way a mail client announces new mail.
//!
//! It lives entirely in-process: notifications arrive only while Koshi is open,
//! which is the whole promise of "poll every few minutes." The poll interval is
//! a user preference ([`crate::settings::poll_interval_minutes`]); each cycle
//! re-reads it, so changing it in Preferences takes effect on the next tick.

use std::collections::{HashMap, HashSet};

use adw::prelude::*;
use gtk::{gio, glib};

use crate::lore;
use crate::settings;
use crate::subscriptions::{self, Subscription};
use crate::thread_page::thread_message_digests;

/// Begin watching subscribed threads. Call once at startup: it polls once right
/// away — so replies that landed while Koshi was closed surface on launch
/// instead of only after the first interval elapses — then reschedules itself
/// for the life of the application.
pub fn start(app: &adw::Application) {
    poll_all(app);
    schedule_next_poll(app);
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
        let Ok(mbox) =
            lore::fetch_thread_mbox(&subscription.list, &subscription.message_id, &cancellable)
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

/// Arm a one-shot timer for the next poll, re-reading the configured interval
/// each time so a change in Preferences is honoured from the following cycle.
/// The poll runs, then re-arms — a fixed recurring source would instead pin the
/// cadence to whatever the interval was when the watcher started.
fn schedule_next_poll(app: &adw::Application) {
    let seconds = settings::poll_interval_minutes().saturating_mul(60);
    glib::timeout_add_seconds_local_once(
        seconds,
        glib::clone!(
            #[weak]
            app,
            move || {
                poll_all(&app);
                schedule_next_poll(&app);
            }
        ),
    );
}

/// Kick off a concurrent fetch of every current subscription. Each subscription
/// is handled by its own local future so one slow or failing thread never holds
/// up the rest.
fn poll_all(app: &adw::Application) {
    for subscription in subscriptions::all() {
        glib::spawn_future_local(glib::clone!(
            #[weak]
            app,
            async move {
                poll_one(&app, subscription).await;
            }
        ));
    }
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
/// next cycle.
async fn poll_one(app: &adw::Application, subscription: Subscription) {
    let cancellable = gio::Cancellable::new();
    let mbox =
        match lore::fetch_thread_mbox(&subscription.list, &subscription.message_id, &cancellable)
            .await
        {
            Ok(mbox) => mbox,
            Err(err) => {
                eprintln!(
                    "koshi: could not poll subscription {}: {err}",
                    subscription.message_id
                );
                return;
            }
        };

    let digests = thread_message_digests(&mbox);
    if digests.is_empty() {
        return;
    }

    // The subscription may have been toggled off while the fetch was in flight;
    // if so, drop this result rather than notify or persist against it.
    if !subscriptions::is_subscribed(&subscription.message_id) {
        return;
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
        eprintln!("koshi: no session bus; cannot notify about \"{subject}\"");
        return;
    };
    // org.freedesktop.Notifications.Notify — signature `susssasa{sv}i`.
    let params = (
        "Koshi",                                 // app_name
        0u32,                                    // replaces_id: never coalesce
        "mail-unread",                           // app_icon: themed, always resolves
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
                eprintln!("koshi: notification failed: {err}");
            }
        },
    );
}
