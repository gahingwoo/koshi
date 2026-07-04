use adw::prelude::*;
use gtk::glib;

use crate::favorites::{self, Favorite};
use crate::list_page::build_list_page;
use crate::thread_page::build_thread_page;

pub const FAVORITES_TITLE: &str = "Favorites";

/// Widget name marking the favorites page, so it can be recognized without
/// comparing titles (a mail subject could legitimately equal the title) and
/// without navigation tags (which must be unique, but the page can appear
/// twice in one stack via favorites → mail → favorites).
pub const FAVORITES_PAGE_NAME: &str = "koshi-favorites-page";

pub fn build_favorites_page(nav: &adw::NavigationView) -> adw::NavigationPage {
    let favorites = favorites::all();

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    if favorites.is_empty() {
        list.append(&build_empty_row());
    } else {
        for fav in &favorites {
            list.append(&build_favorite_row(fav));
        }
        list.connect_row_activated(glib::clone!(
            #[weak]
            nav,
            move |_, _| {
                // Only the bundled sample mail exists for now, so every
                // favorite resolves to it regardless of Message-ID.
                nav.push(&build_thread_page(&nav));
            }
        ));
    }

    let page = build_list_page(
        FAVORITES_TITLE,
        FAVORITES_TITLE,
        "Starred messages",
        &[],
        &list,
    );
    page.set_widget_name(FAVORITES_PAGE_NAME);
    page
}

fn build_empty_row() -> gtk::ListBoxRow {
    let label = gtk::Label::builder()
        .label("No favorites yet")
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .css_classes(["dim-label"])
        .build();
    gtk::ListBoxRow::builder()
        .activatable(false)
        .selectable(false)
        .child(&label)
        .build()
}

fn build_favorite_row(fav: &Favorite) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(format!("<tt>{}</tt>", glib::markup_escape_text(&fav.subject)))
        .title_lines(1)
        .activatable(true)
        .build();
    row.add_suffix(
        &gtk::Label::builder()
            .label(&fav.date)
            .css_classes(["numeric", "caption", "dim-label"])
            .build(),
    );
    row
}
