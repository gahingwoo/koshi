use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::favorites::{self, FavoriteInbox};
use crate::list_page::build_list_page;
use crate::lore::{self, Inbox};
use crate::remote_page::RemoteContent;
use crate::thread_list_page::build_thread_list_page;

pub const INBOX_LIST_TITLE: &str = "Public Inboxes";

/// The main list's star buttons by slug, for updating a star in place when
/// its inbox is unfavorited from the favorites section. Weak refs: the
/// favorites section's handlers hold this map, and a strong ref here would
/// cycle the buttons alive past the page's destruction.
type StarButtons = Rc<HashMap<String, glib::WeakRef<gtk::Button>>>;

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

/// The page content: a favorites section stacked above the full inbox list.
/// The full list is built exactly once — regenerating its hundreds of rows
/// on every star click stalls noticeably — so a toggle only updates star
/// icons in place and regenerates the small favorites section.
fn build_content(nav: &adw::NavigationView, inboxes: Vec<Inbox>) -> gtk::Box {
    let container = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    container.append(&section);

    let list = new_boxed_list();
    let mut stars = HashMap::new();
    for inbox in &inboxes {
        let star = new_star_button(favorites::is_favorite_inbox(&inbox.slug));
        stars.insert(inbox.slug.clone(), star.downgrade());

        let row = build_row(&inbox.slug, &inbox.description);
        row.add_suffix(&star);
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        list.append(&row);
    }
    let stars: StarButtons = Rc::new(stars);

    for inbox in &inboxes {
        let fav = FavoriteInbox {
            slug: inbox.slug.clone(),
            description: inbox.description.clone(),
        };
        let Some(star) = stars.get(&inbox.slug).and_then(|weak| weak.upgrade()) else {
            continue;
        };
        star.connect_clicked(glib::clone!(
            #[weak]
            section,
            #[weak]
            nav,
            #[strong]
            stars,
            move |star| {
                let starred = favorites::toggle_inbox(fav.clone());
                apply_star_state(star, starred);
                refresh_favorites(&section, &nav, &stars);
            }
        ));
    }
    container.append(&list);

    let inboxes = Rc::new(inboxes);
    list.connect_row_activated(glib::clone!(
        #[weak]
        nav,
        move |_, row| {
            let inbox = &inboxes[row.index() as usize];
            nav.push(&build_thread_list_page(&nav, &inbox.slug, &inbox.description));
        }
    ));

    refresh_favorites(&section, nav, &stars);
    container
}

/// Regenerate the favorites section (its heading, list and the trailing
/// "All Inboxes" heading) from the store; empty the section when nothing is
/// starred.
fn refresh_favorites(section: &gtk::Box, nav: &adw::NavigationView, stars: &StarButtons) {
    while let Some(child) = section.first_child() {
        section.remove(&child);
    }

    let favorites = favorites::all_inboxes();
    if favorites.is_empty() {
        return;
    }

    section.append(&build_section_heading("Favorites"));

    let list = new_boxed_list();
    for fav in &favorites {
        let star = new_star_button(true);
        star.connect_clicked(glib::clone!(
            #[weak]
            section,
            #[weak]
            nav,
            #[strong]
            stars,
            #[strong(rename_to = fav)]
            fav.clone(),
            move |_| {
                favorites::toggle_inbox(fav.clone());
                if let Some(main) = stars.get(&fav.slug).and_then(|weak| weak.upgrade()) {
                    apply_star_state(&main, false);
                }
                // Refresh from an idle: it destroys the very button whose
                // handler requested it.
                glib::idle_add_local_once(glib::clone!(
                    #[weak]
                    section,
                    #[weak]
                    nav,
                    #[strong]
                    stars,
                    move || refresh_favorites(&section, &nav, &stars)
                ));
            }
        ));

        let row = build_row(&fav.slug, &fav.description);
        row.add_suffix(&star);
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        list.append(&row);
    }

    list.connect_row_activated(glib::clone!(
        #[weak]
        nav,
        move |_, row| {
            let fav = &favorites[row.index() as usize];
            nav.push(&build_thread_list_page(&nav, &fav.slug, &fav.description));
        }
    ));
    section.append(&list);

    section.append(&build_section_heading("All Inboxes"));
}

fn build_section_heading(label: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(label)
        .halign(gtk::Align::Start)
        .css_classes(["heading"])
        .build()
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

/// A plain button rather than a ToggleButton: the starred/unstarred state
/// already shows through the icon, and a checked ToggleButton would keep a
/// pressed background.
fn new_star_button(starred: bool) -> gtk::Button {
    let button = gtk::Button::builder()
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    apply_star_state(&button, starred);
    button
}

fn apply_star_state(button: &gtk::Button, starred: bool) {
    button.set_icon_name(if starred {
        "starred-symbolic"
    } else {
        "non-starred-symbolic"
    });
    button.set_tooltip_text(Some(if starred {
        "Remove from Favorites"
    } else {
        "Add to Favorites"
    }));
}
