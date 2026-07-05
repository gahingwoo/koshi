use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::favorites::{self, FavoriteInbox};
use crate::list_page::build_list_page;
use crate::lore::{self, Inbox};
use crate::remote_page::RemoteContent;
use crate::thread_list_page::build_thread_list_page;

pub const INBOX_LIST_TITLE: &str = "Public Inboxes";

pub fn build_inbox_page(nav: &adw::NavigationView) -> adw::NavigationPage {
    let remote = RemoteContent::new();
    let page = build_list_page(
        INBOX_LIST_TITLE,
        "Open a public inbox",
        "Every list mirrored on lore.kernel.org. Pick one to open it in this tab.",
        &[],
        remote.widget(),
    );
    load(remote, nav.clone());
    page
}

fn load(remote: RemoteContent, nav: adw::NavigationView) {
    remote.show_loading();
    let cancellable = remote.cancellable();
    glib::spawn_future_local(async move {
        match lore::fetch_inboxes(&cancellable).await {
            Ok(inboxes) => remote.show_content(&build_content(&nav, inboxes)),
            Err(error) if error.is_cancelled() => {}
            Err(error) => {
                let weak = remote.downgrade();
                let nav = nav.downgrade();
                remote.show_error(&error, move || {
                    if let (Some(remote), Some(nav)) = (weak.upgrade(), nav.upgrade()) {
                        load(remote, nav);
                    }
                });
            }
        }
    });
}

/// The page content: an optional Favorites section stacked above the full
/// inbox list. Rebuilt wholesale whenever a star is toggled, so the section
/// and every row's star stay in sync without any cross-widget bookkeeping.
fn build_content(nav: &adw::NavigationView, inboxes: Vec<Inbox>) -> gtk::Box {
    let container = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    rebuild(&container, nav, &Rc::new(inboxes));
    container
}

fn rebuild(container: &gtk::Box, nav: &adw::NavigationView, inboxes: &Rc<Vec<Inbox>>) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let favorites = favorites::all_inboxes();
    if !favorites.is_empty() {
        container.append(&build_section_heading("Favorites"));
        container.append(&build_favorites_list(container, nav, inboxes, &favorites));
        container.append(&build_section_heading("All Inboxes"));
    }
    container.append(&build_inbox_list(container, nav, inboxes));
}

fn build_section_heading(label: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(label)
        .halign(gtk::Align::Start)
        .css_classes(["heading"])
        .build()
}

fn build_favorites_list(
    container: &gtk::Box,
    nav: &adw::NavigationView,
    inboxes: &Rc<Vec<Inbox>>,
    favorites: &[FavoriteInbox],
) -> gtk::ListBox {
    let list = new_boxed_list();
    for fav in favorites {
        let row = build_row(&fav.slug, &fav.description);
        row.add_suffix(&build_star_button(fav.clone(), container, nav, inboxes));
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        list.append(&row);
    }

    let favorites = favorites.to_vec();
    list.connect_row_activated(glib::clone!(
        #[weak]
        nav,
        move |_, row| {
            let fav = &favorites[row.index() as usize];
            nav.push(&build_thread_list_page(&nav, &fav.slug, &fav.description));
        }
    ));

    list
}

fn build_inbox_list(
    container: &gtk::Box,
    nav: &adw::NavigationView,
    inboxes: &Rc<Vec<Inbox>>,
) -> gtk::ListBox {
    let list = new_boxed_list();
    for inbox in inboxes.iter() {
        let fav = FavoriteInbox {
            slug: inbox.slug.clone(),
            description: inbox.description.clone(),
        };
        let row = build_row(&inbox.slug, &inbox.description);
        row.add_suffix(&build_star_button(fav, container, nav, inboxes));
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        list.append(&row);
    }

    list.connect_row_activated(glib::clone!(
        #[weak]
        nav,
        #[strong(rename_to = inboxes)]
        Rc::clone(inboxes),
        move |_, row| {
            let inbox = &inboxes[row.index() as usize];
            nav.push(&build_thread_list_page(&nav, &inbox.slug, &inbox.description));
        }
    ));

    list
}

fn new_boxed_list() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        // The list sits inside a vexpanding stack; without this it stretches
        // and the boxed-list shadow outlines the empty space below the rows.
        .valign(gtk::Align::Start)
        .css_classes(["boxed-list"])
        .build()
}

fn build_row(slug: &str, description: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(slug))
        .subtitle(glib::markup_escape_text(description))
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("mail-unread-symbolic"));
    row
}

/// A star toggle flipping the inbox's favorite state. Toggling rebuilds the
/// whole page content — from an idle, since the rebuild destroys the very
/// button whose signal handler requested it.
fn build_star_button(
    fav: FavoriteInbox,
    container: &gtk::Box,
    nav: &adw::NavigationView,
    inboxes: &Rc<Vec<Inbox>>,
) -> gtk::ToggleButton {
    let starred = favorites::is_favorite_inbox(&fav.slug);
    let button = gtk::ToggleButton::builder()
        .active(starred)
        .icon_name(if starred {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        })
        .tooltip_text(if starred {
            "Remove from Favorites"
        } else {
            "Add to Favorites"
        })
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();

    button.connect_toggled(glib::clone!(
        #[weak]
        container,
        #[weak]
        nav,
        #[strong(rename_to = inboxes)]
        Rc::clone(inboxes),
        move |button| {
            // Drive the store from the button's own state rather than blindly
            // flipping it: another tab's inbox page may have changed the
            // store since this one was built, and a blind flip would then do
            // the opposite of what the click asked for.
            if favorites::is_favorite_inbox(&fav.slug) != button.is_active() {
                favorites::toggle_inbox(fav.clone());
            }
            glib::idle_add_local_once(glib::clone!(
                #[weak]
                container,
                #[weak]
                nav,
                #[strong]
                inboxes,
                move || rebuild(&container, &nav, &inboxes)
            ));
        }
    ));

    button
}
