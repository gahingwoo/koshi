use adw::prelude::*;
use gtk::glib;

use crate::subscriptions::{self, Subscription};
use crate::thread_page::{apply_bell_state, build_thread_page, new_bell_button};
use crate::{list_page::build_list_page, watcher};

pub const SUBSCRIPTIONS_TITLE: &str = "Subscriptions";

/// Widget name marking the subscriptions page, so it can be recognized without
/// comparing titles (a mail subject could legitimately equal the title) and
/// without navigation tags (which must be unique, but the page can appear twice
/// in one stack via subscriptions → mail → subscriptions). Mirrors the
/// favorites page.
pub const SUBSCRIPTIONS_PAGE_NAME: &str = "koshi-subscriptions-page";

/// The page listing every thread the user is subscribed to — the threads the
/// background watcher polls and raises notifications for. Reached from the bell
/// button in the header, mirroring the Favorites page.
pub fn build_subscriptions_page(nav: &adw::NavigationView) -> adw::NavigationPage {
    let subs = subscriptions::all();

    // HIG placeholder-page pattern: an empty view gets a symbolic
    // AdwStatusPage instead of an empty list.
    let page = if subs.is_empty() {
        let status = adw::StatusPage::builder()
            .icon_name("bell-outline-symbolic")
            .title("No Subscriptions")
            .description("Subscribe to a thread to be notified of new replies")
            .build();
        adw::NavigationPage::new(&status, SUBSCRIPTIONS_TITLE)
    } else {
        let list = build_subscription_list(nav, subs);
        build_list_page(
            SUBSCRIPTIONS_TITLE,
            SUBSCRIPTIONS_TITLE,
            "Threads you're notified about new replies on",
            &[],
            &list,
        )
    };
    page.set_widget_name(SUBSCRIPTIONS_PAGE_NAME);
    page
}

fn build_subscription_list(nav: &adw::NavigationView, subs: Vec<Subscription>) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    for sub in &subs {
        list.append(&build_subscription_row(nav, &list, sub));
    }
    list
}

/// One subscribed thread: its subject and date, opening the thread when
/// activated, with a filled bell to unsubscribe. Unsubscribing drops the row on
/// the spot, so the page always reflects the store.
fn build_subscription_row(
    nav: &adw::NavigationView,
    list: &gtk::ListBox,
    sub: &Subscription,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(format!(
            "<tt>{}</tt>",
            glib::markup_escape_text(&sub.subject)
        ))
        .title_lines(1)
        .tooltip_text(&sub.subject)
        .activatable(true)
        .build();
    row.add_suffix(
        &gtk::Label::builder()
            .label(&sub.date)
            .css_classes(["numeric", "caption", "dim-label"])
            .build(),
    );

    // A filled bell (every row here is subscribed); clicking it unsubscribes and
    // removes the row. Re-subscribing from this page is possible if the click
    // races the store, so keep the icon honest either way.
    let bell = new_bell_button(true);
    bell.connect_clicked(glib::clone!(
        #[weak]
        row,
        #[weak]
        list,
        #[strong]
        sub,
        move |bell| {
            let subscribed = subscriptions::toggle(sub.clone());
            if subscribed {
                watcher::seed_new_subscription(sub.clone());
                apply_bell_state(bell, true);
            } else {
                list.remove(&row);
            }
        }
    ));
    row.add_suffix(&bell);

    // Activating the row (a click anywhere but the bell) opens the thread.
    let list_slug = sub.list.clone();
    let message_id = sub.message_id.clone();
    row.connect_activated(glib::clone!(
        #[weak]
        nav,
        move |_| {
            nav.push(&build_thread_page(&nav, &list_slug, &message_id));
        }
    ));
    row
}
