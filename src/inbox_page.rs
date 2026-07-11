use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};

use crate::favorites::{self, FavoriteInbox};
use crate::list_page::{build_list_page_with_search, build_row_menu_button};
use crate::lore::{self, Inbox};
use crate::remote_page::RemoteContent;
use crate::thread_list_page::build_thread_list_page;
use crate::thread_page::launch_uri;

pub const INBOX_LIST_TITLE: &str = "Public Inboxes";

/// The main list's star buttons by slug, for updating a star in place when
/// its inbox is unfavorited from the favorites section. Weak refs: the
/// favorites section's handlers hold this map, and a strong ref here would
/// cycle the buttons alive past the page's destruction.
type StarButtons = Rc<HashMap<String, glib::WeakRef<gtk::Button>>>;

pub fn build_inbox_page(nav: &adw::NavigationView) -> adw::NavigationPage {
    let remote = RemoteContent::new();

    // A reveal-on-demand filter over the (hundreds-long) inbox list, distinct
    // from the window's global lore search: this only narrows the rows already
    // on screen. It stays hidden until the user types or hits Ctrl+F, so the
    // two searches never sit on screen together.
    let entry = gtk::SearchEntry::builder()
        .placeholder_text("Filter inboxes")
        .build();
    let search_bar = gtk::SearchBar::builder().child(&entry).build();
    search_bar.connect_entry(&entry);

    let page = build_list_page_with_search(
        INBOX_LIST_TITLE,
        "Open a public inbox",
        "Every list mirrored on lore.kernel.org. Pick one to open it in this tab.",
        &[],
        remote.widget(),
        Some(&search_bar),
    );
    // Capture typing anywhere on the page to open the bar; scope it to the page
    // so background tabs and the header's global search are unaffected.
    search_bar.set_key_capture_widget(Some(&page));

    load(remote, nav.clone(), entry);
    page
}

fn load(remote: RemoteContent, nav: adw::NavigationView, entry: gtk::SearchEntry) {
    remote.show_loading();
    let cancellable = remote.cancellable();
    glib::spawn_future_local(async move {
        match lore::fetch_inboxes(&cancellable).await {
            Ok(inboxes) => remote.show_content(&build_content(&nav, inboxes, &entry)),
            Err(error) if error.is_cancelled() => {}
            Err(error) => {
                let weak = remote.downgrade();
                let nav = nav.downgrade();
                let entry = entry.downgrade();
                remote.show_error(
                    &error,
                    move || {
                        if let (Some(remote), Some(nav), Some(entry)) =
                            (weak.upgrade(), nav.upgrade(), entry.upgrade())
                        {
                            load(remote, nav, entry);
                        }
                    },
                    None,
                );
            }
        }
    });
}

/// The page content: a favorites section stacked above the full inbox list.
/// The full list is built exactly once — regenerating its hundreds of rows
/// on every star click stalls noticeably — so a toggle only updates star
/// icons in place and regenerates the small favorites section.
fn build_content(nav: &adw::NavigationView, inboxes: Vec<Inbox>, entry: &gtk::SearchEntry) -> gtk::Box {
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
        add_row_actions(&row, &inbox.slug, &inbox.description);
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
    // Narrow the All Inboxes list to rows whose slug or description matches the
    // filter text. Favorites stay pinned and unfiltered: they're your short
    // curated set, and the filter exists to scan the long list below them.
    list.set_filter_func(glib::clone!(
        #[weak]
        entry,
        #[upgrade_or]
        true,
        move |row| row_matches(row, &entry.text())
    ));
    entry.connect_search_changed(glib::clone!(
        #[weak]
        list,
        move |_| list.invalidate_filter()
    ));

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
        add_row_actions(&row, &fav.slug, &fav.description);
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

/// Case-insensitive substring match of an inbox row against the filter query,
/// over both its slug (title) and description (subtitle). An empty query keeps
/// every row.
fn row_matches(row: &gtk::ListBoxRow, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let Some(row) = row.downcast_ref::<adw::ActionRow>() else {
        return true;
    };
    let subtitle = row.subtitle().unwrap_or_default();
    row.title().to_lowercase().contains(&query) || subtitle.to_lowercase().contains(&query)
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

/// Wire an inbox row's secondary-click context menu ("Open in New Tab", "Open
/// on Web") and its middle-click shortcut for opening the inbox in a background
/// tab. "Opening" an inbox means its thread list; the web link is the inbox's
/// lore page.
fn add_row_actions(row: &adw::ActionRow, slug: &str, description: &str) {
    let url = format!("{}/{}/", lore::BASE_URL, slug);
    let slug = slug.to_string();
    let description = description.to_string();

    let menu_button = build_row_menu_button(vec![
        (
            "Open in New _Tab",
            Box::new(glib::clone!(
                #[weak]
                row,
                #[strong]
                slug,
                #[strong]
                description,
                move || open_in_new_tab(&row, &slug, &description)
            )),
        ),
        (
            "Open on _Web",
            Box::new(glib::clone!(
                #[weak]
                row,
                #[strong]
                url,
                move || launch_uri(&row, &url)
            )),
        ),
    ]);
    row.add_suffix(&menu_button);

    // Right-click anywhere on the row opens the same menu.
    let secondary = gtk::GestureClick::new();
    secondary.set_button(gdk::BUTTON_SECONDARY);
    secondary.connect_pressed(glib::clone!(
        #[weak]
        menu_button,
        move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            menu_button.popup();
        }
    ));
    row.add_controller(secondary);

    // Middle-click opens the inbox in a background tab, matching the web
    // convention of middle-clicking a link.
    let middle = gtk::GestureClick::new();
    middle.set_button(gdk::BUTTON_MIDDLE);
    middle.connect_pressed(glib::clone!(
        #[weak]
        row,
        #[strong]
        slug,
        #[strong]
        description,
        move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            open_in_new_tab(&row, &slug, &description);
        }
    ));
    row.add_controller(middle);
}

/// Walk up from an inbox row to the enclosing TabView and open the inbox's
/// thread list in a new background tab there.
fn open_in_new_tab(widget: &impl IsA<gtk::Widget>, slug: &str, description: &str) {
    if let Some(tab_view) = widget
        .ancestor(adw::TabView::static_type())
        .and_downcast::<adw::TabView>()
    {
        crate::open_list_in_new_tab(&tab_view, slug, description);
    }
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
