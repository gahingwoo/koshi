use adw::prelude::*;
use gtk::glib;

use crate::favorites::{self, Favorite, FavoriteInbox};
use crate::list_page::build_list_page;
use crate::subscriptions::{self, Subscription};
use crate::thread_list_page::build_thread_list_page;
use crate::thread_page::{apply_bell_state, build_thread_page, new_bell_button};

pub const FAVORITES_TITLE: &str = "Favorites";

/// Widget name marking the favorites page, so it can be recognized without
/// comparing titles (a mail subject could legitimately equal the title) and
/// without navigation tags (which must be unique, but the page can appear
/// twice in one stack via favorites → mail → favorites).
pub const FAVORITES_PAGE_NAME: &str = "koshi-favorites-page";

pub fn build_favorites_page(nav: &adw::NavigationView) -> adw::NavigationPage {
    let inboxes = favorites::all_inboxes();
    let mails = favorites::all();

    // HIG placeholder-page pattern: an empty view gets a symbolic
    // AdwStatusPage instead of an empty list.
    let page = if inboxes.is_empty() && mails.is_empty() {
        let status = adw::StatusPage::builder()
            .icon_name("non-starred-symbolic")
            .title("No Favorites")
            .description("Star a message or an inbox to add it here")
            .build();
        adw::NavigationPage::new(&status, FAVORITES_TITLE)
    } else {
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        if !inboxes.is_empty() {
            content.append(&build_section_heading("Inboxes"));
            content.append(&build_inbox_list(nav, inboxes));
        }
        if !mails.is_empty() {
            content.append(&build_section_heading("Messages"));
            content.append(&build_mail_list(nav, mails));
        }

        build_list_page(
            FAVORITES_TITLE,
            FAVORITES_TITLE,
            "Starred inboxes and messages",
            &[],
            &content,
        )
    };
    page.set_widget_name(FAVORITES_PAGE_NAME);
    page
}

fn build_section_heading(label: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(label)
        .halign(gtk::Align::Start)
        .css_classes(["heading"])
        .build()
}

fn build_inbox_list(nav: &adw::NavigationView, inboxes: Vec<FavoriteInbox>) -> gtk::ListBox {
    let list = new_boxed_list();
    for fav in &inboxes {
        list.append(&build_inbox_row(fav));
    }
    list.connect_row_activated(glib::clone!(
        #[weak]
        nav,
        move |_, row| {
            let fav = &inboxes[row.index() as usize];
            nav.push(&build_thread_list_page(&nav, &fav.slug, &fav.description));
        }
    ));
    list
}

fn build_mail_list(nav: &adw::NavigationView, mails: Vec<Favorite>) -> gtk::ListBox {
    let list = new_boxed_list();
    for fav in &mails {
        list.append(&build_favorite_row(fav));
    }
    list.connect_row_activated(glib::clone!(
        #[weak]
        nav,
        move |_, row| {
            let fav = &mails[row.index() as usize];
            nav.push(&build_thread_page(&nav, &fav.list, &fav.message_id));
        }
    ));
    list
}

fn new_boxed_list() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build()
}

fn build_inbox_row(fav: &FavoriteInbox) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&fav.slug))
        .subtitle(glib::markup_escape_text(&fav.description))
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("mail-unread-symbolic"));
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    row
}

fn build_favorite_row(fav: &Favorite) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(format!(
            "<tt>{}</tt>",
            glib::markup_escape_text(&fav.subject)
        ))
        .title_lines(1)
        .tooltip_text(&fav.subject)
        .activatable(true)
        .build();
    row.add_suffix(
        &gtk::Label::builder()
            .label(&fav.date)
            .css_classes(["numeric", "caption", "dim-label"])
            .build(),
    );
    // A bell to the right of the date subscribes this thread to new-mail
    // notifications, toggling filled/outline like the header bell in the
    // thread view.
    row.add_suffix(&build_subscribe_button(fav));
    row
}

/// A flat bell button toggling this mail's subscription state in the store,
/// swapping its own icon between outline and filled on each click.
fn build_subscribe_button(fav: &Favorite) -> gtk::Button {
    let subscription = Subscription {
        message_id: fav.message_id.clone(),
        subject: fav.subject.clone(),
        date: fav.date.clone(),
        list: fav.list.clone(),
        // Starts empty; seed_new_subscription fills the baseline the moment it
        // is added (a scheduled poll re-seeds if that fetch fails).
        seen: Vec::new(),
    };
    let button = new_bell_button(subscriptions::is_subscribed(&fav.message_id));
    button.connect_clicked(glib::clone!(
        #[strong]
        subscription,
        move |button| {
            let subscribed = subscriptions::toggle(subscription.clone());
            // On a fresh subscribe, seed the baseline now so an imminent reply
            // isn't absorbed silently by the first scheduled poll.
            if subscribed {
                crate::watcher::seed_new_subscription(subscription.clone());
            }
            apply_bell_state(button, subscribed);
        }
    ));
    button
}
